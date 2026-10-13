//! 启动迁移器：旧 data/ JSON 全量导入 SQLite，成功后原文件移入 `data/backup/`（幂等）。
//!
//! 三态（docs/sqllite-storage-plan.md §三）：
//! - 正常：解析 → 写库 → `fs::rename` 到 `backup/<相对路径>`；
//! - 损坏：JSON 解析失败 → 移 `backup/corrupt/`（对齐 read_json「坏文件回退默认」）；
//! - 失败：IO/DB 错误 → 保留原位，下次启动重试（不置 user_version）。
//!
//! 全部条目处理完且零失败才置 `PRAGMA user_version = 1`；明细落
//! `data/backup/migration_manifest.json`。失败不阻断启动（返回摘要由调用方写日志）。

use serde_json::{json, Value};
use std::path::Path;

use super::{db, docs, schema, Store};

/// KV 组：（相对路径 → kv 键）。content = 文件文本原样（校验为合法 JSON 后入库）。
const KV_ENTRIES: &[(&str, &str)] = &[
    ("conf/app_settings.json", "app_settings"),
    ("data/api_pool.json", "api_pool"),
    ("data/dispatch_policy.json", "dispatch_policy"),
    ("data/api_gateway_settings.json", "api_gateway_settings"),
    ("data/api_models.json", "api_models"),
    ("data/wb_model_catalog.json", "wb_model_catalog"),
    ("data/trae_model_meta.json", "trae_model_meta"),
    ("data/wb_model_route.json", "wb_model_route"),
    ("data/wb_template_map.json", "wb_template_map"),
    ("data/checkin_summary.json", "checkin_summary"),
    ("data/workbuddy_settings.json", "workbuddy_settings"),
    ("data/workbuddy_credits_cache.json", "workbuddy_credits_cache"),
    ("data/workbuddy_usage_official_cache.json", "workbuddy_usage_official_cache"),
    ("data/workbuddy_usage_official_all_cache.json", "workbuddy_usage_official_all_cache"),
    ("data/workbuddy_activity_cache.json", "workbuddy_activity_cache"),
    ("data/wb_cli_rotate_state.json", "wb_cli_rotate_state"),
    ("data/token_stats_files.json", "token_stats_files"),
    ("data/doubao_renew_result.json", "doubao_renew_result"),
    ("data/oauth_device.json", "oauth_device"),
    ("data/scheduler_state.json", "scheduler_state"),
    ("data/doubao_captured_credentials.json", "doubao_captured_credentials"),
];
// P6：workbuddy_credits_history / usage_history / wb_sticky_sessions 三键为流水型数据，
// 已从 kv 组移出为表（wb_credits_history / usage_history_* / sticky_bindings），
// 由下方 DOC_ENTRIES / v1→v2 增量迁移处理。

/// 结构化组：（相对路径 → 导入函数）
const DOC_ENTRIES: &[(&str, fn(&Store, &Path) -> ImportStatus)] = &[
    ("data/checkin_accounts.json", import_accounts),
    ("data/device_map.json", import_device_map),
    ("data/groups.json", import_groups),
    ("data/remaining_credits.json", import_remaining_credits),
    ("data/account_cooldowns.json", import_account_cooldowns),
    ("data/pay_status.json", import_pay_status),
    ("data/api_keys.json", import_api_keys),
    ("data/custom_models.json", import_custom_models),
    ("data/doubao_accounts.json", import_doubao_pool),
    ("data/workbuddy_accounts.json", import_wb_pool),
    ("data/workbuddy_token_store.json", import_wb_token_store),
    ("data/api_usage.json", import_api_usage),
    ("data/credits_history.json", import_credits_history),
    ("data/credits_daily.json", import_credits_daily),
    ("data/checkin_results.json", import_checkin_results),
    ("data/workbuddy_checkin_results.json", import_wb_checkin_results),
    ("data/doubao_health_history.json", import_doubao_health),
    // P6 流水迁出
    ("data/workbuddy_credits_history.json", import_wb_credits_history),
    ("data/usage_history.json", import_usage_history),
    ("data/wb_sticky_sessions.json", import_sticky_bindings),
];

/// 遗留根路径兜底（存在才迁移；含 wb_upstream 死引用曾写出的根路径 token store）。
/// （相对路径, 目标 kv 键 或 None=走结构化导入器）
const LEGACY_ROOT: &[(&str, Option<&str>)] = &[
    ("workbuddy_token_store.json", None),
    ("wb_template_map.json", Some("wb_template_map")),
    ("api_models.json", Some("api_models")),
    ("wb_model_route.json", Some("wb_model_route")),
    ("wb_sticky_sessions.json", None),
    ("checkin_accounts.json", None),
];

enum ImportStatus {
    Ok,
    Corrupt(String),
    Error(String),
}

