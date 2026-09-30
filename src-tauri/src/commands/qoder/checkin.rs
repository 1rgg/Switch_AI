//! Qoder 签到域（F-80 M1，对照 commands/workbuddy/checkin.rs 模式）：
//! 签到（NDJSON 管线）/ 签到结果 / 每日定时任务（schtasks 双轨）/ 启动自动补签。

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::tasks::qoder_checkin::{self, QoderCheckinOpts};

use super::common::load_settings;

#[derive(serde::Deserialize)]
pub struct QoderCheckinOptsDto {
    #[serde(default)]
    pub user_ids: Option<Vec<String>>,
    #[serde(default)]
    pub skip_checked_in: bool,
    #[serde(default)]
    pub lazy_hours: Option<i64>,
}

/// NDJSON 事件转发（与 wb 管线同款：emit 序列化 JSON 字符串，前端逐行 JSON.parse）
fn emit_qoder_event(app: &AppHandle, ev: &Value) {
    if let Ok(line) = serde_json::to_string(ev) {
        let _ = app.emit("qoder-checkin-progress", &line);
    }
}

/// 发起 Qoder 签到（轮次锁互斥 + 工作线程执行 + exit 收尾事件）
#[tauri::command(async)]
pub fn qoder_checkin_start(
    app: AppHandle,
    state: State<AppState>,
    opts: QoderCheckinOptsDto,
) -> Result<(), String> {
    let round = qoder_checkin::try_acquire_qoder_round()?;
    let o = QoderCheckinOpts {
        uids: opts.user_ids.unwrap_or_default(),
        skip_checked: opts.skip_checked_in,
        lazy_hours: opts.lazy_hours.unwrap_or(24),
    };
    let app2 = app.clone();
    let state2 = state.inner().clone();
    std::thread::spawn(move || {
        let _guard = round;
        // panic 不外泄线程：捕获后记录，exit 终态照常下发（否则前端运行态永挂）
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            qoder_checkin::run_checkin_round(&state2, &o, &mut |ev| emit_qoder_event(&app2, ev));
        }));
        if result.is_err() {
            fs_utils::app_log(&state2.data_dir, "Qoder 签到轮次线程 panic（已捕获，exit 终态仍下发）");
        }
        // 终态事件（前端据 "type":"exit" 复位运行态）：emit 失败落日志（issue #44 约定对齐）。
        // 契约同 wb：NDJSON **字符串** payload（listen<string> 后 JSON.parse），传对象会
        // 破坏 parseLine 导致 exit 事件被静默丢弃
        let line = serde_json::json!({ "type": "exit", "ok": true }).to_string();
        crate::events::emit_logged(
            &app2,
            "qoder-checkin-progress",
            serde_json::Value::String(line),
            Some(state2.data_dir.as_path()),
        );
    });
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct QoderCheckinRecord {
    pub date: String,
    pub time: String,
    pub user_id: String,
    pub name: String,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<f64>,
}

/// 签到日志（90 天存储，UI 默认展示 30 天）
#[tauri::command]
pub fn qoder_checkin_results(
    state: State<AppState>,
    days: Option<i64>,
) -> Result<Vec<QoderCheckinRecord>, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let raw: Value = crate::store::docs::qoder_checkin_results_load(&crate::store::db(&state.data_dir));
    let mut out = Vec::new();
    if let Some(arr) = raw.get("results").and_then(|v| v.as_array()) {
        for r in arr {
            let date = r.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if date < cutoff.as_str() {
                continue;
            }
            out.push(QoderCheckinRecord {
                date: date.into(),
                time: r.get("time").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_id: r.get("user_id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").into(),
                message: r.get("message").and_then(|v| v.as_str()).unwrap_or("").into(),
                reward: r.get("reward").and_then(|v| v.as_f64()),
            });
        }
    }
    out.reverse(); // 新→旧
    Ok(out)
}

// ── 每日签到定时任务（schtasks 双轨；对照 wb_checkin_task_register）─────────

const QODER_CHECKIN_TASK_PREFIX: &str = "AIWorkAssistant_QoderCheckin";

fn build_qoder_task_tr(state: &AppState, task: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    let data_dir = state.data_dir.to_string_lossy().to_string();
    Ok(format!(
        "cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" --task-run {}",
        data_dir,
        exe.to_string_lossy(),
        task
    ))
}

fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    crate::commands::misc::run_schtasks(args)
}

