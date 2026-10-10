//! WorkBuddy 公共请求层（原 src-python/wb_common.py 的 Rust 移植）。
//! 依据 docs/workbuddy-product-design.md §5.1~§5.4：
//! - 凭证双源化（F-10）：工具侧 token_store 与桌面 auth 文件「谁新用谁」（expiresAtMs 晚者胜出）
//! - 统一请求头（§5.3）+ 宽容解析 dig（复用 fs_utils::dig 信封下钻语义，对齐 wb.dig）
//! - 红线：chat 请求绝不携带 X-Refresh-Token；仅 refresh 端点携带
//! - 网络：ureq 默认直连（不读系统/环境代理），对齐 python OPENER 绕代理约定
//!
//! P1 先行落地（wb_credits 依赖）；P2 wb_checkin / trae_checkin 直接复用本层。

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::fs_utils;
use crate::state::AppState;

// ── 路径 ────────────────────────────────────────────────────────────────────
// （SQLite 化 P3：pool/token store/checkin results 均改走 store，文件路径函数已删除）

/// 桌面 auth 文件读取路径：settings.wb_auth_file_path 人工指定优先（与
/// commands/workbuddy/common.rs auth_file_path_of 同语义），否则默认布局。
fn auth_file_path(state: &AppState) -> PathBuf {
    if let Some(p) = state.settings().wb_auth_file_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local)
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

// ── 凭证结构（对齐 python creds_of 返回 dict）───────────────────────────────

#[derive(Serialize, Clone, Default, Debug)]
pub struct Creds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub refresh_expires_at_ms: Option<i64>,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub domain: String,
    /// 账号区域（Switch AI：WorkBuddy 国际版）——`"cn"` / `"global"`，空串 = 按 domain 推断。
    /// 国际版账号的签到/积分/刷新都据此路由；显式值优先于 domain 推断。
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub edition: String,
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// 宽容整数：数字或数字字符串（对齐 python int(v) 兜底）
fn i_of(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
}

/// 从 auth 文件 / token store 记录中提取凭证字段（兼容多种嵌套形态，F-04）。
pub fn creds_of(source: &Value) -> Creds {
    let auth = source.get("auth").filter(|v| v.is_object()).unwrap_or(source);
    let account = source
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(source);
    Creds {
        access_token: s_of(fs_utils::dig(auth, &["accessToken", "access_token", "token"])),
        refresh_token: s_of(fs_utils::dig(auth, &["refreshToken", "refresh_token"])),
        expires_at_ms: i_of(fs_utils::dig(
            auth,
            &[
                "expiresAtMs",
                "expires_at_ms",
                "expiresAt",
                "expires_in_ms",
                "accessTokenExpiresAtMs",
            ],
        )),
        refresh_expires_at_ms: i_of(fs_utils::dig(
            auth,
            &["refreshExpiresAtMs", "refresh_expires_at_ms", "refreshExpiresAt"],
        )),
        uid: s_of(fs_utils::dig(account, &["uid", "userId", "user_id", "id"])),
        domain: s_of(fs_utils::dig(source, &["domain"])),
        // Switch AI：区域随凭证一起提取（token store 记录里由 OAuth/导入写入），
        // 与 domain 一起参与区域解析——domain 缺失时区域仍可靠。
        region: s_of(fs_utils::dig(source, &["region"])),
        nickname: s_of(fs_utils::dig(account, &["nickname", "name", "displayName"])),
        edition: s_of(fs_utils::dig(account, &["editionType", "edition_type", "edition"])),
    }
}

fn read_auth_file(state: &AppState) -> Creds {
    creds_of(&fs_utils::read_json::<Value>(&auth_file_path(state)))
}

// ── token store 凭证 vault 收敛（审查 P0-1）────────────────────────────────
//
// wb_tokens 表中敏感字段（access_token / refresh_token）一律占位（空串）存储，
// 明文经 vault::ns_set 走 Stronghold + DPAPI 加密（与 Trae 家族同一 vault）。
// 读取统一回填（仅内存）；写入统一占位。vault 写失败时禁止明文落库（对齐 Trae 红线）。

/// 敏感字段键名集合：snake_case 为主（save_token_store / oauth 写入形态），
/// 兼容 auth 文件导入的 camelCase 形态（accessToken / refreshToken）。
const TOKEN_SENSITIVE_KEYS: [&str; 4] = [
    "access_token",
    "accessToken",
    "refresh_token",
    "refreshToken",
];

/// DB 读取 + vault 回填（仅内存，不落明文）：敏感字段为占位空串时从 vault 回填。
/// vault 不可用 / 无记录 → 保持空串（上层按 no_credential 处理，fail-secure）。
pub fn token_store_load_secure(data_dir: &std::path::Path) -> Value {
    let mut store = crate::store::docs::wb_token_store_load(&crate::store::db(data_dir));
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return store;
    };
    for (id, rec) in tokens.iter_mut() {
        let Some(rm) = rec.as_object_mut() else { continue };
        let Some(sec) = crate::vault::ns_get(data_dir, "wb", id) else { continue };
        let acc = sec.get("access_token").and_then(Value::as_str).unwrap_or("");
        let rt = sec.get("refresh_token").and_then(Value::as_str).unwrap_or("");
        let mut has_access_key = false;
        let mut has_refresh_key = false;
        for k in TOKEN_SENSITIVE_KEYS {
            let is_access = k.ends_with("ccess_token");
            let val = if is_access { acc } else { rt };
            if !rm.contains_key(k) {
                continue;
            }
            if is_access {
                has_access_key = true;
            } else {
                has_refresh_key = true;
            }
            // 仅填空值：DB 明文优先（更新鲜，如迁移残留，待下次写入收敛）
            if !val.is_empty()
                && rm.get(k).and_then(Value::as_str).map_or(true, |s| s.is_empty())
            {
                rm.insert(k.to_string(), serde_json::json!(val));
            }
        }
        // rec 无任何敏感键但 vault 有值 → 补 snake_case 两键（与 save_token_store 写入形态一致）
        if !acc.is_empty() && !has_access_key {
            rm.insert("access_token".into(), serde_json::json!(acc));
        }
        if !rt.is_empty() && !has_refresh_key {
            rm.insert("refresh_token".into(), serde_json::json!(rt));
        }
    }
    store
}

