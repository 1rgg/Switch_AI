//! WorkBuddy 每日活跃上报（限时活动）+ 只读诊断。
//!
//! 活动背景（官方 `/docs/zh/workbuddy/Subscription` §限时每日活跃奖励说明）：
//! Buddy AI **国际站个人版**（Free / Pro）当日经 WorkBuddy / CodeBuddy **客户端**
//! 发起至少一次有效对话或任务即视为活跃，**次日自动发放**、无需手动领取：
//! Free 30 积分/天、Pro 50 积分/天，每个活动日最多 1 次。Team 版不参与。
//!
//! ⚠️ 合规风险（务必知悉，开关默认关闭即出于此）：官方「活动限制」明确将
//! 「利用脚本、外挂、插件、模拟器、**接口调用**、批量注册等方式刷取活跃或奖励」
//! 列为可**取消资格、暂停或追回奖励**的行为。本模块以接口调用模拟客户端活跃，
//! 属于该条款指向的行为，使用者自行承担风险。因此：
//!   - 默认关闭（`wb_activity_enabled` 默认 false），必须手动开启；
//!   - 仅覆盖国际版（用户确认：国际版无签到等活动）；
//!   - 每号每天只发 1 次（日活跃按天去重，多发无收益、徒增风控暴露面）。
//!
//! 实现依据：参考实现 linguo2625469/workbuddy2api-panel 的活跃上报——
//! `POST /v2/report`，body 为**事件数组**，事件含 `eventCode: "chat_request_send"`
//! 与**必须的 `userId`**；`conversationId` 由本地生成（无需真实会话）。
//! 注意该参考实现针对**国内版**（copilot.tencent.com / codebuddy.cn）；本模块按其
//! 同构语义打**国际版** `https://www.workbuddy.ai/v2/report`（未经验证，故附带
//! `run_activity_probe` 只读诊断先行核实端点与结构）。
//!
//! 观测（无领取回执）：活跃奖励次日结算，没有任何同步回执可判定成功。因此上报后
//! 回读 `activity/growth/streak` 记一行日志（对齐参考实现的 streak 自检），
//! `days=0` 记 warn——仅观测，不重试（上报按天幂等）。
//!
//! 红线：全程零 token 输出、凭证不入日志/事件（复用 wb_common 统一请求头）。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::wb_common::{self, WbRegion};

/// 活跃上报间隔（账号间串行等待秒数）：对齐参考实现 800ms 量级，防上游频控。
const ACTIVITY_GAP_MS: u64 = 800;

/// 当日去重状态（state KV）：`{"date":"YYYY-MM-DD","uids":[...]}`，
/// 日期为 **UTC+8 活动日**；日期不匹配即视为空（自然跨日重置）。
const ACTIVITY_KV_KEY: &str = "wb_activity_last";

// ── 端点（区域基址拼接；仅国际版会实际使用）──────────────────────────────

/// 活跃上报端点（`POST /v2/report`，body 为事件数组）。
fn report_url(region: WbRegion) -> String {
    format!("{}/v2/report", region.billing_base())
}

/// 连登/活跃天数观测端点（只读 GET；活跃自检 oracle）。
fn streak_url(region: WbRegion) -> String {
    format!("{}/v2/activity/growth/streak", region.billing_base())
}

// 注：曾探测 `/v2/activity/growth/activity`（猜测的活跃明细端点）——
// 2026-10-14 国际版实测返回 `404 page not found`，该端点不存在，已移除探测项。
// 活跃状态无可读端点，只能靠次日积分到账确认。

/// 积分账本观测端点（只读；看 30/50 这类「活跃赠送」是否落账）。
fn ledger_url(region: WbRegion) -> String {
    format!("{}/billing/meter/get-user-resource-summary", region.billing_base())
}

// ── 解析辅助 ─────────────────────────────────────────────────────────────

/// 宽容取数：递归摘首个数值（优先活跃语义键）。
fn dig_num(v: &Value, depth: usize) -> Option<f64> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Object(m) => {
            for k in ["days", "streak", "current", "count", "continuous_days", "value"] {
                if let Some(x) = m.get(k) {
                    if !x.is_null() {
                        if let Some(n) = dig_num(x, depth + 1) {
                            return Some(n);
                        }
                    }
                }
            }
            m.values().find_map(|c| dig_num(c, depth + 1))
        }
        Value::Array(a) => a.iter().find_map(|c| dig_num(c, depth + 1)),
        _ => None,
    }
}

