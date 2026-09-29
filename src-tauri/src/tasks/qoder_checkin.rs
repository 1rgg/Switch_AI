//! Qoder 签到引擎（F-80 M1 核心，对照 wb_checkin.rs 骨架裁剪）。
//!
//! 端点（sash 活动体系，§2.2/§2.3）：
//! - `GET {open_api}/sash/api/v1/me/campaigns`：活动列表，双活动自然全覆盖
//! - `POST {open_api}/sash/api/v1/me/campaigns/{campaignId}/claim`：**幂等**领取
//!   （重复调用返回 `data.status=="CLAIMED"` + `data.replayed==true`，无副作用）
//!
//! NDJSON 事件契约（`qoder-checkin-progress` 管线，前端逐行 JSON.parse，
//! 与 Buddy 签到前端组件同构）：start {type,total} / account {user_id,name,
//! status,message[,reward],index} / done {type:"done",ok,already,failed}。
//!
//! 调度设计结论（§2.2）：每日 10:15 单次调度同时覆盖「0 点签到」与「10:00 登录奖励」
//! 双活动；失败进入 30 分钟重试冷却（scheduler RETRY_COOLDOWN_MS 同款）。
//!
//! 风控合规内建（§5.2）：claim 间隔 1~3s 抖动；绝不重试轰炸；设备指纹经
//! effective_creds 注入（§5.10 每账号稳定绑定，真实捕获优先）。
//! 红线：全程零 token 输出（凭证不入日志/事件）。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;
use super::qoder_common;

/// 签到轮次参数（对齐 wb_checkin::CheckinOpts；skip_checked 保留契约字段）
#[derive(Clone, Default)]
pub struct QoderCheckinOpts {
    pub uids: Vec<String>,
    #[allow(dead_code)]
    pub skip_checked: bool,
    pub lazy_hours: i64,
}

impl QoderCheckinOpts {
    /// 每日计划任务默认（skip_checked，lazy 24h）
    pub fn daily() -> Self {
        Self {
            uids: vec![],
            skip_checked: true,
            lazy_hours: 24,
        }
    }
}

/// 签到/成长轮次全局锁（对照 WB_ROUND_LOCK；应用内调度器与 UI 路径互斥）。
/// tokio Mutex try_lock 拿不到即拒绝，不排队不阻塞。
static QODER_ROUND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 入口尝试获取轮次锁；guard 移交工作线程并持有至轮次结束（RAII 防泄漏）。
pub(crate) fn try_acquire_qoder_round() -> Result<tokio::sync::MutexGuard<'static, ()>, String> {
    QODER_ROUND_LOCK
        .try_lock()
        .map_err(|_| "已有 Qoder 签到任务在执行中，请等待当前轮次完成".to_string())
}

// ── 端点表（R-4/R-7 抓包固化；常量集中可改）────────────────────────────────

struct QoderUrls {
    campaigns: String,
}

fn urls_for() -> QoderUrls {
    let b = qoder_common::OPEN_API_BASE;
    QoderUrls {
        campaigns: format!("{b}/sash/api/v1/me/campaigns"),
    }
}

// ── 解析辅助（对齐 wb_checkin 同名模式）─────────────────────────────────────

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_or_none(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// claim 间隔抖动（1~3s，时间源派生；ureq 同步请求下线程 sleep）
fn jitter_sleep() {
    let now = chrono::Utc::now().timestamp_millis();
    let ms = 1000 + (now % 2000).unsigned_abs();
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// POST 空 body（R-4 抓包实测：claim 请求 Content-Length: 0，非 JSON `{}`）。
/// 返回 (http_status, parsed)；status=0 网络不可达。
fn post_empty(agent: &ureq::Agent, url: &str, headers: &[(String, String)]) -> (u16, Option<Value>) {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let raw = resp.into_string().unwrap_or_default();
            (200, serde_json::from_str(&raw).ok())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            (code, serde_json::from_str(&raw).ok())
        }
        Err(_e) => (0, None),
    }
}