/// 单账号 UPSERT + 敏感字段 vault 收敛：非空敏感值字段级合并写入 vault（不丢旧值），
/// DB 记录一律占位（空串）。vault 写失败时仍落占位行并返回 Err（禁止明文落库）。
pub fn token_store_upsert_secure(
    data_dir: &std::path::Path,
    id: &str,
    rec: &Value,
) -> Result<(), String> {
    let mut rec = rec.clone();
    let Some(rm) = rec.as_object_mut() else {
        return crate::store::docs::wb_token_store_upsert(&crate::store::db(data_dir), id, &rec);
    };
    // 提取非空敏感值（两组键名各取其一）
    let mut acc = String::new();
    let mut rt = String::new();
    for k in TOKEN_SENSITIVE_KEYS {
        let v = rm.get(k).and_then(Value::as_str).unwrap_or("");
        if v.is_empty() {
            continue;
        }
        if k.ends_with("ccess_token") {
            acc = v.to_string();
        } else {
            rt = v.to_string();
        }
    }
    let vault_result = if acc.is_empty() && rt.is_empty() {
        Ok(())
    } else {
        let mut entry = crate::vault::ns_get(data_dir, "wb", id)
            .unwrap_or_else(|| serde_json::json!({}));
        if !acc.is_empty() {
            entry["access_token"] = serde_json::json!(acc);
        }
        if !rt.is_empty() {
            entry["refresh_token"] = serde_json::json!(rt);
        }
        crate::vault::ns_set(data_dir, "wb", id, &entry)
    };
    // 无论 vault 成败，DB 一律占位
    for k in TOKEN_SENSITIVE_KEYS {
        if rm.contains_key(k) {
            rm.insert(k.to_string(), serde_json::json!(""));
        }
    }
    crate::store::docs::wb_token_store_upsert(&crate::store::db(data_dir), id, &rec)?;
    vault_result.map_err(|e| {
        format!("WB 凭据加密存储失败（已仅保存占位信息，重新登录可恢复）: {e}")
    })
}

/// 生效凭证 = token store 与 auth 文件中 expiresAtMs 更晚者（F-10 谁新用谁）。
/// auth 文件仅当其 uid 与账号匹配时参与双源比较（桌面当前登录态）。
/// 读 token store（SQLite 化 P3：wb_tokens 表；结构 {version, tokens:{id:rec}}）。
/// 全部 token store 读点统一入口。
pub fn load_token_store(state: &AppState) -> Value {
    token_store_load_secure(&state.data_dir)
}

pub fn effective_creds(state: &AppState, acct_id: &str, acct_uid: &str) -> Creds {
    let store: Value = load_token_store(state);
    let store_creds = store
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    let mut file_creds = read_auth_file(state);
    if !acct_uid.is_empty() && !file_creds.uid.is_empty() && file_creds.uid != acct_uid {
        file_creds = Creds::default();
    }
    let a = store_creds.expires_at_ms;
    let b = file_creds.expires_at_ms;
    // python: file 有 token 且 (store 无到期时间 或 file 到期 >= store 到期)
    let file_newer = a.is_none() || b.is_some_and(|bv| a.map_or(true, |av| bv >= av));
    if !file_creds.access_token.is_empty() && file_newer {
        return file_creds;
    }
    store_creds
}

// ── 并发刷新防护（审查 H-1，对齐 qoder_common 同款双锁）──────────────────────

/// token store 表级读改写互斥：load→merge→upsert 非原子，签到/积分/续期/导入/
/// OAuth 多通道并发写会互相覆盖丢更新（last-writer-wins 抹掉彼此的新 token）。
/// tasks 侧 save_token_store 与 commands 侧 upsert_token_store 共用本锁串行化
/// 表级读改写；仅护本地 IO，不覆盖网络请求路径（无死锁面）。
/// 锁序约定：账号刷新锁 → 本表锁（单向获取，无环）。
pub(crate) static WB_TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 每账号刷新互斥（审查 H-1/P0）：签到/积分 401 自愈/续期/调度器与 schtasks CLI
/// 双进程多通道并发触发同一账号刷新时，两个线程可能同时拿旧 refresh_token 换新
/// token——服务端一次性轮换下后落库者覆盖先落库者，被覆盖方刚拿到的令牌即刻失效
///（严重时 refresh_token 一并丢，被迫重登）。以账号 id 为键的进程内锁串行化
/// 「读凭证→网络刷新→落库」全程；持锁后重读 token store 天然构成二次检查。
static WB_REFRESH_LOCKS: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::Mutex::new(None);

