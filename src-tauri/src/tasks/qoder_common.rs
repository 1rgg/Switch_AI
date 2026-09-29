//! Qoder 公共请求层（F-80 M1，对照 wb_common.rs 模式裁剪）。
//!
//! 端点与鉴权情报基线（docs/tmp/f80-qoder-support-design.md §2.3，M0 定稿前均为待验证）：
//! - OpenAPI 域：`openapi.qoder.com.cn`（sash 活动 / quota 用量 / userinfo / deviceToken）
//! - `Cosy-ClientType` 必带（缺失时 sash 返回 `campaigns: []` 静默空列表——最隐蔽的坑）；
//!   CN 客户端真实取值 R-4 待抓包确认，缺省 "10"（社区逆向脚本取值）
//! - token 三前缀：`pt-`（PAT）/ `jt-`（job token）/ `dt-`（device token）
//! - 设备头 `Cosy-MachineId` / `Cosy-MachineToken`：真实捕获优先透传；缺失时由
//!   `effective_creds` 注入账号绑定指纹（§5.10 多账号并发，v1.2 用户决策——
//!   伪造是正式需求，以每账号稳定绑定控制风险，见 tasks::qoder_device）
//!
//! 凭证双源说明：wb 的双源为 token store × auth 文件；Qoder 客户端本地存储解密
//! （auth.v1.dat，DPAPI+AES-GCM）受 R-8 门控——M1 仅 token store 单源（PAT 导入落库），
//! 客户端存储通道在 R-8 闭合后以新增 `client_store_creds()` 接入本层。
//!
//! 红线：全程零 token 输出（凭证不入日志/事件/UI）。

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fs_utils;
use crate::state::AppState;

/// OpenAPI 基址（CN 域；端点常量集中可改，R-7 主备域探测位预留）
pub const OPEN_API_BASE: &str = "https://openapi.qoder.com.cn";

/// `Cosy-ClientType` 真实值（R-4 已闭合：2026-09-27 抓包实测主进程恒带 `cosy-clienttype: 10`）
pub const COSY_CLIENT_TYPE: &str = "10";

/// 客户端 UA 对齐（R-4 抓包实测：主进程 API 请求 UA 为 "Qoder"；渲染进程为
/// Electron 完整 UA `...QoderCN/0.4.2 Chrome/150... Electron/43.1.1...`）
pub const CLIENT_USER_AGENT: &str = "Qoder";

// ── 凭证结构 ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Default)]
pub struct QoderCreds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 凭证种类：pat（pt-，官方认可）| client（客户端存储/抓包透传）
    #[serde(default)]
    pub kind: String,
    /// PAT 原始凭证（pt-）：access_token 为换取后的 24h 作业令牌时，此字段保存
    /// 原始 PAT 供到期重换（PAT 长期有效，绝不入日志/事件）
    #[serde(default)]
    pub pat: String,
    /// 设备指纹头来源：真实捕获值（client/mitm/cli）原样保存在 token store；
    /// effective_creds 合并时缺失则注入账号绑定 machine_id（§5.10）
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub machine_token: String,
}

// 手写脱敏 Debug（derive(Debug) 会把 access_token/refresh_token/pat/machine_token
// 全量打进日志——触犯「凭证不入日志」红线）：敏感字段仅显 **（空则空串便于排障）
impl std::fmt::Debug for QoderCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mask = |s: &str| if s.is_empty() { "" } else { "**" };
        f.debug_struct("QoderCreds")
            .field("access_token", &mask(&self.access_token))
            .field("refresh_token", &mask(&self.refresh_token))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("uid", &self.uid)
            .field("nickname", &self.nickname)
            .field("kind", &self.kind)
            .field("pat", &mask(&self.pat))
            .field("machine_id", &self.machine_id)
            .field("machine_token", &mask(&self.machine_token))
            .finish()
    }
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn i_of(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
}

