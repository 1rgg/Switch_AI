//! Qoder 账号域（F-80 M1）：账号列表/改名/移除/PAT 导入。
//!
//! 导入通道（§5.4）：M1 实装 **PAT 手工录入**（M0 R-3 侦察结论：CLI token 不明文落盘，
//! IDE 存储本机未生成，PAT 是最可靠且官方认可的通道）。
//! IDE 存储发现（L1）在 R-2/R-8 闭合后接入；MITM（L2）随 device_proxy M2 接入。

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Manager, State};

use crate::fs_utils;
use crate::state::AppState;

use super::common::{account_id_of, load_pool, with_pool_mut, QoderAccount};
use crate::tasks::qoder_common::{self, QoderCreds};

#[derive(Serialize, Clone)]
pub struct QoderAccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    pub phone_masked: String,
    pub plan: String,
    pub credential_source: String,
    pub token_expires_at: Option<i64>,
    pub needs_relogin: bool,
    pub relogin_reason: String,
    pub group_id: String,
    pub note: String,
    pub credits_balance: Option<f64>,
    pub credits_fetched_at: Option<String>,
    /// token store 中有可用凭证
    pub has_credential: bool,
    /// token 种类徽标：pat | client | unknown（脱敏，不含 token 本体）
    pub token_kind: String,
    /// 设备指纹徽标（§5.10：machine_id 前 8 位；None = 尚未回填）
    pub fingerprint: Option<String>,
    /// 完整设备指纹（§5.10；指纹查看弹框数据源）
    pub device_profile: Option<crate::tasks::qoder_device::QoderDeviceProfile>,
}

fn view_of(a: &QoderAccount, tokens: &Value) -> QoderAccountView {
    let rec = tokens
        .get("tokens")
        .and_then(|t| t.get(&a.id));
    let has_token = rec
        .and_then(|r| r.get("access_token").or_else(|| r.get("accessToken")))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let kind = rec
        .and_then(|r| r.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    QoderAccountView {
        id: a.id.clone(),
        uid: a.uid.clone(),
        nickname: a.nickname.clone(),
        phone_masked: a.phone_masked.clone(),
        plan: a.plan.clone(),
        credential_source: a.credential_source.clone(),
        token_expires_at: a.token_expires_at,
        needs_relogin: a.needs_relogin,
        relogin_reason: a.relogin_reason.clone(),
        group_id: a.group_id.clone(),
        note: a.note.clone(),
        credits_balance: a.credits_balance,
        credits_fetched_at: a.credits_fetched_at.clone(),
        has_credential: has_token,
        token_kind: kind,
        fingerprint: a
            .device_profile
            .as_ref()
            .filter(|p| !p.machine_id.is_empty())
            // I12：按字符截取，防非 ASCII machine_id 触发字节切片 panic
            .map(|p| p.machine_id.chars().take(8).collect::<String>()),
        device_profile: a.device_profile.clone(),
    }
}

/// 账号列表（含凭证状态；脱敏：只回 kind 徽标不回 token）。
/// 列表前惰性回填设备指纹（§5.10：存量账号幂等补齐，已有不覆盖）。
/// 审查 P3 修复：ensure_pool_profiles（磁盘 IO + RNG）与 vault 回填整体移入
/// spawn_blocking——不占 async worker、不阻塞 UI 线程
#[tauri::command]
pub async fn qoder_accounts_list(state: State<'_, AppState>) -> Result<Vec<QoderAccountView>, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // P2 审查修复：指纹回填为增强性操作，失败仅记日志降级继续——
        // 只读列表不应被回填失败连带拖垮（I14 同款留痕惯例）
        if let Err(e) = crate::tasks::qoder_device::ensure_pool_profiles(&st) {
            fs_utils::app_log(&st.data_dir, &format!("Qoder 指纹回填失败（已降级，列表继续）: {e}"));
        }
        let accounts = load_pool(&st);
        let tokens = qoder_common::load_token_store(&st);
        Ok(accounts.iter().map(|a| view_of(a, &tokens)).collect())
    })
    .await
    .map_err(|e| format!("账号列表任务失败: {e}"))?
}