/// 收集 JSON 键路径（**只采集键名不采集值**——脱敏红线）。
fn collect_key_paths(v: &Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 6 || out.len() >= 60 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                out.push(p.clone());
                collect_key_paths(val, &p, depth + 1, out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate().take(3) {
                collect_key_paths(x, &format!("{prefix}[{i}]"), depth + 1, out);
            }
        }
        _ => {}
    }
}

/// 上海时区（活动按 UTC+8 自然日结算）。
///
/// 为什么固定 UTC+8 而非本地时区：活动活动日由**服务端**按 UTC+8 判定，
/// 用本地时区取「今天」在跨时区机器上会切错日子，导致同一天重复发或漏发。
fn today_cst() -> String {
    let cn = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 偏移合法");
    chrono::Utc::now().with_timezone(&cn).format("%Y-%m-%d").to_string()
}

// ── 当日去重（每号每天 1 次）──────────────────────────────────────────────

/// 读取当日已上报账号集合（state kv：`wb_activity_last`，形如
/// `{"date":"2026-09-25","uids":["a","b"]}`）。日期不匹配即视为空。
fn load_reported(state: &AppState) -> (String, Vec<String>) {
    let store = crate::store::db(&state.data_dir);
    let raw: Value = store.kv_get(ACTIVITY_KV_KEY);
    let date = raw.get("date").and_then(Value::as_str).unwrap_or("").to_string();
    let uids: Vec<String> = raw
        .get("uids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if date != today_cst() {
        return (today_cst(), vec![]);
    }
    (date, uids)
}

/// 记录当日已上报账号（读-改-写；与 load_reported 同期）。
fn mark_reported(state: &AppState, date: &str, uids: &[String]) {
    let payload = json!({ "date": date, "uids": uids });
    let store = crate::store::db(&state.data_dir);
    if let Err(e) = store.kv_set(ACTIVITY_KV_KEY, &payload) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("[wb-activity] 当日去重状态写入失败（可能导致重复上报）: {e}"),
        );
    }
}

// ── 活跃上报核心 ─────────────────────────────────────────────────────────

/// 构造 `chat_request_send` 活跃事件（对齐参考实现字段形状；必须含 `userId`）。
///
/// `conversation_id` 由调用方生成（`wb2api-<ms>` 同款），无需真实会话——
/// 判定只要求「通过客户端发起过对话或任务」的行为事件。
///
/// ⚠️ `timestamp` / `reportDelay` **是服务端硬性要求**（实测缺失即 400
/// `10001:event missing both reportDelay and timestamp fields`）——两者至少给一个，
/// 这里都给（与参考实现的桌面指纹注入一致）。
pub fn build_activity_event(uid: &str, conversation_id: &str) -> Value {
    let now_ms = chrono::Utc::now().timestamp_millis();
    json!({
        "eventCode": "chat_request_send",
        "userId": uid,
        "conversationId": conversation_id,
        // 服务端必填（缺失直接 400）：事件时间 + 上报延迟
        "timestamp": now_ms,
        "reportDelay": 2000,
        "inputLength": 24,
        "isPlan": false,
        "codebaseEnable": false,
        "maxSteps": 500,
        "temperature": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "skillId": "",
        "skillCount": 0,
        "totalCount": 0,
        "agentName": "cli",
        "agentType": "main",
        "codebuddy.session_id": conversation_id,
    })
}