/// 从 token store 记录提取凭证（宽容解析；兼容 accessToken/access_token/token 键名）
pub fn creds_of(source: &Value) -> QoderCreds {
    let auth = source.get("auth").filter(|v| v.is_object()).unwrap_or(source);
    let account = source
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(source);
    QoderCreds {
        access_token: s_of(fs_utils::dig(auth, &["accessToken", "access_token", "token", "pat"])),
        refresh_token: s_of(fs_utils::dig(auth, &["refreshToken", "refresh_token"])),
        expires_at_ms: i_of(fs_utils::dig(
            auth,
            &["expiresAtMs", "expires_at_ms", "expiresAt", "expires_at"],
        )),
        uid: s_of(fs_utils::dig(account, &["uid", "userId", "user_id", "id"])),
        nickname: s_of(fs_utils::dig(account, &["nickname", "name", "displayName"])),
        kind: s_of(fs_utils::dig(auth, &["kind", "token_kind", "credential_source"])),
        pat: s_of(fs_utils::dig(source, &["pat"])),
        machine_id: s_of(fs_utils::dig(source, &["machine_id", "machineId", "cosy_machine_id"])),
        machine_token: s_of(fs_utils::dig(source, &["machine_token", "machineToken", "cosy_machine_token"])),
    }
}

// ── token store（qoder_tokens 表；结构 {version, tokens: {id: rec}}）────────

/// token store 读改写互斥：load→merge→save 非原子，签到/积分/刷新/导入多通道
/// 并发写会互相覆盖丢更新（last-writer-wins 抹掉彼此的新 token）。进程内全局锁
/// 串行化表级读改写；仅持锁做本地 IO，不覆盖网络请求路径（无死锁面）。
static TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 敏感字段键名（审查 P0-1 凭证收敛）：rec 中这些字段一律占位（空串）存 DB，
/// 明文进 vault（Stronghold + DPAPI，与 Trae/WB 同一 vault，ns="qoder"）。
/// machine_id 是设备标识符（非凭证），保留在 DB 供排障。
const TOKEN_SENSITIVE_KEYS: [&str; 4] = [
    "access_token",
    "refresh_token",
    "pat",
    "machine_token",
];

/// DB 读取 + vault 回填（仅内存）：敏感字段为占位空串时从 vault 回填。
/// vault 不可用 / 无记录 → 保持空串（上层按 no_credential 处理，fail-secure）。
pub fn token_store_load_secure(data_dir: &std::path::Path) -> Value {
    let mut store = crate::store::docs::qoder_token_store_load(&crate::store::db(data_dir));
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return store;
    };
    for (id, rec) in tokens.iter_mut() {
        let Some(rm) = rec.as_object_mut() else { continue };
        let Some(sec) = crate::vault::ns_get(data_dir, "qoder", id) else { continue };
        for k in TOKEN_SENSITIVE_KEYS {
            let Some(val) = sec.get(k).and_then(Value::as_str) else { continue };
            if val.is_empty() {
                continue;
            }
            // 仅填空值：DB 明文优先（更新鲜，如迁移残留，待下次写入收敛）
            if rm.get(k).and_then(Value::as_str).map_or(true, |s| s.is_empty()) {
                rm.insert(k.to_string(), serde_json::json!(val));
            }
        }
    }
    store
}