/// 改名/备注（nickname 即展示名，可编辑覆盖 userinfo 值）。
/// 账号变更联动网关池热重载（与 Trae 侧 accounts.rs 同惯例；服务未运行时 no-op）
#[tauri::command]
pub fn qoder_account_save(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    user_id: String,
    name: Option<String>,
    note: Option<String>,
) -> Result<(), String> {
    // I09：持锁读-改-写，防并发整池覆盖丢更新
    with_pool_mut(&state, |accounts| {
        let Some(a) = accounts.iter_mut().find(|a| a.id == user_id) else {
            return Err(format!("账号不在池中: {user_id}"));
        };
        if let Some(n) = name {
            a.nickname = n.trim().to_string();
        }
        if let Some(n) = note {
            a.note = n;
        }
        Ok(())
    })?;
    crate::commands::api_server::reload_pools_if_running(&state, runtime.inner());
    Ok(())
}

/// 移除账号（同步清理 token store 记录，防悬空凭证残留）。
/// 审查修复：① 池中无条目但 token store 有记录（导入中断产生的孤儿凭证）时
/// 仍执行凭证清理——此前直接拒绝，孤儿真实凭证在 vault 中无任何 UI 清理出口；
/// ② 凭证清理失败上抛 Err 透出（此前仅落日志返回 Ok，用户对凭证残留无感知）；
/// ③ 账号变更联动网关池热重载（删除的账号即时退出调度，服务未运行时 no-op）
#[tauri::command]
pub fn qoder_account_remove(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    user_id: String,
) -> Result<(), String> {
    fs_utils::ensure_uid_safe(&user_id)?;
    // 池外孤儿判定：token store 有记录即可清理（池删除接口对孤儿凭证是唯一出口）
    let has_token = qoder_common::load_token_store(&state)
        .get("tokens")
        .and_then(|t| t.get(&user_id))
        .is_some();
    // I09：持锁读-改-写
    let removed = with_pool_mut(&state, |accounts| {
        let before = accounts.len();
        accounts.retain(|a| a.id != user_id);
        Ok(before != accounts.len())
    })?;
    if !removed && !has_token {
        return Err(format!("账号不在池中: {user_id}"));
    }
    // I14：清理失败上抛（原先静默吞掉，悬空凭证难排查且用户无感知）
    if let Err(e) = qoder_common::remove_token(&state, &user_id) {
        fs_utils::app_log(&state.data_dir, &format!("Qoder 账号 {user_id} 凭证清理失败: {e}"));
        let msg = if removed {
            format!("账号已从池中移除，但凭证清理失败：{e}（重新导入同一凭证后再次移除可重试）")
        } else {
            format!("孤儿凭证清理失败：{e}")
        };
        // 池移除已生效：无论凭证清理成败都必须热重载，网关侧即时剔除该账号
        //（否则残留账号凭 vault 旧凭证仍可被调度至下一次生命周期事件/重启）
        crate::commands::api_server::reload_pools_if_running(&state, runtime.inner());
        return Err(msg);
    }
    // Q3：主动回收该账号的全局刷新锁条目（仅摘表项无 DB 读；并发持有者的 Arc 由
    // 引用计数自然释放，若与并发刷新竞争，新到的 ensure_fresh 会重建条目，语义不变）
    qoder_common::refresh_lock_remove(&user_id);
    fs_utils::app_log(&state.data_dir, &format!("Qoder 账号已移除: {user_id}"));
    crate::commands::api_server::reload_pools_if_running(&state, runtime.inner());
    Ok(())
}

