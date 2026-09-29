//! Qoder OAuth 设备流（F-80 M1，R-10 抓包固化 2026-09-27）。
//!
//! 客户端真实链路（PKCE device flow）：
//! 1. 生成 `nonce`（uuid v4）、`verifier`（64 字符）、`machine_id`/`client_id`（uuid v4）
//! 2. 浏览器打开 `https://qoder.cn/device/selectAccounts?challenge=<BASE64URL(SHA256(verifier))>
//!    &challenge_method=S256&nonce=..&machine_id=..&client_id=..`（用户在页面完成授权）
//! 3. 轮询 `GET {open_api}/api/v1/deviceToken/poll?nonce=..&verifier=..&challenge_method=S256`
//!    - **pending = HTTP 404** `{"errorCode":"NotFound",...}`（实测）
//!    - **成功 = HTTP 200**：`{id, token(dt-), user_id, expires_in:2591999999(≈30d ms),
//!      refresh_token, refresh_token_expires_in:31103999999(≈360d ms), ...}`
//!
//! 红线：verifier/token 全程不入日志/事件（脱敏红线）。

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::qoder_common::{self, QoderCreds};
use crate::fs_utils;

/// 授权页基址（需求方实测样例：qoder.cn/device/selectAccounts）
pub const DEVICE_AUTH_BASE: &str = "https://qoder.cn/device/selectAccounts";

/// 轮询间隔（抓包实测约 1s）
pub const POLL_INTERVAL_MS: u64 = 1000;
/// 轮询超时（180s，覆盖人工在浏览器完成登录的时延）
pub const POLL_TIMEOUT_MS: u64 = 180_000;

/// 一次设备流会话的入参（nonce/verifier/machine_id/client_id 全程自持）
pub struct DeviceFlow {
    pub nonce: String,
    pub verifier: String,
    /// 会话标识（仅入授权页 URL 与测试断言；M2 MITM 透传时复用）
    #[allow(dead_code)]
    pub machine_id: String,
    #[allow(dead_code)]
    pub client_id: String,
    /// 授权页完整 URL（challenge = BASE64URL(SHA256(verifier))，S256）
    pub auth_url: String,
}

impl DeviceFlow {
    pub fn new() -> Self {
        // verifier：双 uuid v4 拼接 = 64 hex 字符（抓包样本同为 64 字符长度量级）
        let verifier = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let nonce = uuid::Uuid::new_v4().to_string();
        let machine_id = uuid::Uuid::new_v4().to_string();
        let client_id = uuid::Uuid::new_v4().to_string();
        // challenge：BASE64URL_NO_PAD(SHA256(verifier))（PKCE S256）
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        let auth_url = format!(
            "{DEVICE_AUTH_BASE}?challenge={challenge}&challenge_method=S256&nonce={nonce}&machine_id={machine_id}&client_id={client_id}"
        );
        Self { nonce, verifier, machine_id, client_id, auth_url }
    }
}

impl Default for DeviceFlow {
    fn default() -> Self {
        Self::new()
    }
}