/// 单账号上报一次并回读 streak 自检。返回 (kind, message)；
/// kind ∈ ok / fail（auth 由调用方刷新重试）。
///
/// `acct_uid`：账号池 `wb_accounts.uid`。**必须由调用方传入**——实测 token store
/// 记录里没有 `uid` 字段（uid 只存在于账号池），旧实现只读 `creds.uid` 会对全部
/// 账号误判「凭证缺少 uid」而永远不上报。
fn report_once(
    agent: &ureq::Agent,
    creds: &wb_common::Creds,
    region: WbRegion,
    acct_uid: &str,
) -> (String, String) {
    let headers = wb_common::build_auth_headers(creds, false);
    let conversation_id = format!("wb2api-{}", chrono::Utc::now().timestamp_millis());
    // 事件必须含 userId（服务端据此归因）；凭证 uid 优先，缺失回落到账号池 uid
    let uid = if !creds.uid.trim().is_empty() {
        creds.uid.clone()
    } else {
        acct_uid.trim().to_string()
    };
    if uid.is_empty() {
        return ("fail".into(), "账号缺少 uid，无法归因活跃事件".into());
    }
    let body = Value::Array(vec![build_activity_event(&uid, &conversation_id)]);

    let (mut status, report_body, raw) =
        wb_common::post_json_raw(agent, &report_url(region), &headers, &body);
    if status == 0 {
        // 域名双探测（§2.2）：主域名网络不可达 → 同区备用域名重试一次
        let alt = format!("{}/v2/report", region.alt_billing_base());
        if alt != report_url(region) {
            let r = wb_common::post_json_raw(agent, &alt, &headers, &body);
            status = r.0;
            if status != 0 {
                let head: String = r.2.chars().take(120).collect();
                return ("ok".into(), format!("活跃上报成功（备用域名）{head}"));
            }
        }
        let head: String = raw.chars().take(120).collect();
        return ("fail".into(), format!("网络不可达: {head}"));
    }
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into());
    }
    if !(200..=201).contains(&status) {
        let head: String = raw.chars().take(120).collect();
        return ("fail".into(), format!("活跃上报失败（HTTP {status}）: {head}"));
    }

    // 上报响应回显（只取 code，键名/短值——不含凭证）：受理判据以 HTTP 2xx 为准
    let code_txt = report_body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["code", "msg"]))
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());

    // 回读连登天数自检（只读 oracle；上报按天幂等，失败仅观测不重试）。
    //
    // ⚠️ 探测实测（2026-10-14，国际版 1rgg）：签到成功后 `data.streak.days` 仍为 0，
    // 即**签到不计入活跃**、streak 统计的是「连登兑换档位」而非每日活跃。
    // 故本自检只能证明端点可达，**不能证明活跃被记录**；真正验收只能靠次日看积分。
    // 这里如实区分两种 zero，避免把「oracle 不适用」误报成「上游静默丢弃」。
    let (st, sbody, _sraw) = wb_common::get_json(agent, &streak_url(region), &headers);
    let days = if st == 200 {
        sbody.as_ref().and_then(|b| dig_num(b, 0))
    } else {
        None
    };
    match days {
        Some(d) if d > 0.0 => ("ok".into(), format!("活跃上报已受理（响应 code={code_txt}，连登 {d} 天）")),
        // 实测 streak 与每日活跃是两套计数（签到后仍为 0），故此 0 不构成失败证据
        Some(_) => (
            "ok".into(),
            format!("活跃上报已受理（响应 code={code_txt}；streak=0 属正常，需次日看积分确认）"),
        ),
        None => ("ok".into(), format!("活跃上报已受理（响应 code={code_txt}；streak 自检不可读）")),
    }
}

/// 401 刷新一次重试（禁止二次刷新，对齐 wb_checkin 同款 F-09 语义）。
fn report_with_retry(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    creds: &wb_common::Creds,
    region: WbRegion,
    acct_uid: &str,
) -> (String, String) {
    let (kind, msg) = report_once(agent, creds, region, acct_uid);
    if kind != "auth" {
        return (kind, msg);
    }
    let (new, fail_reason) = wb_common::refresh_token_once_locked(state, agent, acct_id, creds);
    match new {
        Some(new) => {
            let (k2, m2) = report_once(agent, &new, region, acct_uid);
            if k2 == "auth" {
                wb_common::mark_needs_relogin(state, acct_id, "活跃上报刷新后仍 401");
                return ("fail".into(), "登录态失效（刷新后仍 401），需重新登录".into());
            }
            (k2, m2)
        }
        None => {
            if fail_reason == Some(wb_common::RefreshFail::Auth) {
                wb_common::mark_needs_relogin(state, acct_id, &format!("活跃上报 refresh 失败: {msg}"));
            }
            ("fail".into(), "登录态失效且刷新失败".into())
        }
    }
}

/// 单账号活跃处理（区域闸门 → 当日去重 → 上报）。
fn process_account_activity(
    state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    already: &[String],
) -> Value {
    let aid = acct.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = acct
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| aid.chars().take(8).collect());

    let (creds, refreshed, note) = wb_common::ensure_fresh(state, agent, acct, 24);
    let region = wb_common::resolve_region_str(&creds.region, &creds.domain);

    // 区域闸门：本功能只覆盖国际版（用户确认：国际版无签到等活动）；
    // 国内账号直接跳过，不发请求、不产生活跃记录
    if !region.is_global() {
        return json!({
            "user_id": aid, "name": name, "status": "skipped",
            "message": "非国际版账号（本功能仅覆盖 workbuddy.ai）",
        });
    }
    if creds.access_token.is_empty() {
        return json!({
            "user_id": aid, "name": name, "status": "fail",
            "message": format!("无可用凭证（{note}）"),
        });
    }
    // 当日去重：日活跃按天计一次，重复上报无额外收益
    if already.iter().any(|u| u == &aid) {
        return json!({
            "user_id": aid, "name": name, "status": "already",
            "message": "今日已上报活跃",
        });
    }
    // 事件 userId 的来源：账号池 uid（实测 token store 里没有 uid 字段）
    let acct_uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
    let (kind, mut message) = report_with_retry(state, agent, &aid, &creds, region, acct_uid);
    let status = if kind == "fail" { "fail" } else { "ok" };
    if refreshed && status == "ok" {
        message = format!("{message}（凭证已续期）");
    }
    json!({ "user_id": aid, "name": name, "status": status, "message": message })
}