// ── 结果存储 ───────────────────────────────────────────────────────────────

/// 签到结果 90 天滚动存储（趋势/日志数据源；SQLite 化：qoder_checkin_results 表）。
/// 写入为逐条 UPSERT（pk = date|user_id|time，内容派生）：原「整表 load→save」
/// 在计划任务与 UI 并发触发时互相覆盖丢记录（数组下标 pk 冲突）；90 天裁剪内置于
/// docs::qoder_checkin_results_upsert（逐 pk DELETE，不触碰新写入）。
fn append_results(state: &AppState, events: &[Value]) {
    let store = crate::store::db(&state.data_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for ev in events {
        let mut rec = json!({
            "date": today,
            "time": fs_utils::now_ts(),
            // pk 去重源：毫秒时间戳（now_ts 秒级，同账号同秒两进程并发写入会碰撞互覆）
            "time_ms": chrono::Utc::now().timestamp_millis(),
            "user_id": ev.get("user_id").cloned().unwrap_or_default(),
            "name": ev.get("name").cloned().unwrap_or_default(),
            "status": ev.get("status").cloned().unwrap_or_default(),
            "message": ev.get("message").cloned().unwrap_or_default(),
        });
        if let Some(r) = ev.get("reward").filter(|r| !r.is_null()) {
            rec["reward"] = r.clone();
        }
        if let Err(e) = crate::store::docs::qoder_checkin_results_upsert(&store, &rec) {
            fs_utils::app_log(&state.data_dir, &format!("[qoder] 签到结果落库失败: {e}"));
        }
    }
}

// ── 签到主流程 ─────────────────────────────────────────────────────────────

/// 拉取活动列表并过滤可领项。返回 Ok(可领 campaign 列表)；
/// Err(kind, message)：auth（401，调用方刷新重试）| fail。
fn list_claimable(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
) -> Result<Vec<Value>, (String, String)> {
    let (status, body, raw) = qoder_common::get_json(agent, &urls.campaigns, headers);
    if status == 401 {
        return Err(("auth".into(), "登录态失效（401）".into()));
    }
    if status == 0 {
        let head: String = raw.chars().take(120).collect();
        return Err(("fail".into(), format!("网络不可达: {head}")));
    }
    if !(200..=201).contains(&status) {
        return Err(("fail".into(), format!("campaigns 不可用（HTTP {status}）")));
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        return Err(("fail".into(), "campaigns 响应非 JSON".into()));
    };
    // 信封穿透（fs_utils::dig 含 data 包裹下钻）；⚠ campaigns 为空 ≠ 已签：
    // 可能活动未开始 / Cosy-ClientType 缺失（本层恒带）/ 接口结构变更——
    // 与「无可领项」显式区分，kind=fail 触发 UI 提示（§5.2）
    let campaigns = fs_utils::dig(&b, &["campaigns"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if campaigns.is_empty() {
        return Err(("fail".into(), "empty_campaigns：活动列表为空（活动未开始或不可用）".into()));
    }
    // 过滤 claimStatus==CLAIMABLE（大小写宽容），双活动自然全覆盖
    let claimable: Vec<Value> = campaigns
        .into_iter()
        .filter(|c| {
            let st = s_of(fs_utils::dig(c, &["claimStatus", "claim_status"])).to_ascii_uppercase();
            st == "CLAIMABLE"
        })
        .collect();
    Ok(claimable)
}

/// 复查判定（纯函数，可单测）：campaigns 列表中是否存在 campaignId 匹配且
/// claimStatus 已变 CLAIMED 的条目（字段语义与 list_claimable 过滤同构）。
fn campaign_is_claimed(campaigns: &[Value], campaign_id: &str) -> bool {
    campaigns.iter().any(|c| {
        let cid = s_of(fs_utils::dig(c, &["campaignId", "campaign_id"]));
        cid == campaign_id
            && s_of(fs_utils::dig(c, &["claimStatus", "claim_status"]))
                .to_ascii_uppercase()
                == "CLAIMED"
    })
}

/// claim 网络异常后复查恢复（M1 任务，recon §3.2 cpa-multi-plugins 方案）：
/// claim 请求可能已到达服务端但响应丢失（超时/断连），重新 GET campaigns 检查
/// 该活动是否已变 CLAIMED——GET 幂等安全，绝不重发 claim（避免重复领取副作用）。
fn recheck_claimed(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
    campaign_id: &str,
) -> bool {
    let (status, body, _) = qoder_common::get_json(agent, &urls.campaigns, headers);
    if !(200..=201).contains(&status) {
        return false;
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        return false;
    };
    let campaigns = fs_utils::dig(&b, &["campaigns"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    campaign_is_claimed(&campaigns, campaign_id)
}

/// 单个 campaign 领取。返回 (kind, message, reward)：success / already / fail。
/// 幂等：重复调用 replayed=true 归类 already（非错误）。
fn claim_one(
    agent: &ureq::Agent,
    headers: &[(String, String)],
    urls: &QoderUrls,
    campaign: &Value,
) -> (String, String, Option<f64>) {
    let campaign_id = s_of(fs_utils::dig(campaign, &["campaignId", "campaign_id"]));
    if campaign_id.is_empty() {
        return ("fail".into(), "campaign 缺少 campaignId".into(), None);
    }
    // 奖励数额以接口返回为准（campaigns.benefit.amount 优先；claim 响应 benefit.amount 兜底——
    // R-9 抓包实测：claim 成功响应顶层含完整 benefit{kind,amount,validity}）。
    // 显式路径取值（dig 为候选键语义，不按路径下钻）
    let known_reward = campaign
        .get("benefit")
        .and_then(|x| x.get("amount"))
        .and_then(|v| num_or_none(Some(v)));
    let url = format!(
        "{}/sash/api/v1/me/campaigns/{}/claim",
        qoder_common::OPEN_API_BASE,
        campaign_id
    );
    jitter_sleep();
    let (status, body) = post_empty(agent, &url, headers);
    if status == 401 {
        return ("auth".into(), "登录态失效（401）".into(), None);
    }
    if status == 0 {
        // 网络不可达 ≠ 必然失败：claim 可能已到达服务端但响应丢失，
        // 复查 campaigns（GET 幂等安全），已变 CLAIMED 视为已领（M1 复查恢复）
        if recheck_claimed(agent, headers, urls, &campaign_id) {
            return (
                "already".into(),
                "claim 网络异常，复查确认已领取".into(),
                known_reward,
            );
        }
        return ("fail".into(), "网络不可达（claim）".into(), None);
    }
    let replayed = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["replayed", "data.replayed"]))
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);
    let claimed_status = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["status", "data.status"]))
        .map(|v| s_of(Some(v)).to_ascii_uppercase());
    if (200..=201).contains(&status) {
        // claim 响应顶层 benefit.amount（R-9 抓包实测含完整 benefit{kind,amount,validity}）
        let body_reward = body
            .as_ref()
            .and_then(|b| b.get("benefit"))
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        let reward = known_reward.or(body_reward);
        if replayed {
            // 幂等回放：无副作用，归类 already（非错误）
            return ("already".into(), "今日已领取（幂等回放）".into(), reward);
        }
        if claimed_status.as_deref() == Some("CLAIMED") {
            return ("success".into(), "领取成功".into(), reward);
        }
        // 200 但无明确状态：宽容视为成功（响应结构 R-9 已固化，仍保留兜底）
        return ("success".into(), "领取成功".into(), reward);
    }
    let msg = body
        .as_ref()
        .and_then(|b| fs_utils::dig(b, &["message", "msg"]))
        .map(|v| s_of(Some(v)))
        .filter(|m| !m.is_empty());
    (
        "fail".into(),
        msg.unwrap_or_else(|| format!("claim 失败（HTTP {status}）")),
        None,
    )
}

