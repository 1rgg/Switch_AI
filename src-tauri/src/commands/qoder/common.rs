//! Qoder 共享底层（F-80 M1）：账号池/设置读写、账号 id、环境检测。

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tauri::State;

use crate::state::AppState;

// ── 数据结构（§5.4 数据模型；对照 WorkBuddyAccount 裁剪）────────────────────

/// 账号池记录。id = "qd-" + sha256(token)[..12]（同 token 稳定同 id，防换发重复入池）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderAccount {
    #[serde(default)]
    pub id: String,
    /// 全家桶 uid（/api/v1/userinfo 回填；PAT 导入时 userinfo 失败可空）
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 手机号掩码（userinfo 未提供时留空，M1 不做猜测）
    #[serde(default)]
    pub phone_masked: String,
    /// free | pro | pro+ | teams（只读提示）；userinfo 未提供时留空
    #[serde(default)]
    pub plan: String,
    /// 凭证来源：pat | ide_store | qoderwork_store | mitm | cli（M1 实装 pat）
    #[serde(default)]
    pub credential_source: String,
    /// token 过期时间（Unix 秒；PAT 长期凭证为 None）
    #[serde(default)]
    pub token_expires_at: Option<i64>,
    #[serde(default)]
    pub needs_relogin: bool,
    #[serde(default)]
    pub relogin_reason: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// 余额缓存（credits_fetch 成功后回写）
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
    /// 每账号稳定设备指纹（§5.10 多账号并发；入池时生成一次永不轮换，
    /// 存量账号经 ensure_pool_profiles 惰性回填）
    #[serde(default)]
    pub device_profile: Option<crate::tasks::qoder_device::QoderDeviceProfile>,
}

/// Qoder 设置（kv `qoder_settings`；对照 WorkBuddySettings 裁剪）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderSettings {
    /// 启动自动补签 + 应用内调度器 qoder-checkin 启用判定（默认开，§5.7）
    #[serde(default = "default_true")]
    pub auto_checkin: bool,
    /// 多账号签到显式开启项（合规 §七-1：默认关；固定条款风险提示）
    #[serde(default)]
    pub multi_account_enabled: bool,
}

fn default_true() -> bool {
    true
}

// ── 账号池 / 设置读写 ───────────────────────────────────────────────────────

pub(crate) fn load_pool(state: &AppState) -> Vec<QoderAccount> {
    // 只读路径容错：逐行解析，损坏行跳过（原实现整组解析失败 → 静默空池）
    load_pool_rows(state).0
}

/// 逐行解析池：返回 (正常账号, 损坏行原始值)。写入路径必须走 load_pool_checked
/// 拒绝损坏池，避免「load 丢行 → save 整池覆盖」静默永久丢账号。
fn load_pool_rows(state: &AppState) -> (Vec<QoderAccount>, Vec<serde_json::Value>) {
    let v = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let Some(rows) = v.get("accounts").and_then(|a| a.as_array()) else {
        return (Vec::new(), Vec::new());
    };
    let mut ok = Vec::new();
    let mut corrupt = Vec::new();
    for r in rows {
        match serde_json::from_value::<QoderAccount>(r.clone()) {
            Ok(a) => ok.push(a),
            Err(_) => corrupt.push(r.clone()),
        }
    }
    (ok, corrupt)
}

/// 严格版池读（供 with_pool_mut 写路径把关）：存在损坏行时先把损坏行原文备份到
/// data_dir/qoder_pool.corrupt.json，再拒绝返回——整池覆盖前必须显式处理。
pub(crate) fn load_pool_checked(state: &AppState) -> Result<Vec<QoderAccount>, String> {
    let (accounts, corrupt) = load_pool_rows(state);
    if corrupt.is_empty() {
        return Ok(accounts);
    }
    let backup = state.data_dir.join("qoder_pool.corrupt.json");
    let _ = crate::fs_utils::write_json(&backup, &serde_json::json!({ "corrupt_rows": corrupt }));
    crate::fs_utils::app_log(
        &state.data_dir,
        "qoder 账号池存在损坏行，已备份到 qoder_pool.corrupt.json，拒绝整池覆盖以防丢账号",
    );
    Err(
        "账号池数据存在损坏行，已备份到 qoder_pool.corrupt.json；本次修改已取消以保护其余账号，请处理备份文件后重试"
            .into(),
    )
}