/// 整库敏感字段收敛：每个 rec 的非空敏感值字段级合并写入 vault，随后整库占位
///（明文只留 vault）。返回写入 vault 的账号数。
fn secure_store_for_save(
    data_dir: &std::path::Path,
    store: &mut Value,
) -> Result<usize, String> {
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    let ids: Vec<String> = tokens.keys().cloned().collect();
    let mut wrote = 0usize;
    for id in &ids {
        let Some(rec) = tokens.get(id.as_str()) else { continue };
        let mut entry = crate::vault::ns_get(data_dir, "qoder", id)
            .unwrap_or_else(|| serde_json::json!({}));
        let mut dirty = false;
        for k in TOKEN_SENSITIVE_KEYS {
            if let Some(v) = rec.get(k).and_then(Value::as_str) {
                if !v.is_empty() {
                    entry[k] = serde_json::json!(v);
                    dirty = true;
                }
            }
        }
        if dirty {
            crate::vault::ns_set(data_dir, "qoder", id, &entry)?;
            wrote += 1;
        }
    }
    // 整库占位（整表替换写回：所有 rec 的敏感字段一律清空）
    for rec in tokens.values_mut() {
        if let Some(rm) = rec.as_object_mut() {
            for k in TOKEN_SENSITIVE_KEYS {
                if rm.contains_key(k) {
                    rm.insert(k.to_string(), serde_json::json!(""));
                }
            }
        }
    }
    Ok(wrote)
}

/// 启动迁移（P0-1）：存量明文凭证收敛进 vault + DB 占位化（幂等，无明文时零开销）
pub fn migrate_token_store(state: &AppState) -> Result<usize, String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = crate::store::docs::qoder_token_store_load(&crate::store::db(&state.data_dir));
    let has_plain = store
        .get("tokens")
        .and_then(Value::as_object)
        .map(|tokens| {
            tokens.values().any(|rec| {
                TOKEN_SENSITIVE_KEYS.iter().any(|k| {
                    rec.get(k)
                        .and_then(Value::as_str)
                        .map_or(false, |s| !s.is_empty())
                })
            })
        })
        .unwrap_or(false);
    if !has_plain {
        return Ok(0);
    }
    let wrote = secure_store_for_save(&state.data_dir, &mut store)?;
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store)?;
    Ok(wrote)
}

pub fn load_token_store(state: &AppState) -> Value {
    token_store_load_secure(&state.data_dir)
}

/// 生效凭证（M1 单源：token store；客户端存储通道 R-8 闭合后接入双源比较）。
/// 设备指纹在此合并注入（§5.10 唯一出口：checkin/credits 均经此）——
/// 真实捕获优先，缺失时注入账号绑定 machine_id + 现场随机 machine_token。
pub fn effective_creds(state: &AppState, acct_id: &str) -> QoderCreds {
    let mut creds = load_token_store(state)
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    let profile = load_device_profile(state, acct_id);
    super::qoder_device::merge_device_profile(&mut creds, profile.as_ref());
    creds
}

/// 从账号池读取账号绑定指纹（§5.10；缺失返回 None → 不注入）
fn load_device_profile(state: &AppState, acct_id: &str) -> Option<super::qoder_device::QoderDeviceProfile> {
    let pool = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    pool.get("accounts")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(acct_id))
        })
        .and_then(|a| a.get("device_profile").cloned())
        .and_then(|p| serde_json::from_value(p).ok())
}

/// 写工具侧凭证副本（version≠1 拒写；非空字段 merge + updated_at，对齐 wb_common 同语义）。
/// 凭证收敛（P0-1）：敏感字段进 vault、DB 占位；vault 写失败时仍落占位库并返回 Err
///（对齐 Trae 红线：宁可丢本次凭据更新，也不把 token/pat 明文写进 SQLite）。
pub fn save_token_store(state: &AppState, id: &str, creds: &QoderCreds) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_token_store(state);
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().unwrap();
    match obj.get("version") {
        Some(v) if v.as_i64() != Some(1) => {
            return Err("token_store 版本不识别，拒绝写入".to_string());
        }
        _ => {}
    }
    obj.insert("version".into(), serde_json::json!(1));
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(t) = tokens.as_object_mut() {
        let mut rec = t.get(id).cloned().unwrap_or(serde_json::json!({}));
        if let Some(rm) = rec.as_object_mut() {
            let val = serde_json::to_value(creds).map_err(|e| e.to_string())?;
            for (k, v) in val.as_object().into_iter().flatten() {
                // 空串/null 一律不覆盖已有值（局部更新不抹掉设备头/种类等存量字段）
                if !v.is_null() && !(v.is_string() && v.as_str().unwrap_or("").is_empty()) {
                    rm.insert(k.clone(), v.clone());
                }
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        t.insert(id.to_string(), rec);
    }
    let vault_result = secure_store_for_save(&state.data_dir, &mut store).map(|_| ());
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store)?;
    vault_result.map_err(|e| {
        format!("Qoder 凭据加密存储失败（已仅保存占位信息，重新登录可恢复）: {e}")
    })
}