/// 枚举当前用户可见的计划任务名（列序不跨机固定，扫描各行 TaskName 字段；
/// 与 commands/workbuddy/checkin.rs::parse_task_names_from_csv 同款解析，独立副本避免跨域耦合）
fn parse_task_names_from_csv(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        for field in line.split(',') {
            let f = field.trim().trim_matches('"');
            let Some(name) = f.strip_prefix('\\') else { continue };
            let name = name.rsplit('\\').next().unwrap_or(name);
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn qoder_checkin_task_names() -> Vec<String> {
    let prefix = format!("{QODER_CHECKIN_TASK_PREFIX}_");
    match run_schtasks(&["/Query", "/FO", "CSV", "/NH"]) {
        Ok((_, stdout, _)) => parse_task_names_from_csv(&stdout)
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .collect(),
        Err(_) => vec![],
    }
}

/// 删除单个计划任务：run_schtasks 三态归一为 Result（Ok(true)=成功；
/// Ok(false) 取 stderr，Err 透传）——替代原 `let _ = run_schtasks(...)` 静默吞错
fn delete_task_checked(name: &str) -> Result<(), String> {
    match run_schtasks(&["/Delete", "/TN", name, "/F"]) {
        Ok((true, _, _)) => Ok(()),
        Ok((false, _, stderr)) => {
            let msg = stderr.trim().to_string();
            Err(if msg.is_empty() { "schtasks 返回失败".into() } else { msg })
        }
        Err(e) => Err(e),
    }
}

#[tauri::command(async)]
pub fn qoder_checkin_task_register(state: State<AppState>, times: Vec<String>) -> Result<(), String> {
    if times.is_empty() {
        return Err("至少需要一个触发时间（如 10:15）".into());
    }
    for t in &times {
        crate::commands::misc::validate_hhmm(t)?;
    }
    let tr = build_qoder_task_tr(&state, "qoder-checkin")?;
    // 先建后删：/Create /F 直接覆盖同名旧任务，全部创建成功后再清理不在新集合内的
    // 旧任务——原实现先删后建，创建中途失败会导致旧调度已被删除（定时签到整体丢失）
    let mut new_names: Vec<String> = Vec::with_capacity(times.len());
    for t in &times {
        let hhmm = t.replace(':', "");
        let name = format!("{QODER_CHECKIN_TASK_PREFIX}_{hhmm}");
        let (ok, _, stderr) =
            run_schtasks(&["/Create", "/TN", &name, "/TR", &tr, "/SC", "DAILY", "/ST", t, "/F"])?;
        if !ok {
            // 先建后删流：此处失败时旧任务尚未清理，仍按原时间正常触发
            return Err(format!(
                "注册任务 {t} 失败: {}（原任务未被改动，仍按原时间正常触发；可重试本操作）",
                stderr.trim()
            ));
        }
        new_names.push(name);
    }
    for name in qoder_checkin_task_names() {
        if !new_names.contains(&name) {
            if let Err(e) = delete_task_checked(&name) {
                fs_utils::app_log(&state.data_dir, &format!("清理旧签到任务 {name} 失败: {e}"));
                return Err(format!(
                    "新任务已注册成功，但清理旧任务 {name} 失败（残留任务会重复触发签到）: {e}"
                ));
            }
        }
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("Qoder 每日签到定时任务已注册: {}", times.join(" / ")),
    );
    Ok(())
}

#[tauri::command(async)]
pub fn qoder_checkin_task_status() -> Result<Vec<String>, String> {
    let prefix = format!("{QODER_CHECKIN_TASK_PREFIX}_");
    // I15：register 侧存的是 replace(':',"") 后的 4 位数字任务名（如 1015），
    // 需还原为 HH:MM；通用 '_'→':' 替换对其恒 no-op，前端会显示成 1015
    Ok(qoder_checkin_task_names()
        .iter()
        .map(|name| match name.strip_prefix(&prefix) {
            Some(d) if d.len() == 4 && d.chars().all(|c| c.is_ascii_digit()) => {
                format!("{}:{}", &d[..2], &d[2..])
            }
            _ => name.trim_start_matches(&prefix).to_string(),
        })
        .collect())
}

#[tauri::command(async)]
pub fn qoder_checkin_task_unregister(state: State<AppState>) -> Result<(), String> {
    // 删除失败如实反馈（残留任务会继续触发签到），全部成功才记「已注销」；
    // 原实现 `let _ =` 静默吞错，用户以为已注销实际任务仍在跑
    let mut failed: Vec<String> = Vec::new();
    for name in qoder_checkin_task_names() {
        if let Err(e) = delete_task_checked(&name) {
            fs_utils::app_log(&state.data_dir, &format!("注销签到任务 {name} 失败: {e}"));
            failed.push(format!("{name}: {e}"));
        }
    }
    if !failed.is_empty() {
        return Err(format!("部分签到任务注销失败: {}", failed.join("；")));
    }
    fs_utils::app_log(&state.data_dir, "Qoder 每日签到定时任务已注销");
    Ok(())
}

// ── 启动自动补签（F-55 模式；main.rs setup 调用）───────────────────────────

/// 启动自动补签核心：延迟 60s + 轮次锁互斥；未签自动补签，静默执行零打扰。
pub fn startup_auto_checkin(app: &AppHandle, state: &AppState) {
    let s = load_settings(state);
    if !s.auto_checkin {
        return;
    }
    let app2 = app.clone();
    let state2 = state.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let Ok(_round) = qoder_checkin::try_acquire_qoder_round() else {
            fs_utils::app_log(&state2.data_dir, "Qoder 启动补签跳过：已有签到任务在执行中");
            return;
        };
        let opts = QoderCheckinOpts::daily();
        fs_utils::app_log(&state2.data_dir, "Qoder 启动补签：开始核验签到状态");
        let done = qoder_checkin::run_checkin_round(&state2, &opts, &mut |_| {});
        let msg = format!(
            "Qoder 启动补签完成: 成功 {}，已签 {}，失败 {}",
            done["ok"].as_i64().unwrap_or(0),
            done["already"].as_i64().unwrap_or(0),
            done["failed"].as_i64().unwrap_or(0),
        );
        fs_utils::app_log(&state2.data_dir, &msg);
        let failed = done["failed"].as_i64().unwrap_or(0);
        // empty_campaigns（活动未开始/不可用）非用户可操作失败：重试也无解，仅记日志
        // 不推送打扰（审查 L；done.failed_empty_campaigns 由 run_checkin_round 单列）
        let failed_actionable =
            failed - done["failed_empty_campaigns"].as_i64().unwrap_or(0);
        if failed_actionable > 0 {
            crate::commands::workbuddy::push_notify(
                Some(&app2),
                &state2.data_dir,
                "Qoder 签到提醒",
                &format!("启动补签有 {failed_actionable} 个账号失败，请在 Qoder 签到页查看"),
                crate::notify::NotifyEvent::TaskFail,
            );
        }
    });
}