pub(crate) fn refresh_lock_for(acct_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    let mut g = WB_REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(std::collections::HashMap::new)
        .entry(acct_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// 账号移除路径主动清理（P3-L，对齐 qoder refresh_lock_remove）：回收刷新锁表项，
/// 防止账号删除后条目永驻 HashMap（进程生命周期内每次增删账号泄漏一把锁）。
pub(crate) fn refresh_lock_remove(acct_id: &str) {
    let mut g = WB_REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = g.as_mut() {
        map.remove(acct_id);
    }
}

/// 跨进程刷新互斥（H-1 ②）：GUI 调度器与 schtasks CLI（--task-run，绕开单实例
/// 插件）双进程并存，进程内锁无法约束跨进程并发。命名互斥体按账号隔离
///（ns="wb"，scope=账号 id，锁名含 data_dir 短哈希）；抢锁失败幂等跳过本次刷新
///（沿用现有凭证，仅少一次自愈机会，无损失）。
fn cross_refresh_lock(
    state: &AppState,
    acct_id: &str,
) -> Option<crate::tasks::qoder_common::CrossProcLock> {
    let (guard, fail) = crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(
        &state.data_dir,
        "wb",
        acct_id,
        5000,
    );
    if guard.is_none() {
        let reason = fail
            .as_ref()
            .map(|f| f.describe())
            .unwrap_or_default();
        fs_utils::app_log(
            &state.data_dir,
            &format!("[wb-fresh] 跨进程刷新锁未获取(id={acct_id}, {reason})，本轮沿用现有凭证"),
        );
    }
    guard
}

/// 写工具侧凭证副本（F-10 谁新用谁）。version≠1 拒绝写入（版本闸门）；
/// 非空字段合并 + updated_at（与 commands/workbuddy/common.rs upsert_token_store 同语义）。
/// 凭证收敛（P0-1）：落库走 token_store_upsert_secure——敏感字段进 vault、DB 占位；
/// vault 写失败时仅落占位并返回 Err（禁止明文落库）。
pub fn save_token_store(state: &AppState, id: &str, creds: &Creds) -> Result<(), String> {
    // H-1：表级读改写互斥（与 commands 侧 upsert_token_store 共锁）
    let _table = WB_TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store: Value = load_token_store(state);
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
    let mut rec = serde_json::json!({});
    if let Some(t) = tokens.as_object() {
        if let Some(r) = t.get(id) {
            rec = r.clone();
        }
    }
    if let Some(rm) = rec.as_object_mut() {
        let val = serde_json::to_value(creds).map_err(|e| e.to_string())?;
        for (k, v) in val.as_object().into_iter().flatten() {
            if !v.is_null() {
                rm.insert(k.clone(), v.clone());
            }
        }
        rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
    }
    token_store_upsert_secure(&state.data_dir, id, &rec)?;
    // version 闸门语义不变（整库字段，单行 upsert 不携带 version——写入 kv 元数据）
    crate::store::docs::wb_token_store_save_version(&crate::store::db(&state.data_dir), 1)
}

// ── 客户端指纹伪装（issue #48）─────────────────────────────────────────────

/// billing/签到/刷新链路桌面端 UA 伪装：必须带版本号——裸 "WorkBuddy" 在
/// 个人中心「请求明细」的客户端列识别为 "-"（issue #48 反馈的可检测特征）。
pub const WB_DESKTOP_UA: &str = "WorkBuddy/5.5.6";

/// 账号级稳定设备指纹：sha256("wb-fingerprint:{kind}:{uid}") 前 16 字节 →
/// 32 位小写十六进制。同账号恒定（虚拟设备稳定）、跨账号隔离（防关联）；
/// uid 为空返回 None（缺失即不带，不伪造）。
pub fn derive_device_fingerprint(uid: &str, kind: &str) -> Option<String> {
    if uid.trim().is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("wb-fingerprint:{kind}:{uid}").as_bytes());
    let d = h.finalize();
    Some(d[..16].iter().map(|b| format!("{b:02x}")).collect())
}

// ── 统一请求头（§5.3）───────────────────────────────────────────────────────

/// Bearer + X-User-Id（缺省 X-No-* 占位）+ 客户端指纹（issue #48：UA 带版本、
/// X-Machine-ID/X-Session-ID 账号级稳定派生、X-Domain 域标识）；web_platform=true
/// 附加 X-Client-Platform: web（积分三件套必需）。
pub fn build_auth_headers(creds: &Creds, web_platform: bool) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", creds.access_token),
        ),
        ("User-Agent".to_string(), WB_DESKTOP_UA.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if creds.uid.is_empty() {
        h.push(("X-No-User-Id".to_string(), "1".to_string()));
    } else {
        h.push(("X-User-Id".to_string(), creds.uid.clone()));
        // 客户端指纹（issue #48）：个人中心请求明细按设备/会话标识识别客户端
        if let Some(mid) = derive_device_fingerprint(&creds.uid, "machine") {
            h.push(("X-Machine-ID".to_string(), mid));
        }
        if let Some(sid) = derive_device_fingerprint(&creds.uid, "session") {
            h.push(("X-Session-ID".to_string(), sid));
        }
    }
    if !creds.domain.is_empty() {
        h.push(("X-Domain".to_string(), creds.domain.clone()));
    }
    if web_platform {
        h.push(("X-Client-Platform".to_string(), "web".to_string()));
    }
    h
}

/// POST JSON → (http_status, parsed, raw_text)；status=0 表示网络不可达
///（对齐 python post_json 三元组；HTTPError 同样返回状态码与响应体原文）。
pub fn post_json_raw(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>, String) {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            // 审查 P2：读取失败不再吞成空串（调用方诊断日志需区分「空响应」与「读取失败」）
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

/// POST JSON → (http_status, parsed)；status=0 表示网络不可达
///（对齐 python post_json 三元组的可消费部分；HTTPError 同样返回状态码）。
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>) {
    let (status, parsed, _) = post_json_raw(agent, url, headers, body);
    (status, parsed)
}

/// GET 请求 → (http_status, parsed, raw_text)；status=0 网络不可达
///（对齐 python get_json，F-17 成长中心等 GET 端点；HTTPError 同样返回状态码）。
pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            // 审查 P2：读取失败不再吞成空串（调用方诊断日志需区分「空响应」与「读取失败」）
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

// ── 区域路由（T4.5/F-36，§5.2；Switch AI 扩展：WorkBuddy 国际版）────────────
// 三套区域基址**不可混用**（混用会把国际版账号请求打到国内网关）：
//   billing_base  CN https://www.codebuddy.cn    Global https://www.workbuddy.ai
//       用途：签到 / 成长中心 / billing meter 计量 / plugin token refresh
//   credits_base  CN https://www.workbuddy.cn    Global https://www.workbuddy.ai
//       用途：积分三件套 / 官方用量 / 活动 banner（工作台积分页）——
//       注意 CN 的积分站是 workbuddy.cn，与 billing 的 codebuddy.cn 不是同一站点
//   plugin_base   CN https://copilot.tencent.com Global https://www.workbuddy.ai
//       用途：OAuth（auth/state、auth/token、login/account）+ chat 上游 + 模型目录
//
// 国际版实测（2026，Switch AI 二次开发新增）：
//   POST https://www.workbuddy.ai/v2/plugin/auth/state?platform=CLI → 200
//        data.authUrl = https://www.workbuddy.ai/login?platform=CLI&state=<uuid>
//   GET  /v2/plugin/auth/token?state=  → {"code":11217,"msg":"...login ing..."}
//   POST /v2/plugin/auth/token/refresh → 401（凭证无效时）
//   即**国内版与国际版 OAuth 流程同构，仅基址不同**（无 PKCE / 无 client_id）。