/// 处理单账号签到（含 401 刷新一次重试，禁二次刷新）。返回 account 事件（不含 index）。
fn process_account(state: &AppState, agent: &ureq::Agent, acct: &Value, opts: &QoderCheckinOpts) -> Value {
    let aid = s_of(acct.get("id"));
    let name = acct
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            let uid = s_of(acct.get("uid"));
            if uid.is_empty() { aid.clone() } else { uid.chars().take(12).collect() }
        });
    let base_ev = json!({"user_id": aid, "name": name});
    let (creds, refreshed, note) = qoder_common::ensure_fresh(state, agent, &aid, opts.lazy_hours);
    if creds.access_token.is_empty() {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "message": format!("无可用凭证（{note}）") });
    }
    if note == "pat_rejected" {
        return json!({ "user_id": aid, "name": base_ev["name"], "status": "fail",
                       "message": "PAT 校验失败（无效或已吊销）：请到 qoder.com.cn/account/integrations 重新创建并导入" });
    }
    if refreshed {
        qoder_common::sync_pool_expiry(state, &aid, &creds);
    }
    let urls = urls_for();
    let mut headers = qoder_common::build_auth_headers(&creds);
    // 签到前余额（差值兜底数据源；查询失败不阻塞签到。共享 qoder_credits 解析，
    // 端点 R-7 固化为 GET /sash/api/v2/me/usage）
    let pre_balance = super::qoder_credits::fetch_usage_balance(agent, &headers);

    let (mut kind, mut message, mut reward) = match list_claimable(agent, &headers, &urls) {
        Ok(claimable) => {
            if claimable.is_empty() {
                // 全部 CLAIMED：视为已签（幂等语义，非错误）
                ("already".into(), "无可领活动（均已领取）".into(), None)
            } else {
                let mut kind = String::new();
                let mut auth_msg = String::new();
                let mut messages: Vec<String> = Vec::new();
                let mut reward = None;
                for c in &claimable {
                    let (k, m, r) = claim_one(agent, &headers, &urls, c);
                    if k == "auth" {
                        kind = "auth".into();
                        auth_msg = m;
                        // 已领取活动的累计奖励不丢弃（真实入账，原 reward=None 会抹掉）
                        break;
                    }
                    if k == "success" {
                        // kind 优先级：success 覆写任何中间态（部分活动失败不掩盖整体成功）；
                        // 其余 kind 仅在首个出现时定型（kind.is_empty() 门控），不互相覆盖
                        kind = "success".into();
                    } else if kind.is_empty() {
                        kind = k.clone();
                    }
                    if !m.is_empty() {
                        messages.push(m);
                    }
                    // 同轮多活动奖励累加（原实现覆盖取最后一个，真实少记）
                    if let Some(r) = r {
                        reward = Some(reward.unwrap_or(0.0) + r);
                    }
                }
                if kind == "auth" {
                    (kind, auth_msg, reward)
                } else {
                    if kind.is_empty() {
                        kind = "fail".into();
                    }
                    let message = messages.join("；");
                    (kind, message, reward)
                }
            }
        }
        Err((k, m)) => (k, m, None),
    };

    // 401：刷新一次仅重试失败分支（禁二次刷新，对齐 wb_checkin F-09）。
    // 走 ensure_fresh（lazy=MAX 恒刷新，内部成功已落库）：PAT 账号经
    // exchange_job_token 用原始 PAT 重换作业令牌，客户端账号走 refresh_token_once；
    // 此前直接调后者，PAT 换发的作业令牌对其不兼容（qoder_common 明令规避），
    // 自愈必失败且误报「需重新登录」。令牌未变不重试（对齐 credits 401 自愈）
    if kind == "auth" {
        let (new, refreshed, _) = qoder_common::ensure_fresh(state, agent, &aid, i64::MAX);
        let retry_cred =
            if refreshed && new.access_token != creds.access_token { Some(new) } else { None };
        match retry_cred {
            Some(new) => {
                qoder_common::sync_pool_expiry(state, &aid, &new);
                headers = qoder_common::build_auth_headers(&new);
                let retry = match list_claimable(agent, &headers, &urls) {
                    Ok(claimable) => {
                        if claimable.is_empty() {
                            ("already".into(), "无可领活动（均已领取）".into(), None)
                        } else {
                            // kind 空串起步（对照首次路径）：全部 claim 失败时应判 fail
                            // 而非误标 already（成功才覆写 success，否则取首个非成功 kind）
                            let mut kind = String::new();
                            let mut messages: Vec<String> = Vec::new();
                            let mut reward = None;
                            for c in &claimable {
                                let (k, m, r) = claim_one(agent, &headers, &urls, c);
                                if k == "success" {
                                    kind = "success".into();
                                } else if kind.is_empty() {
                                    kind = k.clone();
                                }
                                if !m.is_empty() {
                                    messages.push(m);
                                }
                                // 同轮多活动奖励累加（与首次路径同语义）
                                if let Some(r) = r {
                                    reward = Some(reward.unwrap_or(0.0) + r);
                                }
                            }
                            if kind.is_empty() {
                                kind = "fail".into();
                            }
                            (kind, messages.join("；"), reward)
                        }
                    }
                    Err((k, m)) => (k, m, None),
                };
                kind = retry.0;
                message = retry.1;
                // 重试奖励合并进首次已累计部分（auth 中断前可能已领到部分活动）：
                // 原实现整体覆盖，401 前已入账的奖励被抹掉
                reward = match (reward, retry.2) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => b.or(a),
                };
            }
            None => {
                kind = "fail".into();
                message = "登录态失效且刷新失败，需重新登录".into();
                // 已累计奖励保留（真实入账不因刷新失败而回滚；原 reward=None 丢弃）
            }
        }
    }
    // 奖励差值兜底（对齐 F-17 模式）：接口未返回数额时用签到前后余额差值，仅 >0 采信
    if kind == "success" && reward.is_none() {
        if let Some(pre) = pre_balance {
            if let Some(post) = super::qoder_credits::fetch_usage_balance(agent, &headers) {
                if post > pre {
                    reward = Some(post - pre);
                }
            }
        }
    }
    let status_txt = match kind.as_str() {
        "success" => "success",
        "already" => "already",
        _ => "fail",
    };
    // 空活动列表诊断（脱敏：只记账号与结论，不含 token/响应原文）
    if message.starts_with("empty_campaigns") {
        fs_utils::app_log(
            &state.data_dir,
            &format!("qoder 签到空活动列表: {aid}（Cosy-ClientType={} 已带；若持续出现请核查活动状态/接口结构 R-9）", qoder_common::COSY_CLIENT_TYPE),
        );
    }
    let mut ev = json!({ "user_id": aid, "name": base_ev["name"], "status": status_txt, "message": message });
    if let Some(r) = reward {
        ev["reward"] = json!(r);
    }
    if kind == "success" && note == "refreshed" {
        ev["message"] = json!(format!("{}（凭证已续期）", ev["message"].as_str().unwrap_or("")));
    }
    ev
}

