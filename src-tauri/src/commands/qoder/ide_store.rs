//! Qoder IDE 存储账号发现/导入（F-80 M3；R-8 侦察结论固化为 L1 凭证通道）。
//!
//! R-8 实测结论（2026-09-27 本机侦察，详见设计文档 R-8）：
//! - 登录态真源：`%APPDATA%\QoderCN\User\globalStorage\state.vscdb`
//!   ItemTable 键 `secret://aicoding.auth.userInfo`——TEXT 列存 JSON
//!   `{"type":"Buffer","data":[...]}`，前 3 字节 ASCII "v10"（Chromium os_crypt 形态）
//! - 解密链路：`%APPDATA%\QoderCN\Local State`（dataDir 根目录，非 User\globalStorage 下）
//!   → `os_crypt.encrypted_key`（base64 + DPAPI 包裹）→ AES-256-GCM 密钥 →
//!   'v10' + nonce(12) + ct + tag(16)
//! - 解密后 userInfo JSON：id（36 位 uid）/ token（dt- 前缀）/ refreshToken（drt- 前缀）/
//!   expireTime·refreshTokenExpireTime（13 位毫秒字符串）/ name / login_source="qodercn"
//! - CN 版无国际版 auth.v1.dat 形态（R-8 裁决：以 state.vscdb 通道为准）
//!
//! 快照完整性：switcher QODER_IDE_ITEMS 白名单含 Local State 与 state.vscdb（+WAL/SHM），
//! 快照恢复后密钥与密文同槽走，IDE 可自解密——本模块只读不改。
//!
//! 凭证红线：token 不进日志/事件/返回值；摘要只回 qd- id 与昵称。

use std::path::Path;

use serde::Serialize;
use tauri::State;

use super::common::{account_id_of, ide_data_dir, with_pool_mut, QoderAccount};
use crate::state::AppState;
use crate::tasks::qoder_common::{self, QoderCreds};
use crate::tasks::qoder_device::QoderDeviceProfile;

/// 登录态键（state.vscdb ItemTable）
const KEY_USER_INFO: &str = "secret://aicoding.auth.userInfo";

// ── 解密链路（对齐 device_proxy/local_capture.rs Chrome AES 模板）──────────

/// Local State（dataDir 根目录）→ os_crypt.encrypted_key → base64 → DPAPI → AES 密钥
#[cfg(windows)]
fn ide_aes_key(data_dir: &Path) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let lp = data_dir.join("Local State");
    let raw = std::fs::read_to_string(&lp).map_err(|e| format!("Local State 读取失败: {e}"))?;
    let state: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Local State 解析失败: {e}"))?;
    let b64 = state
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(serde_json::Value::as_str)
        .ok_or("Local State 无 os_crypt.encrypted_key")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("encrypted_key base64 解码失败: {e}"))?;
    if raw.len() < 5 || &raw[..5] != b"DPAPI" {
        return Err("encrypted_key 前缀非 DPAPI".into());
    }
    crate::vault::dpapi::unprotect(&raw[5..])
}

/// v10 密文解密：'v10' + nonce(12) + ct + tag(16) → AES-256-GCM
#[cfg(windows)]
fn decrypt_v10(enc: &[u8], key: &[u8]) -> Option<String> {
    if enc.len() < 19 || &enc[..3] != b"v10" {
        return None;
    }
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&enc[3..15]), &enc[15..])
        .ok()?;
    String::from_utf8(plain).ok()
}

/// secret:// 值形态：TEXT JSON `{"type":"Buffer","data":[...]}` → 字节
#[cfg(windows)]
fn parse_buffer(text: &str) -> Option<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.get("data")?.as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_u64().map(|n| n as u8))
            .collect::<Vec<u8>>()
    })
}

