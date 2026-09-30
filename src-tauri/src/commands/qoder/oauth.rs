//! Qoder OAuth 设备流命令（F-80；R-10 抓包固化）：
//! 浏览器打开授权页（qoder.cn/device/selectAccounts）→ 轮询 deviceToken/poll
//! → dt- 令牌入池。事件契约对齐 wb-oauth（qoder-oauth-progress / qoder-oauth-done）。

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::qoder_oauth::{self, DeviceFlow};
use crate::tasks::{http_agent, qoder_common};

use super::common::{account_id_of, with_pool_mut, QoderAccount};

/// OAuth 防重入（与 wb oauth 同款 AtomicBool；RAII guard 保证异常路径复位）
static OAUTH_RUNNING: AtomicBool = AtomicBool::new(false);
/// 取消标志（用户在弹框点「取消授权」）：轮询线程检测到即发失败终态并退出
static OAUTH_CANCEL: AtomicBool = AtomicBool::new(false);

struct OAuthGuard;
impl Drop for OAuthGuard {
    fn drop(&mut self) {
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 系统浏览器打开 URL（复用 wb oauth 同款实现：cmd /c start + raw_arg 防 & 截断）
#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.contains(['"', '\'', ' ']) {
        return Err(format!("拒绝打开非法 URL：{url}"));
    }
    Command::new("cmd")
        .arg("/c")
        .raw_arg(format!("start \"\" \"{url}\""))
        .creation_flags(0x08000000)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败: {e}"))
}

/// 非 Windows 占位（cmd/raw_arg/creation_flags 为 Windows 专属，逐函数门控
/// 对齐 common.rs is_running 惯例）
#[cfg(not(windows))]
fn open_in_browser(_url: &str) -> Result<(), String> {
    Err("打开浏览器仅支持 Windows".into())
}

fn emit_progress(app: &AppHandle, stage: &str, message: &str, auth_url: Option<&str>) {
    let _ = app.emit(
        "qoder-oauth-progress",
        json!({ "stage": stage, "message": message, "auth_url": auth_url }),
    );
}

/// 终态事件：emit 失败落日志（issue #44 约定对齐，此前 `let _` 静默吞错——
/// 前端 oauthRunning 弹窗将永挂且无任何日志线索）
fn emit_done(app: &AppHandle, data_dir: &Path, ok: bool, id: &str, nickname: &str, message: &str) {
    crate::events::emit_logged(
        app,
        "qoder-oauth-done",
        json!({ "ok": ok, "id": id, "nickname": nickname, "message": message }),
        Some(data_dir),
    );
}

/// 发起 Qoder OAuth 设备流登录：
/// ① 构造 PKCE 会话并打开授权页 → ② 后台线程 1s 轮询（404=pending，200=授权完成）
/// → ③ dt- 令牌入池（幂等：同 token 稳定同 id）并回填 uid/昵称/套餐。
#[tauri::command(async)]
pub fn qoder_oauth_login(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if OAUTH_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("已有 OAuth 登录在执行中，请等待完成".into());
    }
    // 复位取消标志（上一轮会话的取消请求不应影响本次登录）
    OAUTH_CANCEL.store(false, Ordering::SeqCst);
    let flow = DeviceFlow::new();
    let auth_url = flow.auth_url.clone();
    emit_progress(&app, "init", "正在打开 Qoder 授权页…", Some(&auth_url));
    if let Err(e) = open_in_browser(&auth_url) {
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
        return Err(e);
    }
    emit_progress(&app, "browser", "请在浏览器中完成 Qoder 账号授权（登录并确认）", Some(&auth_url));

    let app2 = app.clone();
    let state2 = state.inner().clone();
    // flow 整体移交工作线程（nonce/verifier 会话一致性）
    // I17：命名线程；spawn 失败必须复位防重入标志，否则后续登录永久被拒
    let spawned = std::thread::Builder::new()
        .name("qoder-oauth".into())
        .spawn(move || {
        let _guard = OAuthGuard;
        // panic 不外泄线程：捕获后补发失败终态（OAUTH_RUNNING 由 OAuthGuard drop 复位），
        // 否则前端 oauth 弹窗运行态永挂
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let agent = http_agent(15);
            emit_progress(&app2, "polling", "等待授权完成…", Some(&auth_url));
            let started = std::time::Instant::now();
            loop {
                if started.elapsed().as_millis() as u64 > qoder_oauth::POLL_TIMEOUT_MS {
                    fs_utils::app_log(&state2.data_dir, "qoder OAuth 登录超时：180s 内未完成授权");
                    emit_done(&app2, &state2.data_dir, false, "", "", "授权超时：请在浏览器完成授权后重试");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(qoder_oauth::POLL_INTERVAL_MS));
                if OAUTH_CANCEL.load(Ordering::SeqCst) {
                    fs_utils::app_log(&state2.data_dir, "qoder OAuth 登录已由用户取消");
                    emit_done(&app2, &state2.data_dir, false, "", "", "已取消授权");
                    return;
                }
                let (status, body) = qoder_oauth::poll_once(&agent, &flow);
                match status {
                    // pending：尚未授权（R-10 实测 404 NotFound）
                    404 => continue,
                    200 => {
                        let Some(b) = body else {
                            emit_done(&app2, &state2.data_dir, false, "", "", "授权响应非 JSON，请重试");
                            return;
                        };
                        // nonce 回验（审查 L）：响应必须属于本次会话，防 poll 响应
                        // 被替换为其他会话的授权结果
                        let Some((creds, uid)) = qoder_oauth::parse_poll_success(&b, &flow.nonce) else {
                            emit_done(&app2, &state2.data_dir, false, "", "", "授权响应校验失败（nonce 不匹配或缺少令牌字段），请重试");
                            return;
                        };
                        match import_device_creds(&state2, creds, &uid) {
                            Ok((id, nickname)) => {
                                fs_utils::app_log(&state2.data_dir, &format!("qoder OAuth 登录成功: {id}"));
                                emit_done(
                                    &app2,
                                    &state2.data_dir,
                                    true,
                                    &id,
                                    &nickname,
                                    &format!("授权成功，账号 {nickname} 已入池"),
                                );
                            }
                            Err(e) => emit_done(&app2, &state2.data_dir, false, "", "", &format!("凭证入库失败: {e}")),
                        }
                        return;
                    }
                    // 授权会话过期/被撤销等异常状态：立即终止（避免轮询轰炸）
                    400 | 401 | 403 | 410 => {
                        let msg = match body
                            .as_ref()
                            .and_then(|b| crate::fs_utils::dig(b, &["errorMessage", "error_message", "message"]))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                        {
                            "" => format!("授权会话失效（HTTP {status}），请重新发起登录"),
                            m => format!("授权失败（HTTP {status}）：{m}"),
                        };
                        fs_utils::app_log(&state2.data_dir, &format!("qoder OAuth 终止: {msg}"));
                        emit_done(&app2, &state2.data_dir, false, "", "", &msg);
                        return;
                    }
                    // 网络抖动等其他状态：继续轮询直至超时
                    _ => continue,
                }
            }
        }));
        if result.is_err() {
            fs_utils::app_log(&state2.data_dir, "qoder OAuth 线程 panic（已捕获，补发失败终态）");
            emit_done(&app2, &state2.data_dir, false, "", "", "OAuth 登录线程异常终止，请重试");
        }
    });
    if spawned.is_err() {
        OAUTH_RUNNING.store(false, Ordering::SeqCst);
        return Err("OAuth 后台线程启动失败，请重试".into());
    }
    Ok(())
}