pub const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
pub const BILLING_BASE_GLOBAL: &str = "https://www.workbuddy.ai";
/// 积分三件套（工作台积分页）国内基址；与 billing 的 codebuddy.cn 不同站。
pub const CREDITS_BASE_CN: &str = "https://www.workbuddy.cn";
/// chat 上游 / OAuth / 模型目录国内基址。
pub const CHAT_HOST_CN: &str = "https://copilot.tencent.com";
/// 国际版**同区兄弟站**（Switch AI 新增）。
///
/// 依据：国际版 CodeBuddy CLI 的 `product.json` 中
/// `authentication.attributes.externalDomain = ["www.codebuddy.ai", …]`，
/// 且 `endpoint = https://www.codebuddy.ai`（`productFeatures.InternationalLogin = true`）；
/// 实测 `/billing/meter/get-user-resource-summary` 与 `/v2/chat/completions` 均存在（401 需鉴权）。
/// 用作国际版的**备用域名**，保证双探测不跨区。
pub const CHAT_HOST_GLOBAL_ALT: &str = "https://www.codebuddy.ai";
/// plugin token refresh 路径（区域基址拼接）。
pub const REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";

/// WorkBuddy 账号区域（国内版 / 国际版）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WbRegion {
    #[default]
    Cn,
    Global,
}

impl WbRegion {
    /// 宽松字符串解析：接受前端 / 配置 / 导入文件里的各种写法。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "global" | "intl" | "international" | "oversea" | "overseas" | "ai" | "en" => {
                WbRegion::Global
            }
            _ => WbRegion::Cn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WbRegion::Cn => "cn",
            WbRegion::Global => "global",
        }
    }

    pub fn is_global(self) -> bool {
        matches!(self, WbRegion::Global)
    }

    /// 由账号 domain 推断区域。
    ///
    /// 修正历史缺陷：旧实现用 `domain.contains(".workbuddy.ai")`（**前导点**），
    /// domain 恰为 `workbuddy.ai`（无子域前缀）时会被误判为国内版。
    /// 此处改为主机名后缀匹配，并容忍带协议 / 端口 / 前导点 / 大小写的写法。
    pub fn from_domain(domain: &str) -> Self {
        let d = domain.trim().to_ascii_lowercase();
        if d.is_empty() {
            return WbRegion::Cn;
        }
        let host = d
            .rsplit("://")
            .next()
            .unwrap_or(&d)
            .split('/')
            .next()
            .unwrap_or(&d)
            .split(':')
            .next()
            .unwrap_or(&d)
            .trim_start_matches('.');
        let is_intl = |h: &str, root: &str| h == root || h.ends_with(&format!(".{root}"));
        if is_intl(host, "workbuddy.ai") || is_intl(host, "codebuddy.ai") {
            WbRegion::Global
        } else {
            WbRegion::Cn
        }
    }

    /// 签到 / 成长中心 / billing meter / plugin refresh 基址。
    pub fn billing_base(self) -> &'static str {
        match self {
            WbRegion::Cn => BILLING_BASE_CN,
            WbRegion::Global => BILLING_BASE_GLOBAL,
        }
    }

    /// 积分三件套 / 官方用量 / 活动基址。
    pub fn credits_base(self) -> &'static str {
        match self {
            WbRegion::Cn => CREDITS_BASE_CN,
            WbRegion::Global => BILLING_BASE_GLOBAL,
        }
    }

    /// chat 上游 / 模型目录基址。
    pub fn chat_base(self) -> &'static str {
        match self {
            WbRegion::Cn => CHAT_HOST_CN,
            WbRegion::Global => BILLING_BASE_GLOBAL,
        }
    }

    /// OAuth（auth/state、auth/token、login/account）+ CLI 登录页基址。
    pub fn plugin_base(self) -> &'static str {
        self.chat_base()
    }

    /// OAuth Web 侧 Origin / Referer。
    pub fn web_origin(self) -> &'static str {
        match self {
            WbRegion::Cn => BILLING_BASE_CN,
            WbRegion::Global => BILLING_BASE_GLOBAL,
        }
    }

    /// plugin token refresh 端点（区域感知；国际版账号必须走 workbuddy.ai）。
    pub fn refresh_url(self) -> String {
        format!("{}{}", self.billing_base(), REFRESH_PATH)
    }

    /// 备用域名（§2.2 双探测）：**必须与主域名同区**。
    ///
    /// Switch AI 修正：原实现对 Global 返回国内 `codebuddy.cn`，会把国际版
    /// bearer token 送到国内网关。而按本项目自身的技术契约
    /// （tech-framework §B.2：「令牌域与请求域不一致会被网关拒绝」），
    /// 该跨区重试**不可能成功**，只是把凭证暴露给错误区域。
    /// 现在 Global 的备用域名改为国际版兄弟站 `www.codebuddy.ai`（实测可用）。
    ///
    /// 注意：国内分支有意保持上游既有语义（CN → 国际镜像探测）不变——
    /// 该路径已被上游验证，本次不做改动以免引入回归；如需同样收敛可后续单独评估。
    pub fn alt_billing_base(self) -> &'static str {
        match self {
            WbRegion::Cn => BILLING_BASE_GLOBAL,
            WbRegion::Global => CHAT_HOST_GLOBAL_ALT,
        }
    }
}

/// 域名双探测（§2.2 接口稳定性）：主域名在前、备用域名在后。
/// 两项均由 `domain` 的**所属区域**推导（Switch AI：保证同区，不跨区）。
pub fn billing_bases(domain: &str) -> [&'static str; 2] {
    let r = WbRegion::from_domain(domain);
    [r.billing_base(), r.alt_billing_base()]
}

/// 账号区域解析（Switch AI：WorkBuddy 国际版）——**区域权威来源**。
///
/// 优先级：账号显式 `region` 字段 > 凭证记录 `region` 键 > 凭证记录 `domain` > 默认 CN。
///
/// 为什么需要显式字段：`domain` 只在部分来源里出现（auth 文件 / OAuth 返回），
/// 手工录入或旧版导入的账号往往没有 `domain`，此时旧实现会静默按 CN 处理——
/// 国际版账号的签到/积分/chat 请求会被打到国内网关而失败。
/// 显式 `region` 让国际版身份不依赖可缺失的推断字段。
pub fn resolve_region(explicit: &str, token_rec: Option<&Value>) -> WbRegion {
    if !explicit.trim().is_empty() {
        return WbRegion::parse(explicit);
    }
    let (rec_region, rec_domain) = match token_rec {
        Some(rec) => (
            rec.get("region").and_then(Value::as_str).unwrap_or(""),
            rec.get("domain").and_then(Value::as_str).unwrap_or(""),
        ),
        None => ("", ""),
    };
    if !rec_region.trim().is_empty() {
        return WbRegion::parse(rec_region);
    }
    WbRegion::from_domain(rec_domain)
}

