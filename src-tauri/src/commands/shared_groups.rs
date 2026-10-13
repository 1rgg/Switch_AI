//! 跨应用共享分组（shared-groups-tags-design.md P0，需求①）：
//! - 分组「定义」统一收敛到既有 `groups` 行文档表（每行一个 Group JSON，id 前缀
//!   统一 `shg_`，启动迁移保证存量全量重定向）；「成员关系」保持各池现状（D1）：
//!   Trae 走 group_members 映射表，WB/Qoder 走账号记录 group_id 字段；
//! - 三池既有分组命令保留旧名作薄封装（前端零改动），内部转发统一实现；
//! - 删除分组时按「先回落成员 → 再删定义 → 最后清引用」顺序（对齐
//!   qoder_groups_remove 审查结论），并联动清理 api_pool / api_keys 引用 + 热重载。

use tauri::State;

use crate::models::Group;
use crate::state::AppState;

use super::accounts::GroupView;

// ── 内部工具 ────────────────────────────────────────────────────────────────

pub(crate) fn defs_load(state: &AppState) -> Vec<Group> {
    crate::store::docs::group_defs_load(&crate::store::db(&state.data_dir))
}

fn defs_save(state: &AppState, defs: &[Group]) -> Result<(), String> {
    crate::store::docs::group_defs_save(&crate::store::db(&state.data_dir), defs)
}

/// 全局重名校验（推广 qoder validate_name；trim/非空/共享定义内唯一）。
/// exclude_id 供 update 排除自身；返回 trim 后的名称
pub(crate) fn validate_name(
    defs: &[Group],
    name: &str,
    exclude_id: Option<&str>,
) -> Result<String, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("分组名不能为空".into());
    }
    if defs
        .iter()
        .any(|g| g.name == name && Some(g.id.as_str()) != exclude_id)
    {
        return Err(format!("分组「{name}」已存在"));
    }
    Ok(name)
}