pub(crate) fn save_pool(state: &AppState, accounts: &[QoderAccount]) -> Result<(), String> {
    let v = serde_json::to_value(accounts).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::qoder_pool_save(
        &crate::store::db(&state.data_dir),
        &serde_json::json!({ "accounts": v }),
    )
}

/// 池读-改-写互斥执行（F-80 I09）：OAuth/PAT/IDE 导入、改名/移除、签到回写、
/// 指纹回填等路径并发时整池覆盖会丢更新，统一经此函数持锁执行。
/// 注意：tasks 层 sync_pool_expiry/ensure_pool_profiles 直操原始 JSON 保留未知字段，
/// 不可走本函数（会解析成 Vec<QoderAccount> 丢字段），须自行持 state.qoder_pool_lock。
pub(crate) fn with_pool_mut<T>(
    state: &AppState,
    f: impl FnOnce(&mut Vec<QoderAccount>) -> Result<T, String>,
) -> Result<T, String> {
    // 中毒锁恢复（into_inner）：单次 panic 不应永久阻塞后续池写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut accounts = load_pool_checked(state)?;
    let out = f(&mut accounts)?;
    save_pool(state, &accounts)?;
    Ok(out)
}

pub(crate) fn load_settings(state: &AppState) -> QoderSettings {
    crate::store::db(&state.data_dir).kv_get("qoder_settings")
}

#[tauri::command]
pub fn qoder_settings_get(state: State<AppState>) -> QoderSettings {
    load_settings(&state)
}

#[tauri::command]
pub fn qoder_settings_set(state: State<AppState>, patch: QoderSettings) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("qoder_settings", &patch)
}

// ── 账号 id（qd- + sha256[..12]，单一实现在 tasks::qoder_common，此处 re-export）──

pub(crate) use crate::tasks::qoder_common::account_id_of;

/// 账号绑定 machine_id（F-80 §5.10.2 本地存储覆写取值源）：账号池按 id 查
/// device_profile.machine_id；无档案或值为空 → None（调用方跳过覆写）
pub(crate) fn machine_id_of(state: &AppState, account_id: &str) -> Option<String> {
    load_pool(state)
        .into_iter()
        .find(|a| a.id == account_id)
        .and_then(|a| a.device_profile)
        .map(|p| p.machine_id)
        .filter(|m| !m.is_empty())
}

// ── 环境检测（M0 侦察结论固化为候选路径；M0 R-1/R-2 缺口闭合后扩展）─────────

/// IDE exe 候选：settings.qoder_ide_path 人工指定优先，否则默认安装布局。
/// M0 实测（2026-09-26）：`%LOCALAPPDATA%\Programs\Qoder CN\Qoder CN.exe`（无版本化子目录），
/// 设计文档的 `.qoder-versions\<ver>\` 形态（国际版）与本机不符——候选列表按两种形态兼容。
pub(crate) fn ide_exe_candidates(state: &AppState) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = state.settings().qoder_ide_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            out.push(PathBuf::from(p));
            return out;
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let base = PathBuf::from(&local).join("Programs").join("Qoder CN");
        out.push(base.join("Qoder CN.exe"));
        // 国际版形态：.qoder-versions\<ver>\Qoder CN.exe（版本化目录枚举，最多 4 个）
        if let Ok(entries) = std::fs::read_dir(base.join(".qoder-versions")) {
            let mut vers: Vec<PathBuf> = entries
                .into_iter()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            vers.sort();
            for v in vers.into_iter().rev().take(4) {
                out.push(v.join("Qoder CN.exe"));
            }
        }
    }
    out
}