/// 迁移入口。返回 None = 无需迁移（库已是最新）；Some = 摘要（调用方写 app_log）。
pub fn migrate_on_startup(data_dir: &Path) -> Option<String> {
    let store = db(data_dir);
    let current = store
        .with_conn(|c| Ok(schema::user_version(c)))
        .unwrap_or(0);
    if current >= schema::SCHEMA_VERSION {
        return None;
    }

    let mut items: Vec<Value> = Vec::new();
    let mut failed = 0usize;
    let mut imported = 0usize;
    let mut corrupt = 0usize;
    let mut missing = 0usize;
    // 已成功导入的正牌文件集合（遗留根路径兜底仅在其正牌文件缺失时执行，
    // 防止根路径残留覆盖 data/ 正牌数据）
    let mut imported_rels: std::collections::HashSet<String> = Default::default();

    for (rel, key) in KV_ENTRIES {
        match import_kv(&store, data_dir, rel, key) {
            ItemResult::Imported => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                items.push(json!({"file": rel, "result": "imported", "target": format!("kv:{key}")}));
                move_to_backup(data_dir, rel, false);
            }
            ItemResult::Missing => {
                missing += 1;
                items.push(json!({"file": rel, "result": "missing"}));
            }
            ItemResult::Corrupt(e) => {
                corrupt += 1;
                items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                move_to_backup(data_dir, rel, true);
            }
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (rel, importer) in DOC_ENTRIES {
        let path = data_dir.join(rel);
        if !path.exists() {
            missing += 1;
            items.push(json!({"file": rel, "result": "missing"}));
            continue;
        }
        match importer(&store, &path) {
            ImportStatus::Ok => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                items.push(json!({"file": rel, "result": "imported"}));
                move_to_backup(data_dir, rel, false);
            }
            ImportStatus::Corrupt(e) => {
                corrupt += 1;
                items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                move_to_backup(data_dir, rel, true);
            }
            ImportStatus::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (name, kv_key) in LEGACY_ROOT {
        let path = data_dir.join(name);
        if !path.exists() {
            continue;
        }
        // 正牌文件（data/ 下同名）已导入 → 根路径残留仅作垃圾清理
        if imported_rels.contains(&format!("data/{name}")) {
            items.push(json!({"file": name, "result": "skipped_stale", "scope": "legacy_root"}));
            move_legacy_root_to_backup(data_dir, name);
            continue;
        }
        let result = match kv_key {
            Some(key) => import_kv(&store, data_dir, name, key),
            None => {
                let importer = match *name {
                    "checkin_accounts.json" => import_accounts,
                    "wb_sticky_sessions.json" => import_sticky_bindings,
                    _ => import_wb_token_store,
                };
                to_item(run_importer(&store, &path, importer))
            }
        };
        match result {
            ItemResult::Imported => {
                imported += 1;
                items.push(json!({"file": name, "result": "imported", "scope": "legacy_root"}));
                move_legacy_root_to_backup(data_dir, name);
            }
            ItemResult::Corrupt(_) => {
                corrupt += 1;
                items.push(json!({"file": name, "result": "corrupt", "scope": "legacy_root"}));
                move_legacy_root_to_backup(data_dir, name);
            }
            ItemResult::Missing => {}
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": name, "result": "error", "detail": e}));
            }
        }
    }

    // 全部条目处理完且零失败才置版本号（有失败保留原位，下次启动重试）。
    // 设计取舍：单文件失败 → 整体重跑（已导入文件已移入 backup 记 missing，幂等无害；
    // 代价是失败期间每次启动重写 manifest，属可接受日志噪音）。
    if failed == 0 {
        // P6 v1→v2 增量：老库 kv 中三个流水键搬入表（v0 全新导入路径键已由 DOC_ENTRIES 处理，此处空转）
        if let Err(e) = migrate_kv_flows_v1_to_v2(&store) {
            failed += 1;
            items.push(json!({"file": "<kv_flows_v1_to_v2>", "result": "error", "detail": e}));
        }
    }
    if failed == 0 {
        // v3→v4 增量：三套独立分组定义统一为共享分组（shg_ 新 ID + id_map 全量重定向）；
        // kv 标记幂等，全新安装空转
        match migrate_unified_groups(&store) {
            Ok(n) if n > 0 => {
                items.push(json!({"file": "<unified_groups>", "result": "imported", "detail": format!("统一分组 {n} 条")}));
                imported += 1;
            }
            Ok(_) => {}
            Err(e) => {
                failed += 1;
                items.push(json!({"file": "<unified_groups>", "result": "error", "detail": e}));
            }
        }
    }
    if failed == 0 {
        let _ = store.with_conn(|c| {
            schema::set_user_version(c, schema::SCHEMA_VERSION);
            Ok(())
        });
    }

    // 明细清单落 backup/
    let manifest = json!({
        "started_at": crate::fs_utils::now_ts(),
        "schema_version": schema::SCHEMA_VERSION,
        "imported": imported,
        "missing": missing,
        "corrupt": corrupt,
        "failed": failed,
        "items": items,
    });
    let backup_dir = data_dir.join("data").join("backup");
    let _ = std::fs::create_dir_all(&backup_dir);
    let _ = crate::fs_utils::write_json(&backup_dir.join("migration_manifest.json"), &manifest);

    Some(format!(
        "SQLite 迁移完成：导入 {imported}，缺失 {missing}，损坏 {corrupt}，失败 {failed}（明细见 data/backup/migration_manifest.json）"
    ))
}

enum ItemResult {
    Imported,
    Missing,
    Corrupt(String),
    Error(String),
}

/// KV 导入：文件文本校验为合法 JSON 后入库（serde 解析→重序列化，键序规范化，
/// 语义与原文件等价；解析失败 = Corrupt）
fn import_kv(store: &Store, data_dir: &Path, rel: &str, key: &str) -> ItemResult {
    let path = data_dir.join(rel);
    if !path.exists() {
        return ItemResult::Missing;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return ItemResult::Corrupt("空文件".into());
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => match store.kv_set_raw(key, &v.to_string()) {
                    Ok(()) => ItemResult::Imported,
                    Err(e) => ItemResult::Error(e),
                },
                Err(e) => ItemResult::Corrupt(format!("JSON 解析失败: {e}")),
            }
        }
        Err(e) => ItemResult::Error(format!("读取失败: {e}")),
    }
}