/// 各池成员推导（D1：成员关系保持各池机制）。返回 (trae_uids, wb_ids, qoder_ids)。
/// Trae 成员读 group_members 表；WB/Qoder 读各自池账号 group_id 字段。
pub(crate) fn pool_members(state: &AppState, gid: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let store = crate::store::db(&state.data_dir);
    // Trae：group_members 表
    let trae: Vec<String> = store
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid FROM group_members WHERE group_id = ?1")?;
            let rows = stmt
                .query_map([gid], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    // Buddy：wb_accounts 行文档 group_id 字段（Value 语义，与迁移同源）
    let wb_pool = crate::store::docs::wb_pool_load(&store);
    let wb: Vec<String> = wb_pool
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|a| a.get("group_id").and_then(|v| v.as_str()) == Some(gid))
                .filter_map(|a| a.get("id").and_then(|v| v.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    // Qoder：qoder_accounts 行文档 group_id 字段
    let qoder_pool = crate::store::docs::qoder_pool_load(&store);
    let qoder: Vec<String> = qoder_pool
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|a| a.get("group_id").and_then(|v| v.as_str()) == Some(gid))
                .filter_map(|a| a.get("id").and_then(|v| v.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    (trae, wb, qoder)
}

// ── 列表 ────────────────────────────────────────────────────────────────────

/// 共享分组列表：defs 全局一份；uids = 三池成员并集（trae 数字 uid + wb- + qd-，
/// 命名空间天然不冲突），count = uids.len()。供 API Key 分组绑定等跨池消费方使用。
#[tauri::command]
pub fn shared_groups_list(state: State<AppState>) -> Vec<GroupView> {
    let defs = defs_load(&state);
    defs.into_iter()
        .map(|g| {
            let (mut uids, wb, qoder) = pool_members(&state, &g.id);
            uids.extend(wb);
            uids.extend(qoder);
            let count = uids.len();
            GroupView { id: g.id, name: g.name, color: g.color, order: g.order, count, uids }
        })
        .collect()
}

/// 各池视图组装（三池既有 list 命令共用）：defs 读统一存储，uids 仍从各自池推导。
/// uids_by_gid 由调用方**一次**整池扫描构建（防 N+1：禁止在每分组闭包内重载池）。
pub(crate) fn list_for_pool(
    state: &AppState,
    uids_by_gid: std::collections::HashMap<String, Vec<String>>,
) -> Vec<GroupView> {
    defs_load(state)
        .into_iter()
        .map(|g| {
            let uids = uids_by_gid.get(&g.id).cloned().unwrap_or_default();
            let count = uids.len();
            GroupView { id: g.id, name: g.name, color: g.color, order: g.order, count, uids }
        })
        .collect()
}

// ── 增删改 ──────────────────────────────────────────────────────────────────

/// 创建共享分组（shg_ 前缀 + 全局重名校验）
pub(crate) fn create_inner(state: &AppState, name: String, color: String) -> Result<String, String> {
    let mut defs = defs_load(state);
    let name = validate_name(&defs, &name, None)?;
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("shg_{}{}", chrono::Local::now().timestamp_millis(), &rand[..4]);
    let order = (defs.len() as i32) + 1;
    defs.push(Group { id: id.clone(), name, color, order });
    defs_save(state, &defs)?;
    Ok(id)
}

#[tauri::command]
pub fn shared_groups_create(state: State<AppState>, name: String, color: String) -> Result<String, String> {
    create_inner(&state, name, color)
}

pub(crate) fn update_inner(
    state: &AppState,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    let mut defs = defs_load(state);
    // 先校验后可变借用（规避 iter_mut 与校验读借用冲突，对齐 qoder 实现）
    let new_name = match name.as_deref() {
        Some(n) => Some(validate_name(&defs, n, Some(&id))?),
        None => None,
    };
    let g = defs.iter_mut().find(|g| g.id == id).ok_or("分组不存在")?;
    if let Some(n) = new_name {
        g.name = n;
    }
    if let Some(c) = color {
        g.color = c;
    }
    if let Some(o) = order {
        g.order = o;
    }
    defs_save(state, &defs)
}

#[tauri::command]
pub fn shared_groups_update(
    state: State<AppState>,
    id: String,
    name: Option<String>,
    color: Option<String>,
    order: Option<i32>,
) -> Result<(), String> {
    update_inner(&state, id, name, color, order)
}

/// 删除共享分组（顺序敏感，见模块注释）：
/// ① 三池成员回落「未分组」→ ② 删定义 → ③ 清 api_pool / api_keys.allowed_groups
/// 引用 → ④ 有成员或引用变更时热重载 + org 索引失效。
pub(crate) fn delete_inner(
    state: &AppState,
    runtime: &std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>,
    id: &str,
) -> Result<(), String> {
    let store = crate::store::db(&state.data_dir);
    // ①a Trae 成员回落：直接删 group_members 行（缺失 = 未分组，幂等）
    store.with_conn(|c| {
        c.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
        Ok(())
    })?;
    // ①b Buddy 成员回落（读-改-写互斥：罩住池段防旧副本整文档覆盖）
    {
        let _guard = state.wb_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = crate::store::docs::wb_pool_load(&store);
        let mut changed = false;
        if let Some(arr) = pool.get_mut("accounts").and_then(serde_json::Value::as_array_mut) {
            for a in arr.iter_mut() {
                if a.get("group_id").and_then(|v| v.as_str()) == Some(id) {
                    a["group_id"] = serde_json::json!("");
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::docs::wb_pool_save(&store, &pool)?;
        }
    }
    // ①c Qoder 成员回落（Value 语义直改保留未知字段，持 qoder_pool_lock——
    // qoder::common 为私有模块，此处与 WB 侧同构实现）
    {
        let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut pool = crate::store::docs::qoder_pool_load(&store);
        let mut changed = false;
        if let Some(arr) = pool.get_mut("accounts").and_then(serde_json::Value::as_array_mut) {
            for a in arr.iter_mut() {
                if a.get("group_id").and_then(|v| v.as_str()) == Some(id) {
                    a["group_id"] = serde_json::json!("");
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::docs::qoder_pool_save(&store, &pool)?;
        }
    }
    // ② 删定义
    let mut defs = defs_load(state);
    defs.retain(|g| g.id != id);
    defs_save(state, &defs)?;
    // ③ 清引用：api_pool 三池分组筛选 + api_keys.allowed_groups
    let mut refs_changed = false;
    let mut pool_file = crate::commands::api_server::load_pool_file(&state.data_dir);
    for (field, ids) in [
        ("group_ids", &mut pool_file.group_ids),
        ("wb_group_ids", &mut pool_file.wb_group_ids),
        ("qoder_group_ids", &mut pool_file.qoder_group_ids),
    ] {
        let _ = field;
        if ids.iter().any(|g| g == id) {
            ids.retain(|g| g != id);
            refs_changed = true;
        }
    }
    if refs_changed {
        store.kv_set("api_pool", &pool_file)?;
    }
    // Key 引用清理走锁内原子修改（防与并发 UI 保存互覆约束字段）
    crate::api_server::api_keys::modify(&state.data_dir, |keys_file| {
        let mut changed = false;
        for k in keys_file.keys.iter_mut() {
            if k.allowed_groups.iter().any(|g| g == id) {
                k.allowed_groups.retain(|g| g != id);
                changed = true;
            }
        }
        changed
    });    // ④ 联动：成员回落 / 池筛选变更都影响调度池；Key 约束展开索引失效
    if refs_changed {
        crate::commands::api_server::reload_pools_if_running(state, runtime);
    }
    crate::api_server::org_index::invalidate(&state.data_dir);
    Ok(())
}

#[tauri::command]
pub fn shared_groups_delete(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    id: String,
) -> Result<(), String> {
    delete_inner(&state, runtime.inner(), &id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Group;

    fn tmp_state(tag: &str) -> (std::path::PathBuf, AppState) {
        let d = std::env::temp_dir().join(format!("twa_shg_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let state = AppState {
            data_dir: d.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        (d, state)
    }

    #[test]
    fn validate_name_global_unique() {
        let defs = vec![Group { id: "shg_1".into(), name: "主号".into(), color: String::new(), order: 1 }];
        assert!(validate_name(&defs, "  主号 ", None).is_err(), "全局重名拒绝");
        assert_eq!(validate_name(&defs, "  主号 ", Some("shg_1")).unwrap(), "主号", "排除自身后放行");
        assert!(validate_name(&defs, "   ", None).is_err(), "空名拒绝");
    }

    #[test]
    fn create_update_delete_roundtrip() {
        let (dir, state) = tmp_state("crud");
        let id = create_inner(&state, "主号".into(), "#f00".into()).unwrap();
        assert!(id.starts_with("shg_"));
        // 重名拒绝（全局）
        assert!(create_inner(&state, "主号".into(), "#0f0".into()).is_err());
        update_inner(&state, id.clone(), Some("主力".into()), None, None).unwrap();
        let defs = defs_load(&state);
        assert_eq!(defs[0].name, "主力");
        // 删除：无成员无引用也应成功（幂等空操作）
        let runtime: std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>> =
            std::sync::Mutex::new(None);
        delete_inner(&state, &runtime, &id).unwrap();
        assert!(defs_load(&state).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