/// 读 state.vscdb ItemTable 单键（TEXT 优先，BLOB 按 UTF-8 解码；文件缺失 → Ok(None)）。
/// I16：返回 Result 区分「无键」与「读库失败」——IDE 运行中可能短暂锁定库文件，
/// busy_timeout 1.5s 缓解，打开失败消息标注可能原因，避免一律误报「未登录」。
/// 自建只读连接（switcher::vscdb::read_key 为私有且语义面向全局键合并，不复用）
#[cfg(windows)]
fn read_vscdb_key(vscdb: &Path, key: &str) -> Result<Option<String>, String> {
    if !vscdb.is_file() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open_with_flags(vscdb, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("state.vscdb 打开失败（可能被 IDE 占用）: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))
        .map_err(|e| format!("state.vscdb busy_timeout 设置失败: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT value FROM ItemTable WHERE key = ?1")
        .map_err(|e| format!("state.vscdb 查询准备失败: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![key])
        .map_err(|e| format!("state.vscdb 查询执行失败: {e}"))?;
    let row = match rows
        .next()
        .map_err(|e| format!("state.vscdb 读取行失败: {e}"))?
    {
        Some(r) => r,
        None => return Ok(None),
    };
    let val = row
        .get::<_, rusqlite::types::Value>(0)
        .map_err(|e| format!("state.vscdb 键值类型读取失败: {e}"))?;
    match val {
        rusqlite::types::Value::Text(s) => Ok(Some(s)),
        rusqlite::types::Value::Blob(b) => {
            String::from_utf8(b).map(Some).map_err(|e| format!("state.vscdb BLOB 非 UTF-8: {e}"))
        }
        _ => Ok(None),
    }
}

// ── 登录态读取 ─────────────────────────────────────────────────────────────

/// IDE 存储当前登录账号（解密产物；token 仅供导入通路，不落日志/事件）
#[cfg(windows)]
pub struct IdeLogin {
    pub uid: String,
    pub token: String,
    pub refresh_token: String,
    pub name: String,
    /// expireTime（13 位毫秒 → i64 毫秒）
    pub expires_at_ms: Option<i64>,
}

/// expireTime 提取：13 位毫秒时间戳（字符串/数字形态兼容）
#[cfg(windows)]
fn expire_ms_of(v: &serde_json::Value) -> Option<i64> {
    let raw = v.get("expireTime")?;
    raw.as_i64().or_else(|| raw.as_str()?.trim().parse::<i64>().ok())
}

/// 读 IDE 存储当前登录态（未登录/解密失败 → Err，描述脱敏不含凭证）
#[cfg(windows)]
pub fn scan_ide_login(data_dir: &Path) -> Result<IdeLogin, String> {
    let key = ide_aes_key(data_dir)?;
    let vscdb = data_dir.join("User").join("globalStorage").join("state.vscdb");
    let raw = read_vscdb_key(&vscdb, KEY_USER_INFO)?
        .ok_or("state.vscdb 无登录态（可能未登录或已清除）")?;
    let bytes = parse_buffer(&raw).ok_or("登录态键值形态不识别")?;
    let plain = decrypt_v10(&bytes, &key).ok_or("登录态解密失败（密钥或密文不匹配）")?;
    let v: serde_json::Value =
        serde_json::from_str(&plain).map_err(|e| format!("userInfo 解析失败: {e}"))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Ok(IdeLogin {
        uid: s("id"),
        token: s("token"),
        refresh_token: s("refreshToken"),
        name: s("name"),
        expires_at_ms: expire_ms_of(&v),
    })
}

// ── 扫描/导入命令 ──────────────────────────────────────────────────────────

/// 扫描结果摘要（脱敏：不含 token 本体）
#[derive(Serialize, Clone, Default)]
pub struct QoderIdeScanResult {
    pub found: bool,
    /// true = 新入池；false = 已在池中更新
    pub imported: bool,
    pub updated: bool,
    pub account_id: String,
    pub nickname: String,
    pub reason: String,
}

/// 扫描 IDE 存储当前登录账号并导入账号池（幂等：同 token 稳定同 id，重复=更新）。
/// 凭证入 token store（kind=client，dt-/drt- 原样入 store；effective_creds 请求侧
/// 仍按 §5.10 注入每账号绑定指纹——IDE 本机指纹不入 store，避免多账号共享单指纹）。
#[tauri::command]
pub fn qoder_ide_scan(state: State<AppState>) -> Result<QoderIdeScanResult, String> {
    #[cfg(windows)]
    {
        let data_dir = ide_data_dir().ok_or("无法定位 QoderCN 数据目录（APPDATA 缺失）")?;
        let login = match scan_ide_login(&data_dir) {
            Ok(l) => l,
            Err(e) => {
                return Ok(QoderIdeScanResult {
                    found: false,
                    reason: e,
                    ..Default::default()
                })
            }
        };
        if login.token.is_empty() {
            return Ok(QoderIdeScanResult {
                found: false,
                reason: "登录态中无 token".into(),
                ..Default::default()
            });
        }
        let id = account_id_of(&login.token);
        // I09：持锁读-改-写，防并发整池覆盖丢更新
        let (updated, nickname) = with_pool_mut(&state, |accounts| {
            if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
                // 已有账号保守回填：uid/nickname 只在为空时补，不覆盖用户手动改名
                if a.uid.is_empty() && !login.uid.is_empty() {
                    a.uid = login.uid.clone();
                }
                if a.nickname.is_empty() && !login.name.is_empty() {
                    a.nickname = login.name.clone();
                }
                a.credential_source = "ide_store".into();
                a.token_expires_at = login.expires_at_ms.map(|ms| ms / 1000);
                a.needs_relogin = false;
                a.relogin_reason = String::new();
                if a.device_profile.is_none() {
                    a.device_profile = Some(QoderDeviceProfile::generate());
                }
                Ok((true, a.nickname.clone()))
            } else {
                let nickname = if login.name.is_empty() {
                    format!("Qoder {}", &id[3..9])
                } else {
                    login.name.clone()
                };
                accounts.push(QoderAccount {
                    id: id.clone(),
                    uid: login.uid.clone(),
                    nickname: nickname.clone(),
                    credential_source: "ide_store".into(),
                    token_expires_at: login.expires_at_ms.map(|ms| ms / 1000),
                    // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                    device_profile: Some(QoderDeviceProfile::generate()),
                    ..Default::default()
                });
                Ok((false, nickname))
            }
        })?;
        // 凭证入 token store（save_token_store 非空字段 merge，不抹掉存量字段）
        let creds = QoderCreds {
            access_token: login.token,
            refresh_token: login.refresh_token,
            expires_at_ms: login.expires_at_ms,
            uid: login.uid,
            nickname: login.name,
            kind: "client".into(),
            ..Default::default()
        };
        qoder_common::save_token_store(&state, &id, &creds)?;
        crate::fs_utils::app_log(&state.data_dir, &format!("Qoder IDE 存储账号已发现并导入: {id}"));
        Ok(QoderIdeScanResult {
            found: true,
            imported: !updated,
            updated,
            account_id: id,
            nickname,
            reason: String::new(),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = &state;
        Err("IDE 存储发现仅支持 Windows（DPAPI + AES-GCM）".into())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn parse_buffer_解析与非json容错() {
        assert_eq!(
            parse_buffer(r#"{"type":"Buffer","data":[118,49,48]}"#).unwrap(),
            b"v10".to_vec()
        );
        assert!(parse_buffer("not-json").is_none());
        assert!(parse_buffer(r#"{"type":"Buffer"}"#).is_none());
    }

    #[test]
    fn v10_加解密往返与错钥失败() {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        let key = [7u8; 32];
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let nonce = Nonce::from_slice(&[9u8; 12]);
        let ct = cipher.encrypt(nonce, b"{\"id\":\"u1\"}" as &[u8]).unwrap();
        let mut enc = b"v10".to_vec();
        enc.extend_from_slice(nonce);
        enc.extend_from_slice(&ct);
        assert_eq!(decrypt_v10(&enc, &key).unwrap(), "{\"id\":\"u1\"}");
        assert!(decrypt_v10(&enc, &[8u8; 32]).is_none(), "错误密钥必须失败");
        assert!(decrypt_v10(b"v10", &key).is_none());
        assert!(decrypt_v10(b"DPx0\x01\x02", &key).is_none(), "非 v10 前缀拒绝");
    }

    #[test]
    fn vscdb读键_text与blob与缺文件() {
        let p = std::env::temp_dir().join(format!(
            "f80-ide-{}-{}.vscdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_file(&p);
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")
            .unwrap();
        conn.execute(
            "INSERT INTO ItemTable VALUES(?1, ?2)",
            rusqlite::params![KEY_USER_INFO, r#"{"type":"Buffer","data":[1]}"#],
        )
        .unwrap();
        let blob_val: Vec<u8> = b"from-blob".to_vec();
        conn.execute(
            "INSERT INTO ItemTable VALUES(?1, ?2)",
            rusqlite::params!["secret://other.key", blob_val],
        )
        .unwrap();
        assert_eq!(
            read_vscdb_key(&p, KEY_USER_INFO).unwrap().as_deref(),
            Some(r#"{"type":"Buffer","data":[1]}"#)
        );
        assert_eq!(
            read_vscdb_key(&p, "secret://other.key").unwrap().as_deref(),
            Some("from-blob")
        );
        assert_eq!(read_vscdb_key(&p, "secret://missing.key").unwrap(), None);
        assert_eq!(
            read_vscdb_key(&std::env::temp_dir().join("f80-nope.vscdb"), KEY_USER_INFO).unwrap(),
            None
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn expire_time_字符串与数字形态兼容() {
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":"1791673619906"}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_906));
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":1791673619906}"#).unwrap();
        assert_eq!(expire_ms_of(&v), Some(1_791_673_619_906));
        let v: serde_json::Value = serde_json::from_str(r#"{"expireTime":"abc"}"#).unwrap();
        assert_eq!(expire_ms_of(&v), None);
        let v: serde_json::Value = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(expire_ms_of(&v), None);
    }
}