/// 签到整轮：逐账号串行处理，事件经 emit 回调逐条输出（NDJSON 管线复用）。
/// 返回 done 事件（ok/already/failed 计数），供启动补签/调度静默路径直接消费。
pub fn run_checkin_round(state: &AppState, opts: &QoderCheckinOpts, emit: &mut dyn FnMut(&Value)) -> Value {
    // 设备指纹惰性回填（§5.10：GUI/CLI/启动补签三路共用本漏斗，一处 ensure 全覆盖；
    // 失败不阻塞签到，仅缺注入指纹）
    if let Err(e) = super::qoder_device::ensure_pool_profiles(state) {
        fs_utils::app_log(&state.data_dir, &format!("Qoder 设备指纹回填失败（继续签到）: {e}"));
    }
    let agent = http_agent(30);
    let pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !opts.uids.is_empty() {
        accounts.retain(|a| opts.uids.iter().any(|u| s_of(a.get("id")) == *u));
    }
    emit(&json!({"type": "start", "total": accounts.len()}));

    let mut events: Vec<Value> = Vec::new();
    for (i, acct) in accounts.iter().enumerate() {
        // 单账号失败不中断整轮（对齐 wb try/except 语义）
        let mut ev = process_account(state, &agent, acct, opts);
        ev["index"] = json!(i + 1);
        emit(&ev);
        events.push(ev);
    }

    let ok = events.iter().filter(|e| e["status"] == "success").count();
    let already = events.iter().filter(|e| e["status"] == "already").count();
    let failed = events.len() - ok - already;
    let done = json!({"type": "done", "ok": ok, "already": already, "failed": failed});
    emit(&done);
    append_results(state, &events);
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daily_opts_defaults() {
        let o = QoderCheckinOpts::daily();
        assert!(o.skip_checked);
        assert_eq!(o.lazy_hours, 24);
    }

    /// R-9 抓包样本：claim 成功响应顶层含 grantId/status/replayed + 完整 benefit
    #[test]
    fn claim_reward_from_campaigns_entry_or_claim_body() {
        // campaigns 条目 benefit.amount 优先
        let c = json!({"campaignId": "c1", "benefit": {"amount": 100}});
        let known = c
            .get("benefit")
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        assert_eq!(known, Some(100.0));
        // claim 响应顶层 benefit.amount 兜底（campaigns 条目缺 benefit 时）
        let claim_body = json!({"grantId": "g", "status": "CLAIMED", "replayed": false,
                                "benefit": {"kind": "CREDITS", "amount": 100}});
        let body_reward = claim_body
            .get("benefit")
            .and_then(|x| x.get("amount"))
            .and_then(|v| num_or_none(Some(v)));
        assert_eq!(body_reward, Some(100.0));
        assert_eq!(known.or(body_reward), Some(100.0));
    }

    #[test]
    fn claimable_filter_is_case_insensitive() {
        // list_claimable 的过滤口径：CLAIMABLE（大小写宽容）命中
        let c = json!({"campaignId": "c1", "claimStatus": "claimable", "benefit": {"amount": 100}});
        let st = s_of(fs_utils::dig(&c, &["claimStatus"])).to_ascii_uppercase();
        assert_eq!(st, "CLAIMABLE");
        let claimed = json!({"campaignId": "c2", "claimStatus": "CLAIMED"});
        let st2 = s_of(fs_utils::dig(&claimed, &["claimStatus"])).to_ascii_uppercase();
        assert_ne!(st2, "CLAIMABLE");
    }

    /// M1 claim 失败复查恢复：网络异常后按 campaignId 复查 claimStatus==CLAIMED
    #[test]
    fn campaign_is_claimed_recheck() {
        let claimed = json!({"campaignId": "c1", "claimStatus": "CLAIMED"});
        let claimable = json!({"campaignId": "c1", "claimStatus": "CLAIMABLE"});
        let other = json!({"campaignId": "c2", "claimStatus": "CLAIMED"});
        // id 匹配 + 已 CLAIMED → true
        assert!(campaign_is_claimed(&[claimed.clone()], "c1"));
        // 同 id 仍 CLAIMABLE → false（复查不通过，保持 fail）
        assert!(!campaign_is_claimed(&[claimable], "c1"));
        // id 不匹配 → false
        assert!(!campaign_is_claimed(&[other], "c1"));
        // 空列表 → false
        assert!(!campaign_is_claimed(&[], "c1"));
        // 大小写宽容 + snake_case 候选键（与 list_claimable 口径同构）
        let lower = json!({"campaign_id": "c1", "claim_status": "claimed"});
        assert!(campaign_is_claimed(&[lower], "c1"));
    }

    #[test]
    fn urls_cover_sash_endpoints() {
        let u = urls_for();
        assert!(u.campaigns.starts_with(qoder_common::OPEN_API_BASE));
        assert!(u.campaigns.contains("/sash/api/v1/me/campaigns"));
    }
}
