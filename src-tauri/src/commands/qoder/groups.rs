//! Qoder 账号分组（对齐 Buddy 账号分组能力，复用 accounts.rs GroupView / models::Group）：
//! - 分组定义存 kv("qoder_groups")（`Vec<Group>`，结构与 Trae/Buddy 一致）；
//! - 成员关系直接落在 Qoder 账号记录的 `group_id` 字段（serde(default)，随账号池 JSON
//!   持久化，导出/导入天然携带，无需独立 membership 表）；
//! - 删除分组时组内账号回落「未分组」（对齐 Trae/Buddy group_delete 语义）。

use tauri::State;

use crate::state::AppState;

use super::common::{load_pool, with_pool_mut};

fn load_defs(state: &AppState) -> Vec<crate::models::Group> {
    crate::store::db(&state.data_dir).kv_get("qoder_groups")
}

fn save_defs(state: &AppState, defs: &[crate::models::Group]) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("qoder_groups", &defs.to_vec())
}

/// 分组列表（count/uids 从账号池账号的 group_id 实时推导，供前端过滤 chips 与编辑弹框下拉）
#[tauri::command]
pub fn qoder_groups_list(state: State<AppState>) -> Vec<crate::commands::accounts::GroupView> {
    let defs = load_defs(&state);
    let pool = load_pool(&state);
    defs.into_iter()
        .map(|g| {
            let uids: Vec<String> = pool
                .iter()
                .filter(|a| a.group_id == g.id)
                .map(|a| a.id.clone())
                .collect();
            let count = uids.len();
            crate::commands::accounts::GroupView {
                id: g.id,
                name: g.name,
                color: g.color,
                order: g.order,
                count,
                uids,
            }
        })
        .collect()
}

#[tauri::command]
pub fn qoder_groups_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    let mut defs = load_defs(&state);
    // 重名校验（审查 L）：同名分组会让前端按名匹配/展示产生歧义
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("分组名不能为空".into());
    }
    if defs.iter().any(|g| g.name == name) {
        return Err(format!("分组「{name}」已存在"));
    }
    let id = format!("qoderg_{}", chrono::Local::now().timestamp_millis());
    let order = (defs.len() as i32) + 1;
    defs.push(crate::models::Group {
        id: id.clone(),
        name,
        color,
        order,
    });
    save_defs(&state, &defs)?;
    Ok(id)
}

#[tauri::command]
pub fn qoder_groups_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut defs = load_defs(&state);
    let g = defs.iter_mut().find(|g| g.id == id).ok_or("分组不存在")?;
    if let Some(n) = name {
        g.name = n;
    }
    if let Some(c) = color {
        g.color = c;
    }
    if let Some(o) = order {
        g.order = o;
    }
    save_defs(&state, &defs)
}

#[tauri::command]
pub fn qoder_groups_remove(state: State<AppState>, id: String) -> Result<(), String> {
    let mut defs = load_defs(&state);
    defs.retain(|g| g.id != id);
    save_defs(&state, &defs)?;
    // 组内账号回落「未分组」（I09：持锁读-改-写，防并发整池覆盖丢更新）
    with_pool_mut(&state, |accounts| {
        for a in accounts.iter_mut() {
            if a.group_id == id {
                a.group_id = String::new();
            }
        }
        Ok(())
    })
}

/// 移动账号到分组（group_id=None 回落「未分组」）；user_id = 账号 id（qd- 前缀，与 save/remove 同键）
#[tauri::command]
pub fn qoder_account_move(
    state: State<AppState>,
    user_id: String,
    group_id: Option<String>,
) -> Result<(), String> {
    // 目标分组校验：group_id 传了非空值但分组已删除/不存在时直接拒绝，防幽灵分组
    if let Some(gid) = group_id.as_deref().filter(|g| !g.is_empty()) {
        if !load_defs(&state).iter().any(|g| g.id == gid) {
            return Err(format!("目标分组不存在: {gid}"));
        }
    }
    with_pool_mut(&state, |accounts| {
        let acct = accounts
            .iter_mut()
            .find(|a| a.id == user_id)
            .ok_or_else(|| format!("账号不在池中: {user_id}"))?;
        acct.group_id = group_id.unwrap_or_default();
        Ok(())
    })
}