/// 活跃上报整轮（NDJSON 事件与签到管线同款契约，可直接复用前端解析）。
///
/// 返回 done 事件（ok/already/skipped/failed 计数），供调度器与托盘路径消费。
/// **每号每天只发 1 次**；已上报的按 already 跳过（跨进程/多次调用均幂等）。
pub fn run_activity_round(state: &AppState, uids: &[String], emit: &mut dyn FnMut(&Value)) -> Value {
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !uids.is_empty() {
        accounts.retain(|a| uids.iter().any(|u| a.get("id").and_then(Value::as_str) == Some(u.as_str())));
    }
    emit(&json!({"type": "start", "total": accounts.len(), "mode": "activity"}));

    let (date, mut reported) = load_reported(state);
    let gap_secs = crate::models::effective_checkin_gap(state.settings().wb_checkin_gap_secs);
    let mut events: Vec<Value> = Vec::new();
    for (i, acct) in accounts.iter().enumerate() {
        // 账号间隔：对齐参考实现 800ms 量级防频控；首个不发等待。
        // 取 max(配置间隔, 800ms)——配置为 0（用户关闭间隔）时仍保底 800ms，
        // 因为活跃上报是密集写请求，裸连发风控暴露面明显更高
        if i > 0 {
            let wait_ms = std::cmp::max(gap_secs.saturating_mul(1000), ACTIVITY_GAP_MS);
            std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        }
        let mut ev = process_account_activity(state, &agent, acct, &reported);
        ev["index"] = json!(i + 1);
        if ev["status"] == "ok" {
            let aid = ev["user_id"].as_str().unwrap_or("").to_string();
            if !aid.is_empty() && !reported.contains(&aid) {
                reported.push(aid);
            }
        }
        emit(&ev);
        events.push(ev);
    }
    // 上报成功即落盘去重状态（成功后才记，失败留待下次重试）
    mark_reported(state, &date, &reported);

    let ok = events.iter().filter(|e| e["status"] == "ok").count();
    let already = events.iter().filter(|e| e["status"] == "already").count();
    let skipped = events.iter().filter(|e| e["status"] == "skipped").count();
    let failed = events.len() - ok - already - skipped;
    let done = json!({
        "type": "done", "mode": "activity",
        "ok": ok, "already": already, "skipped": skipped, "failed": failed,
    });
    emit(&done);
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "[wb-activity] 活跃上报完成：成功 {ok}，今日已报 {already}，非国际版跳过 {skipped}，失败 {failed}"
        ),
    );
    done
}

// ── 只读诊断（上线写操作前核实端点与结构）────────────────────────────────

/// 单账号只读探测结果（全程零写请求）。
fn probe_account(state: &AppState, agent: &ureq::Agent, acct: &Value) -> Value {
    let aid = acct.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let name = acct
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| aid.chars().take(8).collect());

    let (creds, _refreshed, note) = wb_common::ensure_fresh(state, agent, acct, 24);
    let region = wb_common::resolve_region_str(&creds.region, &creds.domain);
    if !region.is_global() {
        return json!({
            "user_id": aid, "name": name, "status": "skipped",
            "message": "非国际版账号（本次诊断仅覆盖 workbuddy.ai）",
        });
    }
    if creds.access_token.is_empty() {
        return json!({
            "user_id": aid, "name": name, "status": "fail",
            "message": format!("无可用凭证（{note}）"),
        });
    }

    let headers = wb_common::build_auth_headers(&creds, false);
    let mut out = json!({
        "user_id": aid, "name": name, "status": "ok",
        "region": region.as_str(), "base": region.billing_base(),
    });

    // ① 连登/活跃天数（唯一可靠的活跃观测 oracle）
    let (st, body, raw) = wb_common::get_json(agent, &streak_url(region), &headers);
    out["streak_http"] = json!(st);
    if st == 200 {
        if let Some(b) = body.as_ref() {
            out["streak_days"] = json!(dig_num(b, 0));
            let mut paths = Vec::new();
            collect_key_paths(b, "", 0, &mut paths);
            out["streak_keys"] = json!(paths);
        }
    } else {
        out["streak_error"] = json!(raw.chars().take(120).collect::<String>());
    }

    // ② 积分账本（看活跃赠送是否落账；国际版实测 200 但取数结构不同）
    let (st3, body3) = wb_common::post_json(agent, &ledger_url(region), &headers, &json!({}));
    out["ledger_http"] = json!(st3);
    if st3 == 200 {
        if let Some(b) = body3.as_ref() {
            out["ledger_balance"] = json!(dig_num(b, 0));
        }
    }
    out
}