/// 删除 token store 记录（账号移除时同步清理 vault 凭证）
pub fn remove_token(state: &AppState, id: &str) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_token_store(state);
    if let Some(t) = store.get_mut("tokens").and_then(Value::as_object_mut) {
        t.remove(id);
    }
    let r = crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store);
    crate::vault::ns_remove(&state.data_dir, "qoder", id);
    r
}

// ── 统一请求头（§5.2：Cosy 头必带）─────────────────────────────────────────

/// Bearer + Cosy-ClientType（缺失 → 服务端静默空列表）+ 可选设备头透传 + UA。
pub fn build_auth_headers(creds: &QoderCreds) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", creds.access_token),
        ),
        ("Cosy-ClientType".to_string(), COSY_CLIENT_TYPE.to_string()),
        ("User-Agent".to_string(), CLIENT_USER_AGENT.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    // 设备指纹头原样携带（注入发生在 effective_creds 合并层，§5.10）
    if !creds.machine_id.is_empty() {
        h.push(("Cosy-MachineId".to_string(), creds.machine_id.clone()));
    }
    if !creds.machine_token.is_empty() {
        h.push(("Cosy-MachineToken".to_string(), creds.machine_token.clone()));
    }
    h
}

/// POST JSON → (http_status, parsed)；status=0 网络不可达（复用 wb_common 同款实现）
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>) {
    crate::tasks::wb_common::post_json(agent, url, headers, body)
}

pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    crate::tasks::wb_common::get_json(agent, url, headers)
}

// ── token 刷新（deviceToken/refresh；R-10 轮询参数侦察后补 poll 接入）───────

