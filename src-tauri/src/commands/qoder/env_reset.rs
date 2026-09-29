//! Qoder M4 · 环境重置 / 彻底登出（对照 WorkBuddy F-14 同语义，粒度按语义块）。
//!
//! 8 项清理清单（映射 QODER_IDE_ITEMS 15 条文件级目标，switcher/icube.rs L54-71）：
//! 按「认证语义块」聚合而非逐文件，避免用户面对 15 个勾选项。执行顺序：
//! 关闭 Qoder CN（防占用与清理后回写）→ 按勾选项逐项清理（单项失败不中断）。
//! Qoder 无 SSO 注销对应物（凭证为本地 PAT/客户端存储），无 Keycloak 步骤。
//!
//! 指纹提示：清理 machine_identity / shared_client_cache 等于放弃当前设备身份，
//! 客户端下次启动将重新注册（可配合「账号绑定指纹」仍存于工具侧不受影响）。

use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::ide_data_dir;

fn cli_dir() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .ok()
        .map(|h| PathBuf::from(h).join(".qoder-cn"))
}

/// 8 项清单（id, label, detail）——存在性检查在命令层动态计算
fn qoder_reset_catalog() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("vscdb_auth", "登录令牌库", "删除 User\\globalStorage\\state.vscdb 及 -wal/-shm/.backup 边车（登录态真源，客户端启动重建）"),
        ("storage_json", "storage.json", "删除 User\\globalStorage\\storage.json（设备标识/遥测/认证信息）"),
        ("machine_identity", "机器身份文件", "删除根级 machineid / Local State / Preferences（设备指纹与窗口状态；Local State 含 vscdb 解密密钥，清理后残留 vscdb 登录密文不可解，客户端需重新登录）"),
        ("local_storage", "Local Storage", "删除 Local Storage\\leveldb（web 侧登录/偏好 KV）"),
        ("network_cookies", "Network Cookies", "删除 Network 目录（Cookie 等网络会话数据）"),
        ("session_storage", "Session Storage", "删除 Session Storage 目录（会话级 KV）"),
        ("shared_client_cache", "客户端身份四小件", "删除 SharedClientCache\\cache 下 id / machine_token.json / client.json / status.json（设备注册与激活状态）"),
        ("cli_auth", "CLI 数据目录", "删除 ~/.qoder-cn（R-3 侦察结论：当前无凭证落盘，清残留配置）"),
    ]
}