/// 只读诊断入口（CLI `--task-run wb-activity-probe`）。
/// **全程零写请求**——不调 `/v2/report`、不签到、不领奖。
pub fn run_activity_probe(state: &AppState) -> Value {
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::wb_pool_load(&crate::store::db(&state.data_dir));
    let accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let items: Vec<Value> = accounts.iter().map(|a| probe_account(state, &agent, a)).collect();
    let probed = items.iter().filter(|i| i["status"] == "ok").count();
    let skipped = items.iter().filter(|i| i["status"] == "skipped").count();
    let failed = items.len() - probed - skipped;

    fs_utils::app_log(
        &state.data_dir,
        &format!("[wb-activity] 只读诊断完成：探测 {probed}，跳过 {skipped}，失败 {failed}（未产生任何活跃上报）"),
    );
    json!({
        "mode": "wb-activity-probe", "readonly": true,
        "note": "只读诊断：未调用 /v2/report，未产生活跃记录，未领取任何奖励",
        "finished_at": fs_utils::now_ts(),
        "probed": probed, "skipped": skipped, "failed": failed,
        "accounts": items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_region_aware() {
        assert!(streak_url(WbRegion::Global).starts_with("https://www.workbuddy.ai/"));
        assert!(ledger_url(WbRegion::Global).starts_with("https://www.workbuddy.ai/"));
        assert_eq!(report_url(WbRegion::Global), "https://www.workbuddy.ai/v2/report");
        // 国内版基址不得混入国际站
        assert!(streak_url(WbRegion::Cn).starts_with("https://www.codebuddy.cn/"));
        assert_ne!(streak_url(WbRegion::Cn), streak_url(WbRegion::Global));
    }

    #[test]
    fn activity_event_carries_required_user_id() {
        // 参考实现的硬判据：事件必须含 userId，否则服务端无法归因
        let ev = build_activity_event("u-123", "wb2api-999");
        assert_eq!(ev["userId"], "u-123");
        assert_eq!(ev["eventCode"], "chat_request_send");
        assert_eq!(ev["conversationId"], "wb2api-999");
        // 必须是对象（上报时包成数组）
        assert!(ev.is_object());
    }

    /// 回归（实测 2026-10-14）：缺 `timestamp`/`reportDelay` 时服务端直接 400
    /// `10001:event missing both reportDelay and timestamp fields`，上报永远失败。
    #[test]
    fn activity_event_includes_timestamp_and_report_delay() {
        let ev = build_activity_event("u-1", "c-1");
        assert!(ev["timestamp"].as_i64().is_some_and(|t| t > 0), "timestamp 必填");
        assert!(ev["reportDelay"].as_i64().is_some(), "reportDelay 必填");
    }

    #[test]
    fn dig_num_handles_nested_shapes() {
        assert_eq!(dig_num(&json!({"streak": {"days": 3}}), 0), Some(3.0));
        assert_eq!(dig_num(&json!({"days": "5"}), 0), Some(5.0));
        assert_eq!(dig_num(&json!({"current": 7}), 0), Some(7.0));
        assert_eq!(dig_num(&json!({"unrelated": "abc"}), 0), None);
    }

    #[test]
    fn collect_key_paths_never_emits_values() {
        // 脱敏红线：只输出键名，值绝不出现在结果里
        let mut paths = Vec::new();
        collect_key_paths(&json!({"streak": {"days": 3}, "token": "secret-value"}), "", 0, &mut paths);
        let joined = paths.join(",");
        assert!(joined.contains("streak.days"));
        assert!(!joined.contains("secret-value"));
        assert!(!joined.contains('3'), "值不得出现在键路径中: {joined}");
    }

    /// 活动日按 UTC+8 结算：日期串必须是 UTC+8 的 YYYY-MM-DD，
    /// 且在跨时区机器上仍与 UTC+8 一致（本地时区取「今天」会切错日子）。
    #[test]
    fn activity_day_uses_cst_not_local_timezone() {
        let today = today_cst();
        assert_eq!(today.len(), 10, "应为 YYYY-MM-DD: {today}");
        let cn = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(today, chrono::Utc::now().with_timezone(&cn).format("%Y-%m-%d").to_string());
    }
}