/// 单次轮询：GET /api/v1/deviceToken/poll（无 Authorization 头，与客户端一致）。
/// 返回 (http_status, parsed)；status=0 网络不可达。
pub fn poll_once(agent: &ureq::Agent, flow: &DeviceFlow) -> (u16, Option<Value>) {
    let url = format!(
        "{}/api/v1/deviceToken/poll?nonce={}&verifier={}&challenge_method=S256",
        qoder_common::OPEN_API_BASE,
        flow.nonce,
        urlencode(&flow.verifier)
    );
    let headers = vec![
        ("User-Agent".to_string(), qoder_common::CLIENT_USER_AGENT.to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let (status, parsed, _raw) = qoder_common::get_json(agent, &url, &headers);
    (status, parsed)
}

/// 轮询成功响应 → (凭证, uid)。宽容解析：token/accessToken 候选，
/// expires_in 毫秒级归一（2591999999 ≈ 30d）。
pub fn parse_poll_success(body: &Value) -> Option<(QoderCreds, String)> {
    let token = fs_utils::dig(body, &["token", "accessToken", "access_token"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return None;
    }
    let uid = fs_utils::dig(body, &["user_id", "userId", "uid"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    // expires_in 毫秒/秒归一统一走 qoder_common::normalize_expires_in
    //（原内联阈值 1e7 与 common 的 2_592_000 两套口径，中间值语义有歧义）
    let expires_at_ms = fs_utils::dig(body, &["expires_in", "expiresIn"])
        .and_then(|v| {
            v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
        })
        .map(|e| now_ms + qoder_common::normalize_expires_in(e));
    let refresh_token = fs_utils::dig(body, &["refresh_token", "refreshToken"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((
        QoderCreds {
            access_token: token,
            refresh_token,
            expires_at_ms,
            kind: "client".into(),
            uid: uid.clone(),
            ..Default::default()
        },
        uid,
    ))
}

/// verifier 进 query 前的最小转义（verifier 实为双 uuid4 simple 拼接的 64 位纯 hex，
/// 正常不含任何需转义字符；此转义仅为防御性兜底，防 verifier 构造变化引入保留字）
fn urlencode(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20").replace('&', "%26").replace('=', "%3D").replace('~', "%7E")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_generates_distinct_sessions_and_pkce_url() {
        let f1 = DeviceFlow::new();
        let f2 = DeviceFlow::new();
        assert_ne!(f1.nonce, f2.nonce);
        assert_ne!(f1.verifier, f2.verifier);
        assert_eq!(f1.verifier.len(), 64);
        assert!(f1.auth_url.starts_with(DEVICE_AUTH_BASE));
        // query 参数完整性
        for key in ["challenge=", "challenge_method=S256", "nonce=", "machine_id=", "client_id="] {
            assert!(f1.auth_url.contains(key), "auth_url 缺少 {key}");
        }
        // challenge 可由 verifier 复算（S256 绑定）
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(f1.verifier.as_bytes()));
        assert!(f1.auth_url.contains(&format!("challenge={expect}")));
    }

    #[test]
    fn parse_poll_success_maps_captured_shape() {
        // R-10 抓包样本（token/refresh_token 已脱敏为占位）
        let body: Value = serde_json::from_str(
            r#"{"id":"01a0e03b-5f90-73b8-958e-443488ccf81e","token":"dt-placeholder","user_id":"01a0dff8-cc47-7b64-bd6d-d9cb2aea6792","code_challenge":"kiGczYiWCRH2Csra26Fx4NlhHp2n2CLT8QhMD678dqQ","code_challenge_method":"S256","nonce":"8872dcb1-d9c6-4453-bfb6-3bcbe9b0500b","expires_at":"2026-10-27T00:19:42Z","refresh_token_id":"01a0e03b-5f8f-7e4c-b112-1de3f25908cd","created_at":"2026-09-27T00:19:42Z","updated_at":"2026-09-27T00:19:42Z","refresh_token":"rt-placeholder","expires_in":2591999999,"refresh_token_expires_in":31103999999,"refresh_token_expires_at":"2027-09-22T00:19:42Z"}"#,
        )
        .unwrap();
        let (creds, uid) = parse_poll_success(&body).unwrap();
        assert_eq!(creds.access_token, "dt-placeholder");
        assert_eq!(creds.kind, "client");
        assert_eq!(uid, "01a0dff8-cc47-7b64-bd6d-d9cb2aea6792");
        // ≈30 天
        let now = chrono::Utc::now().timestamp_millis();
        let days = (creds.expires_at_ms.unwrap() - now) as f64 / 86_400_000.0;
        assert!((29.0..=31.0).contains(&days));
    }

    #[test]
    fn parse_poll_success_rejects_pending_shape() {
        let body: Value =
            serde_json::from_str(r#"{"errorCode":"NotFound","errorMessage":"Not found"}"#).unwrap();
        assert!(parse_poll_success(&body).is_none());
    }
}