fn run_importer(store: &Store, path: &Path, f: fn(&Store, &Path) -> ImportStatus) -> ImportStatus {
    f(store, path)
}

fn to_item(s: ImportStatus) -> ItemResult {
    match s {
        ImportStatus::Ok => ItemResult::Imported,
        ImportStatus::Corrupt(e) => ItemResult::Corrupt(e),
        ImportStatus::Error(e) => ItemResult::Error(e),
    }
}

/// 原文件移入 backup/（保留 conf/data 相对路径前缀；corrupt=true 时入 backup/corrupt/）
fn move_to_backup(data_dir: &Path, rel: &str, corrupt: bool) {
    let src = data_dir.join(rel);
    let file_name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.to_string());
    let backup_dir = data_dir
        .join("data")
        .join("backup")
        .join(if corrupt { "corrupt" } else { "" });
    let dst = backup_dir.join(rel);
    let _ = std::fs::create_dir_all(dst.parent().unwrap_or(&backup_dir));
    if dst.exists() {
        // 同名冲突：加时间戳后缀，绝不覆盖既有备份
        let stamped = format!(
            "{}_{}",
            file_name.trim_end_matches(".json"),
            chrono::Local::now().format("%Y%m%d%H%M%S")
        );
        let _ = std::fs::rename(&src, dst.with_file_name(format!("{stamped}.json")));
        return;
    }
    let _ = std::fs::rename(&src, dst);
}

/// 遗留根路径文件 → backup/legacy_root/
fn move_legacy_root_to_backup(data_dir: &Path, name: &str) {
    let src = data_dir.join(name);
    let dir = data_dir.join("data").join("backup").join("legacy_root");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::rename(&src, dir.join(name));
}

// ── 结构化导入器（读原 struct → docs::save；解析失败 = Corrupt）──────────────

/// raw 形态导入（保真）：device_proxy 会向账号 JSON 写入 struct 外扩展字段
///（refresh_token_updated_at 等，typed roundtrip 会丢），与 accounts_save_raw
/// 运行时语义对齐；整文件 JSON 解析失败仍判 Corrupt。
fn import_accounts(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::accounts_save_raw(store, &v))
}

fn import_device_map(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let map: crate::models::DeviceMap = serde_json::from_value(v).map_err(ser_err)?;
        docs::device_map_save(store, &map)
    })
}