/// PAT 手工导入（M1 最可靠凭证通道；幂等：同 token 稳定同 id，重复导入=更新）。
/// 导入时尝试 /api/v1/userinfo 回填 uid/昵称（失败容错不阻塞——userinfo 对 PAT 的
/// 兼容性 R-6 待验证）。返回导入后的账号视图。
#[tauri::command]
pub async fn qoder_account_import_pat(
    app: AppHandle,
    state: State<'_, AppState>,
    name: Option<String>,
    pat: String,
) -> Result<QoderAccountView, String> {
    let pat = pat.trim().to_string();
    if pat.is_empty() {
        return Err("PAT 不能为空".into());
    }
    if !pat.starts_with("pt-") && !pat.starts_with("jt-") {
        return Err("凭证格式不识别：应为 qoder.com.cn/account/integrations 创建的 PAT（pt-）或客户端抓包获取的 job token（jt-）".into());
    }
    let id = account_id_of(&pat);
    // spawn_blocking：fetch_userinfo/fetch_plan 为阻塞 ureq 网络请求（15s 超时），
    // async 命令体内直接执行会占用 async worker 线程
    let pat2 = pat.clone();
    let (uid, nickname, plan) = tauri::async_runtime::spawn_blocking(move || {
        let agent = crate::tasks::http_agent(15);
        let creds = QoderCreds {
            access_token: pat2,
            kind: "pat".into(),
            ..Default::default()
        };
        let (uid, nickname) = qoder_common::fetch_userinfo(&agent, &creds);
        // 套餐回填（R-7 抓包固化：GET /api/v2/user/plan → plan_tier_name，如 "Pro Trial"；失败容错）
        let (tier, _user_type, _end) = qoder_common::fetch_plan(&agent, &creds);
        (uid.unwrap_or_default(), nickname.unwrap_or_default(), tier.unwrap_or_default())
    })
    .await
    .map_err(|e| format!("PAT 账号探测任务失败: {e}"))?;
    // 幂等入池：同 id 保留旧 uid/nickname（userinfo 失败时不覆盖既有信息）；持锁读-改-写（I09）
    let display = name
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    // 审查 P3 修复：入池（池锁 + SQLite）+ 凭证落库（TOKEN_STORE_LOCK + vault 慢 IO）
    // + 热重载整段移入 spawn_blocking——不占 async worker、不放大池锁互斥窗口
    let st = state.inner().clone();
    let app2 = app.clone();
    let id2 = id.clone();
    let pat3 = pat;
    tauri::async_runtime::spawn_blocking(move || -> Result<QoderAccountView, String> {
        let (display, uid, nickname, plan) = (display, uid, nickname, plan);
        with_pool_mut(&st, |accounts| {
            if let Some(a) = accounts.iter_mut().find(|a| a.id == id2) {
                if let Some(d) = display {
                    a.nickname = d;
                } else if a.nickname.is_empty() && !nickname.is_empty() {
                    // I13：显式传名仍覆盖；否则仅在池中昵称为空时补 userinfo 值，
                    // 防重复导入把用户改过的名覆盖回去（对照 ide_store.rs 守卫）
                    a.nickname = nickname.clone();
                }
                if a.uid.is_empty() && !uid.is_empty() {
                    a.uid = uid.clone();
                }
                if !plan.is_empty() {
                    a.plan = plan.clone();
                }
                // P2 审查修复：credential_source 保守更新——本路径按 id（token 摘要）命中，
                // 同 id 即同 token、凭证本体未变，仅来源字段为空时回填，防同账号多通道
                // 导入时徽标随「最后导入者」漂移（与 uid/nickname 的保守回填策略一致）
                if a.credential_source.is_empty() {
                    a.credential_source = "pat".into();
                }
                a.needs_relogin = false;
                a.relogin_reason = String::new();
                // 指纹回填（幂等：已有稳定绑定不覆盖，§5.10）
                if a.device_profile.is_none() {
                    a.device_profile = Some(crate::tasks::qoder_device::QoderDeviceProfile::generate());
                }
            } else {
                accounts.push(QoderAccount {
                    id: id2.clone(),
                    uid: uid.clone(),
                    nickname: display.or_else(|| if nickname.is_empty() { None } else { Some(nickname.clone()) })
                        .unwrap_or_else(|| format!("Qoder {}", &id2[3..9])),
                    plan,
                    credential_source: "pat".into(),
                    // 入池即生成稳定指纹（§5.10：一次生成永不轮换）
                    device_profile: Some(crate::tasks::qoder_device::QoderDeviceProfile::generate()),
                    ..Default::default()
                });
            }
            Ok(())
        })?;
        // 凭证入 token store（M1 单源）。
        // jt- 前缀（审查 L-jt）：同时写入 pat 字段——ensure_fresh 的 PAT 重换通道以
        // has_pat（pat 字段非空）触发，仅落 access_token 时 jt- 走不进重换路径，
        // 24h 过期后直接 refresh_failed 需手工重导
        let creds = QoderCreds {
            pat: pat3.clone(),
            access_token: pat3,
            kind: "pat".into(),
            uid: uid.clone(),
            nickname: nickname.clone(),
            ..Default::default()
        };
        qoder_common::save_token_store(&st, &id2, &creds)?;
        fs_utils::app_log(&st.data_dir, &format!("Qoder PAT 账号已导入: {id2}"));
        // 新账号/凭证变更联动网关池热重载（fail-open 新账号即时入池调度；服务未运行时 no-op）
        let rt = app2.state::<std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>();
        crate::commands::api_server::reload_pools_if_running(&st, rt.inner());
        let accounts = load_pool(&st);
        let tokens = qoder_common::load_token_store(&st);
        accounts
            .iter()
            .find(|a| a.id == id2)
            .map(|a| view_of(a, &tokens))
            .ok_or_else(|| "导入后读取账号失败".into())
    })
    .await
    .map_err(|e| format!("PAT 账号落库任务失败: {e}"))?
}