/// 便捷版：调用方只掌握 `domain` 字符串（如 `Creds.domain`）时使用。
pub fn resolve_region_str(explicit: &str, domain: &str) -> WbRegion {
    if !explicit.trim().is_empty() {
        return WbRegion::parse(explicit);
    }
    WbRegion::from_domain(domain)
}

// ── token 刷新（F-09）──────────────────────────────────────────────────────

/// refresh 失败原因（审查 P1-4：区分「凭证失效」与「网络故障」，
/// 供调用方决定是否标记 needs_relogin——网络失败不应误标）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFail {
    /// 无 refreshToken（不可刷新）
    NoRefreshToken,
    /// 网络不可达 / 传输层失败（不标记重登录，下次重试）
    Network,
    /// HTTP 4xx：refresh token 已失效（应标记 needs_relogin）
    Auth,
    /// 200 但响应无 accessToken（响应结构异常，不标记重登录）
    BadResponse,
    /// H-1：跨进程锁未获取（他方进程正在刷新同账号，本侧幂等跳过；不标记重登录）
    Busy,
}

/// 调 plugin refresh 端点（X-Refresh-Token 仅允许出现在此端点）。
/// 成功返回新 Creds（expires 字段按 expiresIn/refreshExpiresIn 秒数回填）。
pub fn refresh_token_once(agent: &ureq::Agent, creds: &Creds) -> Option<Creds> {
    refresh_token_once_ex(agent, creds).0
}

/// refresh_token_once 的带失败原因版本（审查 P1-4）。
/// 区域由 `creds.region` 优先、`creds.domain` 兜底推断——
/// 国际版账号自动走 workbuddy.ai 的 refresh 端点。
pub fn refresh_token_once_ex(agent: &ureq::Agent, creds: &Creds) -> (Option<Creds>, RefreshFail) {
    refresh_token_once_region(
        agent,
        creds,
        resolve_region_str(&creds.region, &creds.domain),
    )
}

/// 区域感知刷新（Switch AI：WorkBuddy 国际版）。
/// 国际版账号必须打 `https://www.workbuddy.ai/v2/plugin/auth/token/refresh`；
/// 打国内 codebuddy.cn 会把 refresh token 送到错误的网关注销。
pub fn refresh_token_once_region(
    agent: &ureq::Agent,
    creds: &Creds,
    region: WbRegion,
) -> (Option<Creds>, RefreshFail) {
    if creds.refresh_token.is_empty() {
        return (None, RefreshFail::NoRefreshToken);
    }
    let mut h = build_auth_headers(creds, false);
    h.push(("X-Refresh-Token".to_string(), creds.refresh_token.clone()));
    h.push((
        "X-Auth-Refresh-Source".to_string(),
        "workbuddy".to_string(),
    ));
    let url = region.refresh_url();
    let (status, body) = post_json(agent, &url, &h, &serde_json::json!({}));
    if status == 0 {
        return (None, RefreshFail::Network);
    }
    if (400..500).contains(&status) {
        return (None, RefreshFail::Auth);
    }
    if status != 200 {
        return (None, RefreshFail::Network); // 5xx 等服务端故障：不标记重登录
    }
    let Some(body) = body else {
        return (None, RefreshFail::BadResponse);
    };
    let acc = s_of(fs_utils::dig(&body, &["accessToken"]));
    if acc.is_empty() {
        return (None, RefreshFail::BadResponse);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut out = creds.clone();
    out.access_token = acc;
    let ref_tok = s_of(fs_utils::dig(&body, &["refreshToken"]));
    if !ref_tok.is_empty() {
        out.refresh_token = ref_tok;
    }
    out.expires_at_ms = i_of(fs_utils::dig(&body, &["expiresIn"])).map(|e| now_ms + e * 1000);
    out.refresh_expires_at_ms = i_of(fs_utils::dig(&body, &["refreshExpiresIn"]))
        .map(|e| now_ms + e * 1000);
    (Some(out), RefreshFail::NoRefreshToken)
}

/// 标记账号需重新登录（审查 P1-4：刷新凭证失效时回写账号池，
/// 网关池同步（sync_from_wb disabled=needs_relogin）与调度跳过随之生效）
pub fn mark_needs_relogin(state: &AppState, acct_id: &str, reason: &str) {
    let store = crate::store::db(&state.data_dir);
    // 读-改-写互斥：罩住 load→改→save 段，防命令/后台/调度线程旧副本整文档覆盖
    let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = crate::store::docs::wb_pool_load(&store);
    let mut changed = false;
    if let Some(arr) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in arr {
            if a.get("id").and_then(Value::as_str) == Some(acct_id) {
                a["needs_relogin"] = serde_json::json!(true);
                a["relogin_reason"] = serde_json::json!(reason);
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::wb_pool_save(&store, &pool);
        fs_utils::app_log(
            &state.data_dir,
            &format!("wb: 账号 {acct_id} 已标记需重新登录（{reason}）"),
        );
    }
}

// ── 本地 quota 端口发现兜底（T5.8/F-21）────────────────────────────────────
// 云端 billing 全链失败时的最后兜底：WorkBuddy/CodeBuddy 桌面端本地服务在
// 127.0.0.1 暴露 quota 查询端点。发现顺序：
// ① ~/.workbuddy/*.port 文件声明的端口；② 固定候选端口；③ 有界端口段。
// 红线：单次单发不重试、每端口 0.8s 超时、候选总数有界。

const QUOTA_PATH: &str = "/api/v1/quota";
const QUOTA_PORT_CANDIDATES: [u16; 4] = [18789, 11101, 8890, 8899];
const QUOTA_PORT_RANGE: std::ops::RangeInclusive<u16> = 18780..=18795;

/// 响应含 remaining/credits/quota/balance 任一键即认定 quota 端点（浅层宽容）。
fn quota_looks_valid(v: &Value, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    if let Some(map) = v.as_object() {
        for (k, val) in map {
            let kl = k.to_ascii_lowercase();
            if kl == "remaining" || kl == "credits" || kl == "quota" || kl == "balance" {
                return true;
            }
            if quota_looks_valid(val, depth + 1) {
                return true;
            }
        }
    }
    false
}

fn quota_probe_port(agent: &ureq::Agent, port: u16) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}{QUOTA_PATH}");
    let resp = agent
        .get(&url)
        .timeout(Duration::from_millis(800))
        .set("User-Agent", "WorkBuddy")
        .set("Accept", "application/json")
        .call()
        .ok()?;
    let raw = resp.into_string().ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    quota_looks_valid(&v, 0).then_some(v)
}