/// 调 deviceToken/refresh（body {"refresh_token":...}）。
/// 成功返回新 Creds（expires 按 expiresIn 归一为毫秒回填——R-6 实测毫秒级 86400000，
/// 秒级值兼容 ×1000；设备头原样透传保留）。
pub fn refresh_token_once(agent: &ureq::Agent, creds: &QoderCreds) -> Option<QoderCreds> {
    if creds.refresh_token.is_empty() {
        return None;
    }
    // 刷新端点不带 Authorization（下方 retain 移除；鉴权完全靠 body 中的 refresh_token）+ Cosy 头
    let mut h = build_auth_headers(creds);
    h.retain(|(k, _)| k != "Authorization");
    let url = format!("{OPEN_API_BASE}/api/v1/deviceToken/refresh");
    let (status, body) = post_json(
        agent,
        &url,
        &h,
        &serde_json::json!({ "refresh_token": creds.refresh_token }),
    );
    if status != 200 {
        return None;
    }
    let body = body?;
    let acc = s_of(fs_utils::dig(&body, &["accessToken", "access_token", "token"]));
    if acc.is_empty() {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut out = creds.clone();
    out.access_token = acc;
    let ref_tok = s_of(fs_utils::dig(&body, &["refreshToken", "refresh_token"]));
    if !ref_tok.is_empty() {
        out.refresh_token = ref_tok;
    }
    if let Some(e) = i_of(fs_utils::dig(&body, &["expiresIn", "expires_in"])) {
        out.expires_at_ms = Some(now_ms + normalize_expires_in(e));
    }
    Some(out)
}

// ── userinfo（PAT 导入时回填 uid/昵称；失败容错不阻塞导入）──────────────────

/// POST /api/v1/userinfo → (uid, nickname)；任何失败返回 (None, None)。
/// （R-13 侦察注：设计文档 L553 写 GET，但实现一直以 POST 在用且未被证伪——
/// 端点真实方法未实测，若后续发现 404/405 再切 get_json。）
pub fn fetch_userinfo(agent: &ureq::Agent, creds: &QoderCreds) -> (Option<String>, Option<String>) {
    let url = format!("{OPEN_API_BASE}/api/v1/userinfo");
    let (status, body) = post_json(agent, &url, &build_auth_headers(creds), &serde_json::json!({}));
    if status != 200 {
        return (None, None);
    }
    let Some(b) = body else { return (None, None) };
    // 信封穿透：{data:{uid,nickname}} 或平铺
    let uid = fs_utils::dig(&b, &["uid", "userId", "user_id", "id"]).and_then(Value::as_str).map(String::from);
    let nickname = fs_utils::dig(&b, &["nickname", "nickName", "name", "displayName"])
        .and_then(Value::as_str)
        .map(String::from);
    (uid, nickname)
}

// ── plan 查询（R-7 抓包固化：GET /api/v2/user/plan → plan_tier_name 等）─────

/// GET /api/v2/user/plan → (plan_tier_name, user_type, end_date_ms)；失败全 None。
/// 实测响应：{"user_type":"personal_professional_trial","plan_tier_name":"Pro Trial",
///           "is_personal_version":true,"is_paid_plan":false,...,"end_date":1791673619906}
pub fn fetch_plan(agent: &ureq::Agent, creds: &QoderCreds) -> (Option<String>, Option<String>, Option<i64>) {
    let url = format!("{OPEN_API_BASE}/api/v2/user/plan");
    let (status, body, _raw) = get_json(agent, &url, &build_auth_headers(creds));
    if status != 200 {
        return (None, None, None);
    }
    let Some(b) = body else { return (None, None, None) };
    let tier = fs_utils::dig(&b, &["plan_tier_name", "planTierName"])
        .and_then(Value::as_str)
        .map(String::from);
    let user_type = fs_utils::dig(&b, &["user_type", "userType"])
        .and_then(Value::as_str)
        .map(String::from);
    let end_date = fs_utils::dig(&b, &["end_date", "endDate"]).and_then(|v| {
        v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
    });
    (tier, user_type, end_date)
}

// ── PAT 作业令牌换取（R-6 闭合：2026-09-27 实测 pt- 直调 sash 端点 401，
//    客户端真实链路为 POST /api/v1/me/jobToken 换 24h 作业令牌后再调业务端点）──

/// 作业令牌 clientId：按 PAT 派生的稳定 UUID 格式串（客户端实测携带其安装级
/// 固定 clientId；服务端未见强校验，按账号稳定派生即可，避免每次随机）
fn job_client_id(pat: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"qoder-job-client:");
    h.update(pat.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// expires_in 归一为毫秒（R-6 抓包实测 86400000 = 24h，即毫秒；秒级值兼容 ×1000）。
/// 阈值 2_592_000 = 30 天的秒数：秒级上限（30d=2.592e6）与毫秒级下限（1h=3.6e6）
/// 之间留出安全间隔，杜绝「秒级超长有效期被误当毫秒」的歧义
pub(crate) fn normalize_expires_in(e: i64) -> i64 {
    if e > 2_592_000 {
        e
    } else {
        e * 1000
    }
}

/// PAT → 作业令牌。多端点/多形态尝试（按优先级）：
/// ① `POST /api/v1/jobToken/exchange` body `{"pat": ...}`（设计文档附录 A，社区逆向）
/// ② `POST /api/v1/jobToken/exchange` 空 body（Bearer 鉴权）
/// ③ `POST /api/v1/me/jobToken` body `{"clientId": ...}`（R-6 抓包：客户端真实路径）
/// 成功返回以作业令牌为 access_token 的 Creds（pat 字段保存原始 PAT 供到期重换）。
/// data_dir 用于状态码落日志（脱敏：只记端点与 HTTP 状态，不含 token）——全部失败时
/// 便于定位是 401（PAT 不被接受）还是 404（端点不存在）。
pub fn exchange_job_token(
    agent: &ureq::Agent,
    pat: &str,
    data_dir: &std::path::Path,
) -> Option<QoderCreds> {
    let attempts: [(&str, Value, &str); 3] = [
        (
            "/api/v1/jobToken/exchange",
            serde_json::json!({ "pat": pat }),
            "exchange+pat",
        ),
        ("/api/v1/jobToken/exchange", serde_json::json!({}), "exchange"),
        (
            "/api/v1/me/jobToken",
            serde_json::json!({ "clientId": job_client_id(pat) }),
            "me/jobToken",
        ),
    ];
    let mut last_status: u16 = 0;
    for (path, body, tag) in attempts {
        let url = format!("{OPEN_API_BASE}{path}");
        let headers = vec![
            ("Authorization".to_string(), format!("Bearer {pat}")),
            ("Cosy-ClientType".to_string(), COSY_CLIENT_TYPE.to_string()),
            ("User-Agent".to_string(), CLIENT_USER_AGENT.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let (status, resp) = post_json(agent, &url, &headers, &body);
        last_status = status;
        if status == 200 {
            if let Some(b) = resp {
                let token = s_of(fs_utils::dig(&b, &["token", "accessToken", "access_token", "job_token"]));
                if !token.is_empty() {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let expires_at_ms = i_of(fs_utils::dig(&b, &["expires_in", "expiresIn"]))
                        .map(normalize_expires_in)
                        .map(|e| now_ms + e);
                    let refresh_token = s_of(fs_utils::dig(&b, &["refresh_token", "refreshToken"]));
                    fs_utils::app_log(
                        data_dir,
                        &format!("qoder PAT→作业令牌换取成功（通道 {tag}）"),
                    );
                    return Some(QoderCreds {
                        access_token: token,
                        refresh_token,
                        expires_at_ms,
                        kind: "pat".into(),
                        pat: pat.to_string(),
                        ..Default::default()
                    });
                }
            }
            // 200 但无令牌字段：视为该形态不匹配，继续下一通道
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} 返回 200 但缺少令牌字段"));
        } else if status != 0 {
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} → HTTP {status}"));
        } else {
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} 网络不可达"));
        }
    }
    fs_utils::app_log(
        data_dir,
        &format!("qoder PAT→作业令牌全部通道失败（最后 HTTP {last_status}）：PAT 可能无效/已吊销，或换取端点对 PAT 另有要求"),
    );
    None
}

// ── 惰性刷新（对齐 wb_common::ensure_fresh 语义）────────────────────────────

/// 惰性刷新：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
/// 返回 (creds, refreshed, note)，note ∈ no_credential/fresh/expired_needs_relogin/
/// refreshed/refresh_failed/pat_rejected。
///
/// PAT 通道（R-6）：pt- 不被 sash 业务端点接受（实测 401），先经 jobToken 换取
/// 24h 作业令牌；作业令牌临期（< lazy_hours，与客户端通道同语义）或已过期时用
/// 原始 PAT 重换（PAT 长期有效）；调用方传 i64::MAX（401 自愈/恒刷路径）即无条件重换。
pub fn ensure_fresh(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    lazy_hours: i64,
) -> (QoderCreds, bool, &'static str) {
    let creds = effective_creds(state, acct_id);
    if creds.access_token.is_empty() {
        return (creds, false, "no_credential");
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let is_pat = creds.access_token.starts_with("pt-");
    let has_pat = !creds.pat.is_empty();
    // PAT 通道覆盖全部携带 pat 备份的凭证（含作业令牌）：其生命周期由原始 PAT
    // 重换管理（可靠自愈路径），不得掉入客户端通道赌 deviceToken/refresh 对
    // 作业令牌 refresh_token 的兼容性（旧门控 e<=now 使临期窗口误入该通道报 refresh_failed）。
    if is_pat || has_pat {
        // ── PAT 通道：确保作业令牌有效 ──
        let pat = if creds.pat.is_empty() {
            creds.access_token.clone()
        } else {
            creds.pat.clone()
        };
        // 临期窗口消费 lazy_hours（与客户端通道同语义，不再写死 1h）：调用方传
        // i64::MAX（401 自愈/恒刷路径）时 saturating_mul 封顶 → 无条件重换——原硬编码
        // 1h 窗口会把「服务端已吊销但本地未临期」的令牌挡回 fresh，401 自愈失效
        let need_exchange = is_pat
            || creds
                .expires_at_ms
                .is_none_or(|e| e - now_ms < lazy_hours.saturating_mul(3_600_000));
        if !need_exchange {
            return (creds, false, "fresh");
        }
        return match exchange_job_token(agent, &pat, &state.data_dir) {
            Some(new) => {
                // 落库失败不能静默：否则新作业令牌只存活本轮，下轮仍走 PAT 重换
                if let Err(e) = save_token_store(state, acct_id, &new) {
                    fs_utils::app_log(&state.data_dir, &format!("[qoder] token 落库失败(id={acct_id}, PAT通道): {e}"));
                }
                // §5.10：换取产物为全新 Creds 不含指纹，返回前在内存层补注入账号绑定
                // 指纹（effective_creds 同款合并语义）——否则本轮后续请求丢
                // Cosy-MachineId/Cosy-MachineToken 头。随机 machine_token 不落库
                //（save_token_store 空串不覆盖），保持「每会话现场随机」模型。
                let mut out = new;
                let profile = load_device_profile(state, acct_id);
                super::qoder_device::merge_device_profile(&mut out, profile.as_ref());
                (out, true, "refreshed")
            }
            None => (creds, false, "pat_rejected"),
        };
    }
    // ── 客户端 token 通道：惰性刷新（deviceToken/refresh）──
    if let Some(exp) = creds.expires_at_ms {
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    if let Some(new) = refresh_token_once(agent, &creds) {
        // §5.10：指纹不落库。refresh_token_once 原样保留注入指纹（返回值供本轮请求带
        // 设备头），但落库前须剥离设备字段——注入值/随机 machine_token 回写 store 后会被
        // creds_of 读作「真实捕获」，污染 merge_device_profile 的优先级①判定，且把
        // 「每会话现场随机」的 machine_token 固化。空串经 save_token_store 跳过 →
        // 不覆盖存量（真实捕获值如有则原样保留），与 PAT 通道落库语义一致。
        let mut persist = new.clone();
        persist.machine_id.clear();
        persist.machine_token.clear();
        // 落库失败不能静默：否则刷新结果只存活本轮，凭证可能在下轮前过期
        if let Err(e) = save_token_store(state, acct_id, &persist) {
            fs_utils::app_log(&state.data_dir, &format!("[qoder] token 落库失败(id={acct_id}, 客户端通道): {e}"));
        }
        return (new, true, "refreshed");
    }
    (creds, false, "refresh_failed")
}

// ── 账号池回写 ─────────────────────────────────────────────────────────────

/// 刷新成功后回写账号池过期时间与登录态标记（调度/到期数据源）。
/// 供 qoder_checkin / qoder_credits（401 自愈）等通道共用。
pub fn sync_pool_expiry(state: &AppState, aid: &str, creds: &QoderCreds) {
    // I09：直操原始 JSON 保留未知字段（不可走 with_pool_mut），持池锁防并发整池覆盖丢写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            if a.get("id").and_then(Value::as_str) == Some(aid) {
                a["token_expires_at"] = serde_json::json!(creds.expires_at_ms.map(|ms| ms.div_euclid(1000)));
                a["needs_relogin"] = serde_json::json!(false);
                a["relogin_reason"] = serde_json::json!("");
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::qoder_pool_save(&crate::store::db(&state.data_dir), &pool);
    }
}

// ── 账号 id（对齐 wb- 惯例：qd- + sha256[..12]）────────────────────────────

/// 稳定账号 id：同 token 稳定同 id（防换发 token 重复入池）
pub fn account_id_of(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("qd-{}", &hex[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn creds_of_parses_flat_and_nested() {
        let flat = creds_of(&json!({"accessToken": "pt-abc", "uid": "u1", "kind": "pat"}));
        assert_eq!(flat.access_token, "pt-abc");
        assert_eq!(flat.uid, "u1");
        assert_eq!(flat.kind, "pat");
        let nested = creds_of(&json!({
            "account": {"uid": "u2", "nickname": "n"},
            "auth": {"access_token": "jt-x", "expiresAtMs": 123}
        }));
        assert_eq!(nested.access_token, "jt-x");
        assert_eq!(nested.uid, "u2");
        assert_eq!(nested.expires_at_ms, Some(123));
    }

    #[test]
    fn account_id_stable_and_prefixed() {
        let a = account_id_of("pt-abc");
        let b = account_id_of("pt-abc");
        assert_eq!(a, b);
        assert!(a.starts_with("qd-"));
        assert_eq!(a.len(), "qd-".len() + 12);
    }

    #[test]
    fn build_auth_headers_carries_cosy_and_device_passthrough() {
        let mut c = QoderCreds { access_token: "pt-x".into(), ..Default::default() };
        let h = build_auth_headers(&c);
        assert!(h.iter().any(|(k, v)| k == "Cosy-ClientType" && v == "10"));
        assert!(!h.iter().any(|(k, _)| k == "Cosy-MachineId"), "设备头缺失时不得伪造");
        c.machine_id = "mid".into();
        c.machine_token = "mtk".into();
        let h = build_auth_headers(&c);
        assert!(h.iter().any(|(k, v)| k == "Cosy-MachineId" && v == "mid"));
        assert!(h.iter().any(|(k, v)| k == "Cosy-MachineToken" && v == "mtk"));
    }

    /// R-6 抓包固化：clientId 为 PAT 派生的稳定 UUID 格式串
    #[test]
    fn job_client_id_stable_uuid_format() {
        let a = job_client_id("pt-abc");
        let b = job_client_id("pt-abc");
        assert_eq!(a, b, "同 PAT 稳定同 clientId");
        assert_ne!(job_client_id("pt-xyz"), a, "不同 PAT 不同 clientId");
        // 8-4-4-4-12 UUID 形态
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    /// R-6 抓包实测 expires_in=86400000（毫秒，24h）；秒级值兼容
    #[test]
    fn normalize_expires_in_handles_ms_and_seconds() {
        assert_eq!(normalize_expires_in(86_400_000), 86_400_000, "毫秒原样");
        assert_eq!(normalize_expires_in(86_400), 86_400_000, "秒级 ×1000");
        // 阈值边界：30 天秒级恰为分界（>2_592_000 判毫秒原样），其下判秒级 ×1000
        assert_eq!(normalize_expires_in(2_592_000), 2_592_000_000, "30 天秒级 ×1000");
        assert_eq!(normalize_expires_in(2_592_001), 2_592_001, "超阈值判毫秒原样");
        // 1h 秒级（3600）不得被误判为毫秒（3.6e6 ms 恰为 1h 毫秒级下限之下仍安全）
        assert_eq!(normalize_expires_in(3_600_000), 3_600_000, "1h 毫秒原样");
        assert_eq!(normalize_expires_in(3_600), 3_600_000, "1h 秒级 ×1000");
        let now = chrono::Utc::now().timestamp_millis();
        let exp = now + normalize_expires_in(86_400_000);
        let hours = (exp - now) as f64 / 3_600_000.0;
        assert!((hours - 24.0).abs() < 0.01, "24h 窗口");
    }
}