fn import_groups(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::GroupsFile>(&text) {
            Ok(f) => wrap(docs::groups_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_remaining_credits(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::RemainingCreditsFile>(&text) {
            Ok(f) => wrap(docs::remaining_credits_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_account_cooldowns(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::AccountCooldownsFile>(&text) {
            Ok(f) => wrap(docs::account_cooldowns_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_pay_status(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let f: crate::commands::trae_apps::PayStatusFile =
            serde_json::from_value(v).map_err(ser_err)?;
        docs::pay_status_save(store, &f)
    })
}

fn import_api_keys(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::api_keys::ApiKeysFile>(&text) {
            Ok(f) => wrap(docs::api_keys_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_custom_models(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::custom_models::CustomModelsFile>(&text) {
            Ok(f) => wrap(docs::custom_models_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_doubao_pool(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            match serde_json::from_str::<crate::commands::doubao::DoubaoAccountPool>(&text) {
                Ok(pool) => wrap(docs::doubao_pool_save(store, &pool)),
                // DoubaoAccountPool 序列化含 private 字段默认值兜底，解析失败按损坏处理
                Err(e) => ImportStatus::Corrupt(e.to_string()),
            }
        }
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_pool(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_pool_save(store, &v))
}

fn import_wb_token_store(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_token_store_save(store, &v))
}

fn import_api_usage(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::usage::UsageFile>(&text) {
            Ok(f) => wrap(docs::api_usage_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_history(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsFile>(&text) {
            Ok(f) => wrap(docs::credits_history_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_daily(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsDailyFile>(&text) {
            Ok(f) => wrap(docs::credits_daily_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::checkin_results::ResultsFile>(&text) {
            Ok(f) => wrap(docs::checkin_results_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_checkin_results_save(store, &v))
}

fn import_doubao_health(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let empty = Vec::new();
        let events = v.get("events").and_then(Value::as_array).unwrap_or(&empty);
        docs::doubao_health_save(store, events)
    })
}

// ── P6 流水迁出导入器 ────────────────────────────────────────────────────────

fn import_wb_credits_history(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_credits_history_save(store, &v))
}

fn import_usage_history(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::usage_history_save(store, &v))
}

fn import_sticky_bindings(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::sticky_bindings_save(store, &v))
}

/// v1→v2 增量迁移：kv 中三个流水键搬入对应表（v1 老库升级路径）
fn migrate_kv_flows_v1_to_v2(store: &Store) -> Result<(), String> {
    let importers: [(&str, fn(&Store, &Value) -> Result<(), String>); 3] = [
        ("workbuddy_credits_history", import_wb_credits_history_value),
        ("usage_history", import_usage_history_value),
        ("wb_sticky_sessions", import_sticky_bindings_value),
    ];
    for (key, importer) in importers {
        let Some(text) = store.kv_get_raw(key) else {
            continue; // v1 老库无此键（全新安装）→ 跳过
        };
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("kv {key} 解析失败: {e}"))?;
        importer(store, &v)?;
        store.kv_delete(key)?;
    }
    Ok(())
}

fn import_wb_credits_history_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::wb_credits_history_save(store, v)
}

// ── v3→v4：三套分组定义统一为跨应用共享分组（shg_ 新 ID）─────────────────────

/// 三套分组定义统一（shared-groups-tags-design.md D2，已确认 2026-10-10）：
/// 1. 读三处 defs（groups 表 + kv workbuddy_groups + kv qoder_groups），
///    迁移前整体快照至 kv("groups_pre_migration") 可回滚；
/// 2. 按 name 去重合并（同名即同组，颜色/排序取 Trae > Buddy > Qoder 首个出现），
///    全部重新生成 `shg_` 新 ID（不保留任何旧应用 id），构建 id_map 全量重定向；
/// 3. 重定向 group_members 表、WB/Qoder 账号 group_id 字段、
///    api_pool.group_ids/wb_group_ids/qoder_group_ids；
/// 4. 全删全写 groups 表；删除 kv workbuddy_groups / qoder_groups；
/// 5. kv("unified_groups_migrated") 标记幂等，二次启动跳过。
/// 容错（审查修复）：中途失败重试时，① 快照不覆盖（只写一次保原始备份）；
/// ② 复用 groups 表已残留的 shg_ 定义 id，重定向引用不悬空。
/// 返回统一后的分组条数（0 = 幂等跳过）。
fn migrate_unified_groups(store: &Store) -> Result<usize, String> {
    use crate::models::Group;
    if store.kv_get::<bool>("unified_groups_migrated") {
        return Ok(0);
    }
    // 1. 读三处 defs + Trae membership，整体快照备份（可回滚）。
    //    快照只写一次（审查修复）：中途失败重试时若覆盖快照，备份会被
    //    「已部分重定向」的状态污染，恰在回滚最需要时失效。
    let trae_all: Vec<Group> = docs::group_defs_load(store);
    // 上次中途失败残留的 shg_ 定义不算 Trae 旧定义（迁移计数/备份口径均剔除）
    let trae: Vec<Group> = trae_all
        .iter()
        .filter(|g| !g.id.starts_with("shg_"))
        .cloned()
        .collect();
    let wb: Vec<Group> = store.kv_get("workbuddy_groups");
    let qoder: Vec<Group> = store.kv_get("qoder_groups");
    let member_rows: Vec<(String, String)> = store
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    let members_before: std::collections::HashMap<String, String> =
        member_rows.iter().cloned().collect();
    if store.kv_get_raw("groups_pre_migration").is_none() {
        store.kv_set(
            "groups_pre_migration",
            &json!({
                "groups": trae_all,
                "workbuddy_groups": wb,
                "qoder_groups": qoder,
                "group_members": members_before,
            }),
        )?;
    }

    // 2. 按 name 合并 + 生成统一新 ID（id_map 覆盖全部旧 id，含同名多组）。
    //    中途失败重试场景（审查修复）：groups 表可能残留上次生成的 shg_ 定义
    //    （重定向已生效但 defs 全删全写失败、标记未置位）——按 name 复用既有
    //    shg_ id，保证已重定向的成员/账号/池筛选引用不悬空。
    let mut new_defs: Vec<Group> = trae_all
        .into_iter()
        .filter(|g| g.id.starts_with("shg_"))
        .collect();
    let mut id_map: std::collections::HashMap<String, String> = Default::default();
    let mut name_to_new: std::collections::HashMap<String, String> = new_defs
        .iter()
        .map(|g| (g.name.clone(), g.id.clone()))
        .collect();
    for g in [&trae, &wb, &qoder].into_iter().flatten() {
        if let Some(nid) = name_to_new.get(&g.name) {
            id_map.insert(g.id.clone(), nid.clone());
            continue;
        }
        let rand = uuid::Uuid::new_v4().simple().to_string();
        let nid = format!("shg_{}{}", chrono::Local::now().timestamp_millis(), &rand[..4]);
        name_to_new.insert(g.name.clone(), nid.clone());
        id_map.insert(g.id.clone(), nid.clone());
        new_defs.push(Group { id: nid, name: g.name.clone(), color: g.color.clone(), order: g.order });
    }
    // order 重排（合并后保持出现顺序 1..n）
    for (i, g) in new_defs.iter_mut().enumerate() {
        g.order = (i + 1) as i32;
    }

    // 3. id_map 全量重定向（逐条 UPDATE，旧 id 不存在时 0 行受影响，幂等无害）
    for (old, new) in &id_map {
        if old == new {
            continue;
        }
        store.with_conn(|c| {
            c.execute(
                "UPDATE group_members SET group_id = ?1 WHERE group_id = ?2",
                rusqlite::params![new, old],
            )?;
            Ok(())
        })?;
    }
    redirect_pool_group_ids(store, "wb_accounts", &id_map)?;
    redirect_pool_group_ids(store, "qoder_accounts", &id_map)?;
    redirect_api_pool_group_ids(store, &id_map)?;

    // 4. 全删全写统一 defs；5. 删除三套私有 kv
    docs::group_defs_save(store, &new_defs)?;
    store.kv_delete("workbuddy_groups")?;
    store.kv_delete("qoder_groups")?;
    store.kv_set("unified_groups_migrated", &true)?;
    // 一次性启动通知（前端 init 拉取 toast 后即清）：合并结果用户可见，不只落日志
    let _ = store.kv_set(
        "startup_notice",
        &format!(
            "分组已升级为跨应用共享：原 {} 条分组定义合并为 {} 条共享分组（同名自动合并），\
             成员与调度筛选已同步重定向；原定义已备份，可在系统日志查看详情",
            trae.len() + wb.len() + qoder.len(),
            new_defs.len()
        ),
    );
    crate::fs_utils::app_log(
        &store.data_dir,
        &format!(
            "分组统一迁移：{} 条定义合并为 {} 条共享分组（shg_ 新 ID），成员/池筛选引用已全量重定向；原三套定义快照见 kv groups_pre_migration",
            trae.len() + wb.len() + qoder.len(),
            new_defs.len()
        ),
    );
    Ok(new_defs.len())
}

/// 账号池表（wb_accounts/qoder_accounts 行文档，data 内 group_id 字段）重定向
fn redirect_pool_group_ids(
    store: &Store,
    table: &str,
    id_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let rows: Vec<(String, Value)> = store
        .rows_all(table)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, data)| {
            data.get("group_id")
                .and_then(Value::as_str)
                .map(|g| id_map.contains_key(g))
                .unwrap_or(false)
        })
        .map(|(pk, mut data)| {
            if let Some(g) = data.get("group_id").and_then(Value::as_str) {
                let new = id_map.get(g).cloned().unwrap_or_else(|| g.to_string());
                data["group_id"] = json!(new);
            }
            (pk, data)
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    for (pk, data) in &rows {
        let text = serde_json::to_string(data).map_err(|e| format!("序列化失败: {e}"))?;
        store.with_conn(move |c| {
            c.execute(
                &format!("UPDATE {table} SET data = ?1 WHERE pk = ?2"),
                rusqlite::params![text, pk],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// api_pool kv 的 group_ids / wb_group_ids / qoder_group_ids 重定向
fn redirect_api_pool_group_ids(
    store: &Store,
    id_map: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let Some(raw) = store.kv_get_raw("api_pool") else {
        return Ok(());
    };
    let mut v: Value = serde_json::from_str(&raw).map_err(|e| format!("api_pool 解析失败: {e}"))?;
    let mut changed = false;
    for field in ["group_ids", "wb_group_ids", "qoder_group_ids"] {
        if let Some(arr) = v.get_mut(field).and_then(Value::as_array_mut) {
            for item in arr.iter_mut() {
                if let Some(old) = item.as_str() {
                    if let Some(new) = id_map.get(old) {
                        *item = json!(new);
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        let text = serde_json::to_string(&v).map_err(|e| format!("序列化失败: {e}"))?;
        store.kv_set_raw("api_pool", &text)?;
    }
    Ok(())
}

fn import_usage_history_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::usage_history_save(store, v)
}

fn import_sticky_bindings_value(store: &Store, v: &Value) -> Result<(), String> {
    docs::sticky_bindings_save(store, v)
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

fn parse_then(
    path: &Path,
    f: impl FnOnce(Value) -> Result<(), String>,
) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => match f(v) {
                Ok(()) => ImportStatus::Ok,
                Err(e) => ImportStatus::Error(e),
            },
            Err(e) => ImportStatus::Corrupt(format!("JSON 解析失败: {e}")),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

/// pay_status 等 struct 导入的 serde 错误转换（parse_then 闭包用）
fn ser_err(e: serde_json::Error) -> String {
    e.to_string()
}

fn wrap(r: Result<(), String>) -> ImportStatus {
    match r {
        Ok(()) => ImportStatus::Ok,
        Err(e) => ImportStatus::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("twa_migrate_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("conf")).unwrap();
        std::fs::create_dir_all(d.join("data")).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn migrate_imports_moves_and_is_idempotent() {
        let dir = tmp_dir("full");
        // 造三类数据：KV、行文档、列化流水 + 一个损坏文件
        write(&dir.join("conf/app_settings.json"), r#"{"theme":"dark","proxy_port":8899}"#);
        write(&dir.join("data/api_pool.json"), r#"{"enabled_uids":["u1"],"strategy":"expire_first"}"#);
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"a1","UserID":"u1","jwt":"j1"},{"name":"a2","jwt":""}]}"#,
        );
        write(
            &dir.join("data/credits_history.json"),
            r#"{"records":[{"date":"2026-09-15","user_id":"u1","credits":100,"delta":10}]}"#,
        );
        write(&dir.join("data/device_map.json"), r#"{"u1":{"device_id":"924145245134852"}}"#);
        write(&dir.join("data/pay_status.json"), "not-json");
        write(&dir.join("data/workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a1":{"access_token":"t"}}}"#);
        // 遗留根路径兜底
        write(&dir.join("workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a2":{"access_token":"t2"}}}"#);

        let summary = migrate_on_startup(&dir).expect("首次应触发迁移");
        assert!(summary.contains("导入"), "摘要应含导入数: {summary}");
        assert!(summary.contains("损坏"), "摘要应含损坏计数: {summary}");
        assert!(!summary.contains("失败 1"), "不应有失败项: {summary}");

        // 库生成且版本置位
        let store = db(&dir);
        let v = store.with_conn(|c| Ok(schema::user_version(c))).unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);

        // 原文件移入 backup/（保留 conf/data 相对路径前缀）
        assert!(dir.join("data/backup/conf/app_settings.json").exists());
        assert!(dir.join("data/backup/legacy_root/workbuddy_token_store.json").exists());
        assert!(dir.join("data/backup/corrupt/data/pay_status.json").exists());
        assert!(!dir.join("data/checkin_accounts.json").exists());
        // manifest 落盘
        assert!(dir.join("data/backup/migration_manifest.json").exists());

        // 数据验证：accounts 两行、credits_history 一条、kv 生效、wb_tokens 两行（data + legacy）
        let acc = docs::accounts_load(&store);
        assert_eq!(acc.accounts.len(), 2);
        assert_eq!(acc.accounts[0].user_id.as_deref(), Some("u1"));
        assert_eq!(acc.accounts[0].jwt, "j1");
        assert_eq!(acc.accounts[1].user_id, None);
        let ch = docs::credits_history_load(&store);
        assert_eq!(ch.records.len(), 1);
        assert_eq!(ch.records[0].credits, 100);
        let settings: Value = store.kv_get("app_settings");
        assert_eq!(settings["theme"], "dark");
        let tokens = docs::wb_token_store_load(&store);
        assert_eq!(tokens["tokens"]["a1"]["access_token"], "t");
        // 遗留根路径残留不覆盖正牌数据（仅清理，a2 不得混入）
        assert!(tokens["tokens"].get("a2").is_none());

        // 幂等：二次迁移不执行
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_dir_first_boot_creates_empty_db() {
        let dir = tmp_dir("empty");
        let summary = migrate_on_startup(&dir).expect("空目录也应完成建库");
        assert!(summary.contains("导入 0"));
        assert!(dir.join("data/aiwork.sqlite").exists());
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P7 验收：历史 JSON 遗留的重复 user_id 不得卡死迁移（accounts.user_id UNIQUE
    /// 防线去重、保序取首条），更不得让运行期 accounts 表为空。
    #[test]
    fn duplicate_uid_accounts_import_dedups_not_fails() {
        let dir = tmp_dir("dup");
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[
                {"name":"a","UserID":"u1","jwt":"j1"},
                {"name":"b","UserID":"u1","jwt":"j2"},
                {"name":"c","jwt":""}]}"#,
        );
        let summary = migrate_on_startup(&dir).expect("迁移应完成");
        assert!(!summary.contains("失败 1"), "重复 uid 不得计为失败: {summary}");
        let store = db(&dir);
        let acc = docs::accounts_load(&store);
        assert_eq!(acc.accounts.len(), 2, "重复 uid 去重，占位账号保留");
        assert_eq!(acc.accounts[0].name, "a");
        assert_eq!(acc.accounts[0].jwt, "j1", "保序取首条");
        assert_eq!(acc.accounts[1].name, "c");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P5 安全验收：store 迁移先于 vault 迁移时，JSON 中的明文凭据先入库、
    /// 再由 vault::migrate_on_startup 收敛进 Stronghold 并从库中占位化抹除。
    /// （main.rs 实际调用顺序与此测试一致；红线「vault 写失败禁止明文落盘」的库侧等价物）
    #[test]
    fn plaintext_converges_into_vault_after_store_migration() {
        use crate::state::AppState;
        let dir = tmp_dir("vault_conv");
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"a1","UserID":"u1","jwt":"secret-jwt","refresh_token":"secret-rt"}]}"#,
        );

        // ① store 迁移（main.rs 顺序：先于 vault）
        migrate_on_startup(&dir);
        // 入库后此刻仍为明文（待 vault 收敛）
        let store = db(&dir);
        let mid = super::super::docs::accounts_load(&store);
        assert_eq!(mid.accounts[0].jwt, "secret-jwt", "store 迁移保真导入");

        // ② vault 迁移（ AppState 手工构造，同 doubao_session 测试模式）
        let state = AppState {
            data_dir: dir.clone(),
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        crate::vault::migrate_on_startup(&state);

        // ③ 库中凭据已占位化抹除（明文只存在于 vault）
        let after = super::super::docs::accounts_load(&store);
        assert_eq!(after.accounts.len(), 1);
        assert_eq!(after.accounts[0].jwt, "", "jwt 必须被占位化抹除");
        assert!(after.accounts[0].refresh_token.is_none(), "refresh_token 必须被抹除");
        assert_eq!(after.accounts[0].name, "a1", "非敏感字段保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3→v4 验收：三套分组定义统一为 shg_ 共享分组——同名合并、旧 id 全量重定向
    /// （group_members / WB / Qoder 账号字段 / api_pool 筛选）、私有 kv 清除、
    /// 备份键可回滚、二次调用幂等。
    #[test]
    fn unified_groups_migration_merges_redirects_and_is_idempotent() {
        use crate::models::Group;
        let dir = tmp_dir("unified_groups");
        let store = db(&dir);

        // 种子：Trae g_1「主号」；Buddy wbg_1「主号」（同名合并）+ wbg_2「备用」；
        // Qoder qoderg_1「备用」；成员关系与 api_pool 引用齐全
        docs::group_defs_save(
            &store,
            &[Group { id: "g_1".into(), name: "主号".into(), color: "#f00".into(), order: 1 }],
        )
        .unwrap();
        store
            .kv_set(
                "workbuddy_groups",
                &vec![
                    Group { id: "wbg_1".into(), name: "主号".into(), color: "#0f0".into(), order: 1 },
                    Group { id: "wbg_2".into(), name: "备用".into(), color: "#00f".into(), order: 2 },
                ],
            )
            .unwrap();
        store
            .kv_set(
                "qoder_groups",
                &vec![Group { id: "qoderg_1".into(), name: "备用".into(), color: "#ff0".into(), order: 1 }],
            )
            .unwrap();
        store
            .with_conn(|c| {
                c.execute("INSERT INTO group_members(uid, group_id) VALUES('u1','g_1')", [])?;
                Ok(())
            })
            .unwrap();
        docs::wb_pool_save(&store, &json!({"accounts": [
            {"id": "wb-1", "group_id": "wbg_1"},
            {"id": "wb-2", "group_id": "wbg_2"}
        ]}))
        .unwrap();
        docs::qoder_pool_save(&store, &json!({"accounts": [
            {"id": "qd-1", "group_id": "qoderg_1"}
        ]}))
        .unwrap();
        store
            .kv_set_raw(
                "api_pool",
                r#"{"group_ids":["g_1"],"wb_group_ids":["wbg_2"],"qoder_group_ids":["qoderg_1"]}"#,
            )
            .unwrap();

        // 执行迁移
        let n = migrate_unified_groups(&store).unwrap();
        assert_eq!(n, 2, "同名「主号」合并 → 主号 + 备用 两条");

        // defs 全部 shg_ 前缀；私有 kv 清除
        let defs = docs::group_defs_load(&store);
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().all(|g| g.id.starts_with("shg_")), "统一新 ID: {:?}", defs);
        assert!(store.kv_get_raw("workbuddy_groups").is_none());
        assert!(store.kv_get_raw("qoder_groups").is_none());
        assert_eq!(store.kv_get::<bool>("unified_groups_migrated"), true);
        // 备份键可回滚
        let backup: Value = store.kv_get("groups_pre_migration");
        assert_eq!(backup["groups"][0]["id"], "g_1");
        assert_eq!(backup["workbuddy_groups"].as_array().unwrap().len(), 2);
        // 一次性启动通知已写入（前端 init 拉取 toast 后即清）
        let notice: Option<String> = store.kv_get("startup_notice");
        let notice = notice.unwrap();
        assert!(notice.contains("2 条共享分组"), "通知应含合并结果: {notice}");

        // 旧 id 引用零残留：group_members / WB / Qoder / api_pool 均重定向到对应新 id
        let name_new: std::collections::HashMap<String, String> =
            defs.iter().map(|g| (g.name.clone(), g.id.clone())).collect();
        let main_new = &name_new["主号"];
        let alt_new = &name_new["备用"];
        let m: std::collections::HashMap<String, String> = store
            .with_conn(|c| {
                let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.into_iter().collect())
            })
            .unwrap();
        assert_eq!(m.get("u1").unwrap(), main_new);
        let wb = docs::wb_pool_load(&store);
        assert_eq!(wb["accounts"][0]["group_id"], json!(main_new));
        assert_eq!(wb["accounts"][1]["group_id"], json!(alt_new));
        let qd = docs::qoder_pool_load(&store);
        assert_eq!(qd["accounts"][0]["group_id"], json!(alt_new));
        let pool: Value = store.kv_get("api_pool");
        assert_eq!(pool["group_ids"], json!([main_new]));
        assert_eq!(pool["wb_group_ids"], json!([alt_new]));
        assert_eq!(pool["qoder_group_ids"], json!([alt_new]));

        // 幂等：二次调用跳过
        assert_eq!(migrate_unified_groups(&store).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v3→v4 容错验收（审查修复）：上次中途失败残留的 shg_ 定义（重定向已生效、
    /// defs 全删全写失败、标记未置位）重试时必须按 name 复用既有 shg_ id——
    /// 不得重新生成新 id，否则已重定向的成员/账号引用悬空。
    #[test]
    fn unified_groups_migration_resumes_from_partial_failure() {
        use crate::models::Group;
        let dir = tmp_dir("unified_groups_resume");
        let store = db(&dir);
        // 模拟残留态：shg_ 定义已在 groups 表、成员与 WB 账号已指向 shg_A，
        // 但旧 kv 定义仍在（上次 defs 全删全写失败）、迁移标记未置位
        docs::group_defs_save(
            &store,
            &[Group { id: "shg_A".into(), name: "主号".into(), color: "#f00".into(), order: 1 }],
        )
        .unwrap();
        store
            .kv_set(
                "workbuddy_groups",
                &vec![Group { id: "wbg_1".into(), name: "主号".into(), color: "#0f0".into(), order: 1 }],
            )
            .unwrap();
        store
            .with_conn(|c| {
                c.execute("INSERT INTO group_members(uid, group_id) VALUES('u1','shg_A')", [])?;
                Ok(())
            })
            .unwrap();
        docs::wb_pool_save(&store, &json!({"accounts": [{"id": "wb-1", "group_id": "shg_A"}]}))
            .unwrap();

        // 重试迁移：应复用 shg_A（同名对齐），不生成新 id
        let n = migrate_unified_groups(&store).unwrap();
        assert_eq!(n, 1, "残留 shg_ 定义 + 同名旧定义合并 → 仍 1 条");
        let defs = docs::group_defs_load(&store);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "shg_A", "必须复用残留 shg_ id，防引用悬空");
        // 已重定向的引用仍有效
        let m: Vec<(String, String)> = store
            .with_conn(|c| {
                let mut stmt = c.prepare("SELECT uid, group_id FROM group_members")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(m, vec![("u1".into(), "shg_A".into())], "成员引用未悬空");
        let wb = docs::wb_pool_load(&store);
        assert_eq!(wb["accounts"][0]["group_id"], json!("shg_A"), "WB 账号引用未悬空");
        assert!(store.kv_get_raw("workbuddy_groups").is_none());

        // 幂等：二次调用跳过
        assert_eq!(migrate_unified_groups(&store).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