/// 发现本机 quota 服务：*.port 声明端口 → 固定候选 → 有界端口段。
/// 返回已确认可用的 (port, 响应)（按发现序，至多 limit 个）。
pub fn discover_local_quota_services(
    agent: &ureq::Agent,
    limit: usize,
) -> Vec<(u16, Value)> {
    let mut found: Vec<(u16, Value)> = vec![];
    let mut seen: std::collections::HashSet<u16> = Default::default();
    let mut ports: Vec<u16> = vec![];
    // ① ~/.workbuddy/*.port（服务启动时落盘的端口声明，最多扫 16 个）
    if let Ok(home) = std::env::var("USERPROFILE") {
        let wb_dir = PathBuf::from(home).join(".workbuddy");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&wb_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "port").unwrap_or(false))
            .collect();
        files.sort();
        for f in files.into_iter().take(16) {
            if let Ok(txt) = std::fs::read_to_string(&f) {
                if let Some(first) = txt.split_whitespace().next() {
                    if let Ok(p) = first.parse::<u16>() {
                        ports.push(p);
                    }
                }
            }
        }
    }
    // ② 固定候选 ③ 有界端口段
    for p in ports
        .into_iter()
        .chain(QUOTA_PORT_CANDIDATES.into_iter())
        .chain(QUOTA_PORT_RANGE.clone())
    {
        if seen.contains(&p) || p == 0 {
            continue;
        }
        seen.insert(p);
        if found.len() >= limit {
            return found;
        }
        if let Some(v) = quota_probe_port(agent, p) {
            found.push((p, v));
        }
    }
    found
}

/// 本地 quota 兜底余额：首个可用端点的 remaining/credits/balance 取数；无可用端点 None。
pub fn local_quota_balance(agent: &ureq::Agent) -> Option<f64> {
    for (_port, v) in discover_local_quota_services(agent, 2) {
        if let Some(num) = dig_num(fs_utils::dig(
            &v,
            &["remaining", "RemainingCapacity", "credits", "balance"],
        )) {
            return Some(num);
        }
    }
    None
}

/// 宽容取数：递归摘出首个数值（含数字字符串）。
pub fn dig_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Object(m) => m.values().find_map(|c| dig_num(Some(c))),
        Value::Array(a) => a.iter().find_map(|c| dig_num(Some(c))),
    }
}

// ── 惰性刷新（F-09/F-55，原 wb_common.ensure_fresh）────────────────────────

