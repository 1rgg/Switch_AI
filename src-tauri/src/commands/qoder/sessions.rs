//! Qoder 会话明细（issue #82 问题5）：官网无会话消耗明细，查询网关本地流水表
//! `qoder_sessions`（由 `record_usage_qoder` 按请求级写入）。
//!
//! 契约与 Trae `usage_sessions_query` / Buddy `workbuddy_sessions_query` 完全对齐
//! （复用 `UsageSessionView` / `UsageSessionsResult`），前端 SessionsTab 三平台共用：
//! 纯本地零网络、区间按 YYYY-MM-DD 字典序比较、model/user_id 可选过滤、
//! ts 倒序分页、models 清单不受 model 过滤影响。
//! 口径差异：Qoder 侧无官网明细，故 mode/cost_money/缓存 token 恒为空或 0；
//! uid="none" 行 = 轮换耗尽等未落账号的失败请求（诚实展示，不隐藏）。

use std::collections::BTreeSet;

use tauri::State;

use crate::commands::usage_history::{UsageSessionView, UsageSessionsResult};
use crate::state::AppState;

/// 会话明细查询（纯本地流水；区间 [start, end] 按本地日字符串比较）。
#[tauri::command(async)]
pub fn qoder_sessions_query(
    state: State<'_, AppState>,
    start: String,
    end: String,
    model: Option<String>,
    user_id: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
) -> Result<UsageSessionsResult, String> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    // 账号展示名以账号池为准（uid → nickname）；拷入闭包所有权（with_conn 要求 'static 闭包）
    let name_of: std::collections::HashMap<String, String> = super::common::load_pool(&state)
        .into_iter()
        .filter(|a| !a.uid.is_empty())
        .map(|a| (a.uid.clone(), a.nickname.clone()))
        .collect();

    let raw = crate::store::db(&state.data_dir).with_conn(move |c| {
        let mut stmt = c.prepare(
            "SELECT request_id, session_id, uid, date, ts, model, credits, pt, ct, preview
             FROM qoder_sessions WHERE date >= ?1 AND date <= ?2",
        )?;
        let mut rows: Vec<(String, UsageSessionView)> = Vec::new();
        let mut models: BTreeSet<String> = BTreeSet::new();
        let mut it = stmt.query_map([start.as_str(), end.as_str()], |r| {
            let uid: String = r.get(2)?;
            let model: String = r.get(5)?;
            let nickname = name_of.get(&uid).cloned().unwrap_or_default();
            Ok((
                model.clone(),
                (
                    r.get::<_, String>(0)?,
                    UsageSessionView {
                        user_id: uid,
                        account_name: nickname,
                        session_id: r.get(1)?,
                        ts: r.get(4)?,
                        date: r.get(3)?,
                        model,
                        mode: String::new(),
                        credits: r.get(6)?,
                        cost_money: 0.0,
                        input_tokens: r.get::<_, i64>(7)? as u64,
                        output_tokens: r.get::<_, i64>(8)? as u64,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        preview: r.get(9)?,
                    },
                ),
            ))
        })?;
        while let Some(item) = it.next() {
            let (model, pair) = item?;
            // models 清单收录不受 model 过滤影响（筛选下拉用）
            models.insert(model);
            if uid_filter.is_some_and(|f| pair.1.user_id != f) {
                continue;
            }
            if model_filter.is_some_and(|m| pair.1.model != m) {
                continue;
            }
            rows.push(pair);
        }
        Ok((rows, models))
    })?;

    let (mut rows, models) = raw;
    // 新会话在前；request_id 唯一，作最终 tie-breaker 保证翻页次序稳定
    rows.sort_by(|a, b| {
        b.1.ts
            .cmp(&a.1.ts)
            .then_with(|| b.1.session_id.cmp(&a.1.session_id))
            .then_with(|| b.0.cmp(&a.0))
    });
    let total = rows.len() as u64;
    let credits_total = rows.iter().map(|r| r.1.credits).sum();
    let begin = ((page - 1) * page_size) as usize;
    let rows: Vec<UsageSessionView> = rows
        .into_iter()
        .skip(begin)
        .take(page_size as usize)
        .map(|(_, v)| v)
        .collect();

    Ok(UsageSessionsResult {
        // 本地流水为权威态（网关记账即时落库），取当前时刻
        fetched_at: chrono::Local::now().timestamp(),
        total,
        credits_total,
        page,
        page_size,
        rows,
        models: models.into_iter().collect(),
    })
}