/// 带重试删除目录（Windows 文件占用场景：最多 3 次，间隔 400ms）；不存在返回 Ok(false)
fn force_rmtree(p: &Path) -> Result<bool, String> {
    if !p.exists() {
        return Ok(false);
    }
    let mut last = String::new();
    for _ in 0..3 {
        match std::fs::remove_dir_all(p) {
            Ok(()) => return Ok(true),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
    Err(format!("删除 {} 失败: {last}", p.display()))
}

/// 删除 dir 下指定文件名集合，返回实际删除个数
fn remove_files(dir: &Path, names: &[&str]) -> Result<usize, String> {
    let mut n = 0;
    for name in names {
        let f = dir.join(name);
        if f.exists() {
            std::fs::remove_file(&f).map_err(|e| format!("删除 {} 失败: {e}", f.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// 执行单个清理项，返回人类可读结果描述。
/// items 供关联项校验：machine_identity 删除 Local State 后残留 vscdb 登录密文
/// 不可解，未勾选 vscdb_auth 时追加联动提示（不自动连带删除，保持用户勾选语义）
fn run_reset_item(id: &str, items: &[String]) -> Result<String, String> {
    let base = ide_data_dir().ok_or("无法解析 %APPDATA%（QoderCN 数据目录不可用）")?;
    let gs = base.join("User").join("globalStorage");
    match id {
        "vscdb_auth" => {
            let n = remove_files(
                &gs,
                &["state.vscdb", "state.vscdb-wal", "state.vscdb-shm", "state.vscdb.backup"],
            )?;
            Ok(format!("已删除登录令牌库 {n}/4 个文件"))
        }
        "storage_json" => match remove_files(&gs, &["storage.json"])? {
            1 => Ok("已删除 storage.json".into()),
            _ => Ok("storage.json 不存在（跳过）".into()),
        },
        "machine_identity" => {
            let n = remove_files(&base, &["machineid", "Local State", "Preferences"])?;
            let mut detail = format!("已删除机器身份文件 {n}/3 个");
            if n > 0 && !items.iter().any(|i| i == "vscdb_auth") && gs.join("state.vscdb").exists() {
                detail.push_str(
                    "（注意：Local State 已删但「登录令牌库」未勾选，残留 state.vscdb 的解密密钥已丢失，客户端需重新登录；建议下次一并勾选）",
                );
            }
            Ok(detail)
        }
        "local_storage" => match force_rmtree(&base.join("Local Storage").join("leveldb"))? {
            true => Ok("已删除 Local Storage\\leveldb".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "network_cookies" => match force_rmtree(&base.join("Network"))? {
            true => Ok("已删除 Network 目录（Cookie）".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "session_storage" => match force_rmtree(&base.join("Session Storage"))? {
            true => Ok("已删除 Session Storage 目录".into()),
            false => Ok("目录不存在（跳过）".into()),
        },
        "shared_client_cache" => {
            let cache = base.join("SharedClientCache").join("cache");
            let n = remove_files(&cache, &["id", "machine_token.json", "client.json", "status.json"])?;
            Ok(format!("已删除客户端身份文件 {n}/4 个"))
        }
        "cli_auth" => match cli_dir() {
            Some(d) => match force_rmtree(&d)? {
                true => Ok("已删除 ~/.qoder-cn".into()),
                false => Ok("目录不存在（跳过）".into()),
            },
            None => Ok("无法解析 %USERPROFILE%（跳过）".into()),
        },
        _ => Err(format!("未知清理项: {id}")),
    }
}

#[derive(Serialize, Clone)]
pub struct QoderResetItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    pub exists: bool,
}

/// 环境重置清单：8 项 + 动态存在性标注（供 UI 勾选预览）
#[tauri::command]
pub fn qoder_env_reset_items() -> Vec<QoderResetItem> {
    let base = ide_data_dir();
    let gs = base.as_ref().map(|b| b.join("User").join("globalStorage"));
    qoder_reset_catalog()
        .iter()
        .map(|(id, label, detail)| {
            let in_gs = |name: &str| gs.as_ref().map(|g| g.join(name).exists()).unwrap_or(false);
            let in_base = |name: &str| base.as_ref().map(|b| b.join(name).exists()).unwrap_or(false);
            let exists = match *id {
                "vscdb_auth" => {
                    in_gs("state.vscdb") || in_gs("state.vscdb-wal") || in_gs("state.vscdb-shm") || in_gs("state.vscdb.backup")
                }
                "storage_json" => in_gs("storage.json"),
                "machine_identity" => in_base("machineid") || in_base("Local State") || in_base("Preferences"),
                "local_storage" => base.as_ref().map(|b| b.join("Local Storage").join("leveldb").is_dir()).unwrap_or(false),
                "network_cookies" => base.as_ref().map(|b| b.join("Network").is_dir()).unwrap_or(false),
                "session_storage" => base.as_ref().map(|b| b.join("Session Storage").is_dir()).unwrap_or(false),
                "shared_client_cache" => {
                    in_base("SharedClientCache")
                        && ["id", "machine_token.json", "client.json", "status.json"]
                            .iter()
                            .any(|f| base.as_ref().map(|b| b.join("SharedClientCache").join("cache").join(f).exists()).unwrap_or(false))
                }
                "cli_auth" => cli_dir().map(|d| d.is_dir()).unwrap_or(false),
                _ => false,
            };
            QoderResetItem {
                id: id.to_string(),
                label: label.to_string(),
                detail: detail.to_string(),
                exists,
            }
        })
        .collect()
}

/// 环境重置执行：关闭 Qoder CN → 按勾选项逐项清理（单项失败不中断）。
#[tauri::command(async)]
pub fn qoder_env_reset(
    app: AppHandle,
    state: State<AppState>,
    items: Vec<String>,
) -> Result<Vec<serde_json::Value>, String> {
    if items.is_empty() {
        return Err("未选择任何清理项".into());
    }
    let mut results: Vec<serde_json::Value> = vec![];

    // 关闭 Qoder CN（防数据目录占用与清理后回写）
    let _ = crate::commands::process::graceful_kill_app("Qoder CN");

    // 按勾选项执行（单项失败不中断其余项）
    for id in &items {
        match run_reset_item(id, &items) {
            Ok(detail) => results.push(serde_json::json!({ "id": id, "ok": true, "detail": detail })),
            Err(e) => results.push(serde_json::json!({ "id": id, "ok": false, "detail": e })),
        }
    }
    let ok_n = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let fail_n = results.len() - ok_n;
    fs_utils::app_log(
        &state.data_dir,
        &format!("qoder: 环境重置完成（{ok_n}/{} 项成功）", items.len()),
    );
    if fail_n > 0 {
        crate::commands::workbuddy::push_notify(
            Some(&app),
            &state.data_dir,
            "Qoder 环境重置",
            &format!("清理完成，{fail_n} 项失败，请查看详情"),
            crate::notify::NotifyEvent::Other,
        );
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_catalog_has_8_unique_ids() {
        let cat = qoder_reset_catalog();
        assert_eq!(cat.len(), 8);
        let mut ids: Vec<&str> = cat.iter().map(|(id, _, _)| *id).collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn remove_files_counts_only_existing() {
        let dir = std::env::temp_dir().join(format!("qoder_reset_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        let n = remove_files(&dir, &["a.json", "missing.json"]).unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join("a.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