/// 惰性刷新：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
/// 返回 (creds, refreshed, note)，note ∈ no_credential/fresh/expired_needs_relogin/refreshed/refresh_failed。
///
/// H-1（P0 修复）：「读凭证→网络刷新→落库」全程持每账号刷新锁（进程内）+ 跨进程
/// 命名互斥体，双进程/多通道并发触发同账号刷新时串行化；持锁后重读凭证构成二次
/// 检查——他人已刷新落库则直接命中新凭证（fresh），不再重复发网络请求。新鲜快路径
/// 无锁直返（调度/签到热路径不承担锁开销）。
pub fn ensure_fresh(
    state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    lazy_hours: i64,
) -> (Creds, bool, &'static str) {
    let acct_id = acct.get("id").and_then(Value::as_str).unwrap_or("");
    let acct_uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
    let creds = effective_creds(state, acct_id, acct_uid);
    if creds.access_token.is_empty() {
        return (creds, false, "no_credential");
    }
    if let Some(exp) = creds.expires_at_ms {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    // H-1 ①：进程内每账号锁串行化；②跨进程命名互斥体（GUI/CLI 双进程）
    let lock = refresh_lock_for(acct_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) = cross_refresh_lock(state, acct_id) else {
        // 他方进程刷新中：幂等跳过（沿用现有凭证，不误标 needs_relogin）
        return (creds, false, "refresh_failed");
    };
    // 二次检查：他人已刷新落库 → 直接复用新凭证（不发网络请求）
    let creds = effective_creds(state, acct_id, acct_uid);
    if creds.access_token.is_empty() {
        return (creds, false, "no_credential");
    }
    if let Some(exp) = creds.expires_at_ms {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    if let Some(new) = refresh_token_once(agent, &creds) {
        let _ = save_token_store(state, acct_id, &new);
        return (new, true, "refreshed");
    }
    (creds, false, "refresh_failed")
}

/// 401 自愈专用加锁刷新（H-1）：串行化「网络刷新→落库」并做二次检查——他人已刷新
/// 落库（token store 中该账号 access_token 与调用方持有凭证不同）则直接复用新凭证，
/// 不再重复发网络请求；否则以调用方凭证刷新并落库（调用方无须再自行 save）。
/// 跨进程抢锁失败 → (None, Some(RefreshFail::Busy))：调用方按非 Auth 处理
///（不误标 needs_relogin，下次自然重试）。
pub fn refresh_token_once_locked(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    stale: &Creds,
) -> (Option<Creds>, Option<RefreshFail>) {
    let lock = refresh_lock_for(acct_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) = cross_refresh_lock(state, acct_id) else {
        return (None, Some(RefreshFail::Busy));
    };
    // 二次检查：store 权威源中该账号已换新 token（他人刷新落库）→ 直接复用。
    // 仅比对 store 记录（刷新产物只落 store，不落 auth 文件），避免 auth 文件
    // 属于其他账号时误判
    let store_cur: Creds = load_token_store(state)
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    if !store_cur.access_token.is_empty() && store_cur.access_token != stale.access_token {
        return (Some(store_cur), None);
    }
    let (new, fail) = refresh_token_once_ex(agent, stale);
    if let Some(n) = &new {
        let _ = save_token_store(state, acct_id, n);
    }
    (new, Some(fail))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============ WorkBuddy 区域路由（Switch AI：国际版支持） ============

    #[test]
    fn region_from_domain_covers_intl_and_cn_shapes() {
        // 国际版各种写法
        for d in [
            "www.workbuddy.ai",
            "workbuddy.ai",       // 历史缺陷：旧 contains(".workbuddy.ai") 会误判为 CN
            ".workbuddy.ai",
            "WORKBUDDY.AI",
            "https://www.workbuddy.ai/v2/chat/completions",
            "www.workbuddy.ai:443",
            "codebuddy.ai",
            "www.codebuddy.ai",
        ] {
            assert_eq!(WbRegion::from_domain(d), WbRegion::Global, "domain={d}");
        }
        // 国内版各种写法（含「看起来像但其实不是」的近似域名）
        for d in [
            "",
            "   ",
            "www.codebuddy.cn",
            "codebuddy.cn",
            "www.workbuddy.cn",
            "copilot.tencent.com",
            "https://www.codebuddy.cn/v2/activity/banner",
            "notworkbuddy.ai.evil.com", // 后缀匹配不能被绕过
            "workbuddy.ai.evil.com",
            "evil-workbuddy.ai",
        ] {
            assert_eq!(WbRegion::from_domain(d), WbRegion::Cn, "domain={d}");
        }
    }

    #[test]
    fn region_parse_and_roundtrip() {
        for s in ["global", "Global", " GLOBAL ", "intl", "international", "oversea", "overseas", "ai", "en"] {
            assert_eq!(WbRegion::parse(s), WbRegion::Global, "s={s}");
        }
        for s in ["cn", "CN", "", "  ", "china", "zh", "unknown"] {
            assert_eq!(WbRegion::parse(s), WbRegion::Cn, "s={s}");
        }
        // as_str ↔ parse 往返
        for r in [WbRegion::Cn, WbRegion::Global] {
            assert_eq!(WbRegion::parse(r.as_str()), r);
        }
        assert_eq!(WbRegion::default(), WbRegion::Cn);
    }

    #[test]
    fn region_bases_are_distinct_and_correct() {
        let cn = WbRegion::Cn;
        let gl = WbRegion::Global;

        // 三套基址不可混用：CN 的 billing 站 ≠ CN 的积分站
        assert_eq!(cn.billing_base(), "https://www.codebuddy.cn");
        assert_eq!(cn.credits_base(), "https://www.workbuddy.cn");
        assert_eq!(cn.chat_base(), "https://copilot.tencent.com");
        assert_eq!(cn.plugin_base(), "https://copilot.tencent.com");
        assert_eq!(cn.web_origin(), "https://www.codebuddy.cn");
        assert_ne!(cn.billing_base(), cn.credits_base(), "CN 两站必须区分");

        // 国际版全部落到 workbuddy.ai
        assert_eq!(gl.billing_base(), "https://www.workbuddy.ai");
        assert_eq!(gl.credits_base(), "https://www.workbuddy.ai");
        assert_eq!(gl.chat_base(), "https://www.workbuddy.ai");
        assert_eq!(gl.plugin_base(), "https://www.workbuddy.ai");
        assert_eq!(gl.web_origin(), "https://www.workbuddy.ai");

        // refresh 端点必须区域化（国际版打国内网关会注销 refresh token）
        assert_eq!(
            gl.refresh_url(),
            "https://www.workbuddy.ai/v2/plugin/auth/token/refresh"
        );
        assert_eq!(
            cn.refresh_url(),
            "https://www.codebuddy.cn/v2/plugin/auth/token/refresh"
        );
        assert!(gl.refresh_url() != cn.refresh_url());
    }

    #[test]
    fn region_double_probe_puts_main_first() {
        // Switch AI：国际版备用域名改为同区兄弟站 www.codebuddy.ai（原为国内 codebuddy.cn）
        assert_eq!(
            billing_bases("www.workbuddy.ai"),
            ["https://www.workbuddy.ai", "https://www.codebuddy.ai"]
        );
        // 国内分支保持上游既有语义
        assert_eq!(
            billing_bases(""),
            ["https://www.codebuddy.cn", "https://www.workbuddy.ai"]
        );
        // 枚举判定本身即区域权威（原 is_global_region/region_billing_base 包装已内联）
        assert!(WbRegion::from_domain("www.workbuddy.ai").is_global());
        assert!(!WbRegion::from_domain("www.codebuddy.cn").is_global());
        assert_eq!(WbRegion::from_domain("workbuddy.ai").billing_base(), BILLING_BASE_GLOBAL);
        assert_eq!(WbRegion::from_domain("x.cn").billing_base(), BILLING_BASE_CN);
    }

    /// Switch AI：备用域名**绝不跨区**——跨区会把 token 送到错误网关，
    /// 按 tech-framework §B.2「令牌域与请求域不一致会被网关拒绝」不可能成功。
    /// 这是本次修复的真实缺陷：Global 的 alt 原为国内 codebuddy.cn。
    #[test]
    fn global_alt_domain_never_crosses_region() {
        let gl = WbRegion::Global;
        let alt = gl.alt_billing_base();
        assert!(
            !alt.ends_with(".cn"),
            "国际版备用域名不得是国内站: {alt}"
        );
        assert_eq!(alt, CHAT_HOST_GLOBAL_ALT);
        assert!(WbRegion::from_domain(alt).is_global(), "备用域名必须仍判定为国际版");

        // 主域名与备用域名必须是不同主机（双探测才有意义）
        assert_ne!(alt, gl.billing_base());

        // 双探测列表（以主域名的区域推导）两项都应是国际站
        for b in billing_bases("www.workbuddy.ai") {
            assert!(WbRegion::from_domain(b).is_global(), "双探测项应为国际站: {b}");
        }
    }

    /// Switch AI：区域分类对齐**厂商自己的域名表**（非我方臆测）。
    /// 来源：国际版 CodeBuddy CLI 包内 `product.json`
    ///   authentication.attributes.internalDomain（国内）
    ///   authentication.attributes.externalDomain（国际）
    /// 以及 `endpoint = https://www.codebuddy.ai`、`productFeatures.InternationalLogin = true`。
    #[test]
    fn region_classification_matches_vendor_domain_tables() {
        // 国内域名表 → 必须判为 Cn
        for d in [
            "copilot.tencent.com",
            "staging-copilot.tencent.com",
            "www.codebuddy.cn",
            "staging.codebuddy.cn",
            "www.workbuddy.cn",
            "staging.workbuddy.cn",
            // iOADomain（企业 SSO，同属国内基建）
            "tencent.sso.copilot.tencent.com",
            "tencent.sso.codebuddy.cn",
        ] {
            assert_eq!(WbRegion::from_domain(d), WbRegion::Cn, "国内域名 {d}");
        }
        // 国际域名表 → 必须判为 Global
        for d in ["www.codebuddy.ai", "www.workbuddy.ai"] {
            assert_eq!(WbRegion::from_domain(d), WbRegion::Global, "国际域名 {d}");
        }
        // 国内积分站与国内计费站不同站，且都不属于国际版
        assert_eq!(WbRegion::from_domain(CREDITS_BASE_CN), WbRegion::Cn);
        assert_eq!(WbRegion::from_domain(BILLING_BASE_CN), WbRegion::Cn);
        assert_eq!(WbRegion::from_domain(CHAT_HOST_CN), WbRegion::Cn);
    }

    /// Switch AI：区域解析优先级（账号显式字段 > 凭证 region > 凭证 domain > CN）。
    /// 这是「国际版账号不被误判为国内版」的核心保证。
    #[test]
    fn resolve_region_precedence() {
        let intl_rec = serde_json::json!({ "domain": "www.workbuddy.ai" });
        let cn_rec = serde_json::json!({ "domain": "www.codebuddy.cn" });
        let region_only = serde_json::json!({ "region": "global" }); // 无 domain 的国际版账号
        let empty = serde_json::json!({});

        // 显式字段最优先（可覆盖凭证里的 domain）
        assert_eq!(resolve_region("global", Some(&cn_rec)), WbRegion::Global);
        assert_eq!(resolve_region("cn", Some(&intl_rec)), WbRegion::Cn);
        // 凭证 region 次之：domain 缺失时仍能判定国际版（旧实现的核心缺口）
        assert_eq!(resolve_region("", Some(&region_only)), WbRegion::Global);
        // 凭证 domain 再次之
        assert_eq!(resolve_region("", Some(&intl_rec)), WbRegion::Global);
        assert_eq!(resolve_region("", Some(&cn_rec)), WbRegion::Cn);
        // 全缺省 → CN
        assert_eq!(resolve_region("", Some(&empty)), WbRegion::Cn);
        assert_eq!(resolve_region("", None), WbRegion::Cn);
        assert_eq!(resolve_region("  ", None), WbRegion::Cn);
        // resolve_region_str 与之一致
        assert_eq!(resolve_region_str("", "www.workbuddy.ai"), WbRegion::Global);
        assert_eq!(resolve_region_str("global", ""), WbRegion::Global);
        assert_eq!(resolve_region_str("", ""), WbRegion::Cn);
    }

    // ==================== 客户端指纹伪装（issue #48） ====================

    #[test]
    fn derive_device_fingerprint_deterministic_and_isolated() {
        // 同账号恒定、32 位小写 hex；不同 kind / 不同 uid 互异；uid 空缺失即不带
        let a1 = derive_device_fingerprint("u-1", "machine").unwrap();
        let a2 = derive_device_fingerprint("u-1", "machine").unwrap();
        assert_eq!(a1, a2, "同账号指纹必须恒定");
        assert_eq!(a1.len(), 32);
        assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            derive_device_fingerprint("u-1", "machine").unwrap(),
            derive_device_fingerprint("u-1", "session").unwrap()
        );
        assert_ne!(
            derive_device_fingerprint("u-1", "machine").unwrap(),
            derive_device_fingerprint("u-2", "machine").unwrap(),
            "跨账号必须隔离防关联"
        );
        assert!(derive_device_fingerprint("", "machine").is_none());
        assert!(derive_device_fingerprint("  ", "machine").is_none());
    }

    #[test]
    fn auth_headers_carry_client_fingerprint() {
        let creds = Creds {
            access_token: "tk".into(),
            uid: "u-1".into(),
            domain: "d1".into(),
            ..Default::default()
        };
        let h = build_auth_headers(&creds, false);
        let get = |hs: &[(String, String)], k: &str| {
            hs.iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        // UA 带版本（issue #48：裸 "WorkBuddy" 在个人中心明细识别为 "-"）
        assert_eq!(get(&h, "User-Agent").as_deref(), Some(WB_DESKTOP_UA));
        assert_eq!(get(&h, "X-User-Id").as_deref(), Some("u-1"));
        assert_eq!(get(&h, "X-Domain").as_deref(), Some("d1"));
        // 设备/会话指纹：与派生函数一致、32 位 hex
        let mid = get(&h, "X-Machine-ID").expect("X-Machine-ID 必须存在");
        let sid = get(&h, "X-Session-ID").expect("X-Session-ID 必须存在");
        assert_eq!(mid, derive_device_fingerprint("u-1", "machine").unwrap());
        assert_eq!(sid, derive_device_fingerprint("u-1", "session").unwrap());
        assert_eq!(mid.len(), 32);
        // web 平台附加
        let hw = build_auth_headers(&creds, true);
        assert!(hw.iter().any(|(k, v)| k == "X-Client-Platform" && v == "web"));
        // uid/domain 缺失：X-No-* 占位、不带指纹与域标识
        let empty = build_auth_headers(&Creds::default(), false);
        assert_eq!(get(&empty, "X-No-User-Id").as_deref(), Some("1"));
        assert!(get(&empty, "X-User-Id").is_none());
        assert!(get(&empty, "X-Machine-ID").is_none());
        assert!(get(&empty, "X-Session-ID").is_none());
        assert!(get(&empty, "X-Domain").is_none());
    }
}