/// 取消进行中的 OAuth 轮询（弹框「取消授权」）：置标志后轮询线程自行收尾。
#[tauri::command(async)]
pub fn qoder_oauth_cancel() -> Result<(), String> {
    OAUTH_CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

/// 设备流凭证入库：幂等入池（uid 优先匹配，防重复）+ token store + userinfo/plan 回填。
/// 返回 (id, 展示名)。命中已有账号保留原 id（I10：换发派生 id 漂移会使
/// 快照/分组/外部引用悬空，对照蓝本 merge_auth_entry 的取舍）。
fn import_device_creds(
    state: &AppState,
    mut creds: qoder_common::QoderCreds,
    uid: &str,
) -> Result<(String, String), String> {
    let id = account_id_of(&creds.access_token);
    // userinfo/plan 回填（失败容错：dt- 对 openapi 端点的可用性随客户端一致）
    let agent = http_agent(15);
    let probe = qoder_common::QoderCreds {
        access_token: creds.access_token.clone(),
        kind: "client".into(),
        ..Default::default()
    };
    let (info_uid, nickname) = qoder_common::fetch_userinfo(&agent, &probe);
    let tier = qoder_common::fetch_plan(&agent, &probe).0.unwrap_or_default();
    let uid = match info_uid.filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => uid.to_string(),
    };
    creds.uid = uid.clone();
    creds.nickname = nickname.clone().unwrap_or_default();

    let display = if let Some(n) = nickname.filter(|n| !n.is_empty()) {
        n
    } else if !uid.is_empty() {
        uid.chars().take(12).collect()
    } else {
        format!("Qoder {}", &id[3..9])
    };

    // 先持锁入池拿到最终 id，再写 token store（I10：避免先写凭证后改 id 的孤儿记录）
    let final_id = with_pool_mut(state, |accounts| {
        if let Some(a) = accounts
            .iter_mut()
            .find(|a| a.id == id || (!uid.is_empty() && a.uid == uid))
        {
            // 原位更新并保留原 id
            if a.nickname.is_empty() {
                a.nickname = display.clone();
            }
            if !tier.is_empty() {
                a.plan = tier;
            }
            a.credential_source = "client".into();
            // uid 空时不清空既有绑定（userinfo 失败且设备流未返回 uid 的降级场景）
            if !uid.is_empty() {
                a.uid = uid.clone();
            }
            a.needs_relogin = false;
            a.relogin_reason = String::new();
            // 指纹回填（幂等：已有稳定绑定不覆盖，§5.10）
            if a.device_profile.is_none() {
                a.device_profile = Some(crate::tasks::qoder_device::QoderDeviceProfile::generate());
            }
            Ok(a.id.clone())
        } else {
            accounts.push(QoderAccount {
                id: id.clone(),
                uid: uid.clone(),
                nickname: display.clone(),
                plan: tier,
                credential_source: "client".into(),
                // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                device_profile: Some(crate::tasks::qoder_device::QoderDeviceProfile::generate()),
                ..Default::default()
            });
            Ok(id.clone())
        }
    })?;
    qoder_common::save_token_store(state, &final_id, &creds)?;
    Ok((final_id, display))
}