/// IDE 数据目录（M0 实测修正：`%APPDATA%\QoderCN`，与 switcher Qoder 档案 data_dir 同源）
pub(crate) fn ide_data_dir() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|d| PathBuf::from(d).join("QoderCN"))
}

/// QoderWork CN exe 候选：settings.qoderwork_path 人工指定优先，否则默认安装布局。
/// 需求方确认（2026-09-27）：QoderWork CN Windows 默认经 Launcher 安装于
/// `%LOCALAPPDATA%\Qoder CN\Qoder CN Launcher\Qoder CN Launcher.exe`。
pub(crate) fn work_exe_candidates(state: &AppState) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = state.settings().qoderwork_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            out.push(PathBuf::from(p));
            return out;
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        out.push(
            PathBuf::from(&local)
                .join("Qoder CN")
                .join("Qoder CN Launcher")
                .join("Qoder CN Launcher.exe"),
        );
    }
    out
}

// tasklist/creation_flags 为 Windows 专属（与 ide_store.rs 同款逐函数门控）
#[cfg(windows)]
fn is_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Qoder CN.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Qoder CN.exe"))
}

#[cfg(not(windows))]
fn is_running() -> bool {
    false
}

#[cfg(windows)]
fn launcher_running() -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Qoder CN Launcher.exe", "/NH"])
        .creation_flags(0x08000000)
        .output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Qoder CN Launcher.exe"))
}

#[cfg(not(windows))]
fn launcher_running() -> bool {
    false
}

/// 环境检测（环境配置页/概述页/顶栏数据源）：IDE / 数据目录 / CLI / QoderWork。
/// async 命令：内部 tasklist 子进程 + 磁盘探测会阻塞数百毫秒，同步命令会卡死主线程
#[tauri::command(async)]
pub fn qoder_env_check(state: State<AppState>) -> serde_json::Value {
    let exe = ide_exe_candidates(&state)
        .into_iter()
        .find(|p| p.exists());
    let work_exe = work_exe_candidates(&state)
        .into_iter()
        .find(|p| p.exists());
    let data_dir = ide_data_dir();
    let data_dir_str = data_dir.as_ref().map(|p| p.to_string_lossy().to_string());
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    serde_json::json!({
        "ide_installed": exe.is_some(),
        "ide_running": is_running(),
        "ide_exe": exe.map(|p| p.to_string_lossy().to_string()),
        "ide_data_dir": data_dir_str,
        "ide_data_dir_exists": data_dir.map(|p| p.exists()).unwrap_or(false),
        "cli_dir": home,
        "cli_dir_exists": !home.is_empty() && PathBuf::from(&home).join(".qoder-cn").exists(),
        // QoderWork：即 Qoder CN 本体（v1.4.1 产品同一性澄清），exe 装机检测走 Launcher 形态；
        // 用户数据目录即 ide_data_dir（%APPDATA%\QoderCN），无独立布局
        "qoderwork_installed": work_exe.is_some(),
        "qoderwork_running": launcher_running(),
        "qoderwork_exe": work_exe.map(|p| p.to_string_lossy().to_string()),
    })
}

// ── 打开客户端（对照 commands/env.rs open_buddy_app 极简版：spawn 不等待）──

fn spawn_first_existing(candidates: Vec<PathBuf>, display: &str) -> Result<(), String> {
    let exe = candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| format!("未检测到 {display} 客户端，请先安装或在「环境配置」手动指定路径"))?;
    Command::new(&exe)
        .spawn()
        .map_err(|e| format!("启动 {display} 失败: {e}"))?;
    Ok(())
}

/// 打开 Qoder CN IDE
#[tauri::command(async)]
pub fn qoder_open_ide(state: State<AppState>) -> Result<(), String> {
    spawn_first_existing(ide_exe_candidates(&state), "Qoder CN IDE")
}

/// 打开 QoderWork CN
#[tauri::command(async)]
pub fn qoder_open_work(state: State<AppState>) -> Result<(), String> {
    spawn_first_existing(work_exe_candidates(&state), "QoderWork CN")
}
