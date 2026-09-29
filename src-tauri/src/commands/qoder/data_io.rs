//! Qoder 账号池导出/导入（F-80 M4，对照 WorkBuddy F-46 扩展同语义）。
//!
//! 导出：`kind: "aiwork-qoder-pool"` + version 强校验；include_credentials 开关
//! （池/凭证分离，true 时整凭证对象作 `credential` 附带——导出文件等同密码）。
//! 导入：id 三重校验（非空/ensure_uid_safe/qd-<12位十六进制小写>）→
//! find_uid_or_id 幂等原位更新（uid 优先 id 兜底）→ credential 回写 token store。
//!
//! 与 WorkBuddy 的关键差异（device_profile 指纹红线，§5.10）：指纹入池生成一次
//! 永不轮换——命中已有账号时 device_profile 仅在本地为空才补入，绝不覆盖
//! （覆盖等于轮换指纹）；新增账号时采用导出文件携带的指纹。

use serde_json::Value;
use tauri::State;

use super::common::{load_pool, load_pool_checked, save_pool, QoderAccount};
use crate::fs_utils;
use crate::state::AppState;

// ── 导出 ────────────────────────────────────────────────────────────────────

/// 导出账号池：元数据必含；include_credentials=true 时附工具侧凭证副本
/// （迁移场景用；导出文件等同密码，由前端提示）。池字段全量导出
/// （device_profile 恒带，供异机导入沿用同一指纹）。
#[tauri::command(async)]
pub fn qoder_accounts_export(
    state: State<AppState>,
    include_credentials: Option<bool>,
) -> Result<Value, String> {
    let pool = load_pool(&state);
    let include_cred = include_credentials.unwrap_or(false);
    let tokens = if include_cred {
        crate::tasks::qoder_common::load_token_store(&state)
            .get("tokens")
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let accounts: Vec<Value> = pool
        .iter()
        .map(|a| {
            let mut v = serde_json::to_value(a).map_err(|e| format!("序列化失败: {e}"))?;
            if include_cred {
                v["credential"] = tokens.get(&a.id).cloned().unwrap_or(Value::Null);
            }
            Ok(v)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(serde_json::json!({
        "kind": "aiwork-qoder-pool",
        "version": 1,
        "exported_at": fs_utils::now_iso(),
        "include_credentials": include_cred,
        "accounts": accounts,
    }))
}

// ── 导入 ────────────────────────────────────────────────────────────────────

/// 单账号合并结果：Ok((生效 id, 是否新增))；Err = 拒绝原因。
type MergeResult = Result<(String, bool), String>;

/// 单账号幂等合并（纯函数，可单测）：uid 优先 id 兜底原位更新，保留原 id
/// （换 token / 异机 id 的同账号不再产生重复条目，分组引用不悬空）。
///
/// 覆盖规则：
/// - 更新：uid/nickname/phone_masked/plan/credential_source/token_expires_at
///   非空（Some）才覆盖；group_id/note 本地可编辑不覆盖；needs_relogin/
///   credits_* 属本地运行态不覆盖；device_profile 仅本地为空才补入（指纹红线）
/// - 新增：导出文件字段全量采用（含 group_id/note/device_profile）
fn merge_account(pool: &mut Vec<QoderAccount>, a: &Value) -> MergeResult {
    let parsed: QoderAccount = serde_json::from_value(a.clone())
        .map_err(|e| format!("账号字段解析失败: {e}"))?;
    let id = parsed.id.clone();
    if id.is_empty() {
        return Err("缺少 id".into());
    }
    // 字符集白名单，杜绝 `..`/绝对路径/分隔符注入
    if let Err(e) = fs_utils::ensure_uid_safe(&id) {
        return Err(e);
    }
    // id 规则校验：须与池内生成规则一致——qd-<sha256 前 12 位十六进制小写>
    let hex = id.strip_prefix("qd-").unwrap_or("");
    if hex.len() != 12 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("id 不符合 qd-<12位十六进制小写> 规则".into());
    }
    // find_uid_or_id：uid 优先，id 兜底（两次顺序查找，避免同时借用 pool 两次）
    let hit = if !parsed.uid.is_empty() {
        pool.iter_mut().find(|x| x.uid == parsed.uid)
    } else {
        None
    };
    let hit = match hit {
        Some(x) => Some(x),
        None => pool.iter_mut().find(|x| x.id == id),
    };
    if let Some(x) = hit {
        if !parsed.uid.is_empty() {
            x.uid = parsed.uid.clone();
        }
        if !parsed.nickname.is_empty() {
            x.nickname = parsed.nickname.clone();
        }
        if !parsed.phone_masked.is_empty() {
            x.phone_masked = parsed.phone_masked.clone();
        }
        if !parsed.plan.is_empty() {
            x.plan = parsed.plan.clone();
        }
        if !parsed.credential_source.is_empty() {
            x.credential_source = parsed.credential_source.clone();
        }
        if parsed.token_expires_at.is_some() {
            x.token_expires_at = parsed.token_expires_at;
        }
        // 指纹红线：本地为空才补入，绝不覆盖已有档案（覆盖 = 轮换指纹）
        if x.device_profile.is_none() {
            x.device_profile = parsed.device_profile.clone();
        }
        return Ok((x.id.clone(), false));
    }
    pool.push(parsed);
    Ok((id, true))
}

/// 账号池导入：解析导出文件 → 逐账号幂等入池 + 凭证回写 token store
/// （含凭证时按生效 id 写回，与池条目对齐）。
#[tauri::command(async)]
pub fn qoder_accounts_import(state: State<AppState>, payload: Value) -> Result<Value, String> {
    if payload.get("kind").and_then(Value::as_str) != Some("aiwork-qoder-pool") {
        return Err("文件格式无法识别（缺少 aiwork-qoder-pool 标记）".into());
    }
    // 导出方恒写 version:1；导入同样校验（后续格式演进时可按版本分支）
    if payload.get("version").and_then(Value::as_i64) != Some(1) {
        return Err("导出文件版本不识别（version 必须为 1）".into());
    }
    let accounts = payload
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("导出文件缺少 accounts 数组")?;
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut with_cred = 0usize;
    let mut rejected: Vec<Value> = Vec::new();
    // 全程持池锁：签到/刷新/积分回写等通道的「load→改→save」并发时整池覆盖会丢导入。
    // 写路径必须走 checked 版：池存在损坏行时拒绝导入（坏行静默丢弃后 save 会整池
    // 覆盖永久丢账号，违反 common.rs 红线）
    let _guard = state.qoder_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut pool = load_pool_checked(&state)?;
    for a in accounts {
        match merge_account(&mut pool, a) {
            Ok((final_id, is_new)) => {
                if is_new {
                    added += 1;
                } else {
                    updated += 1;
                }
                // 凭证副本回写（导出时含凭证才有效）：至少有作业令牌或 PAT 才落库
                if let Some(cred) = a.get("credential").filter(|c| c.is_object()) {
                    // §5.10 红线纵深：入库前剥离设备字段（指纹只存账号池 device_profile，
                    // token store 不落指纹——与 ensure_fresh 客户端通道落库语义一致）
                    let mut creds = crate::tasks::qoder_common::creds_of(cred);
                    creds.machine_id.clear();
                    creds.machine_token.clear();
                    if !creds.access_token.is_empty() || !creds.pat.is_empty() {
                        // 单账号凭证落库失败不再 `?` 中断整体（部分导入比整体中断更糟）：
                        // 聚合进 rejected 继续导入；账号信息照常入池，重新导入可补齐凭证
                        match crate::tasks::qoder_common::save_token_store(&state, &final_id, &creds) {
                            Ok(()) => with_cred += 1,
                            Err(e) => rejected.push(serde_json::json!({
                                "id": final_id,
                                "reason": format!("账号已入池，但凭证落库失败：{e}（重新导入可补齐）"),
                            })),
                        }
                    }
                }
            }
            Err(reason) => {
                let id = a.get("id").and_then(Value::as_str).unwrap_or("");
                rejected.push(serde_json::json!({ "id": id, "reason": reason }));
            }
        }
    }
    save_pool(&state, &pool)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "qoder: 账号池导入 新增 {added} / 更新 {updated} / 带凭证 {with_cred} / 拒绝 {}",
            rejected.len()
        ),
    );
    Ok(serde_json::json!({
        "added": added,
        "updated": updated,
        "skipped": 0,
        "with_credentials": with_cred,
        "rejected": rejected,
    }))
}

#[cfg(test)]
mod tests {
    use super::merge_account;
    use crate::commands::qoder::common::QoderAccount;
    use crate::tasks::qoder_device::QoderDeviceProfile;

    fn acct(id: &str, uid: &str, nickname: &str) -> QoderAccount {
        QoderAccount {
            id: id.into(),
            uid: uid.into(),
            nickname: nickname.into(),
            ..Default::default()
        }
    }

    fn profile(machine_id: &str) -> QoderDeviceProfile {
        QoderDeviceProfile {
            machine_id: machine_id.into(),
            ..Default::default()
        }
    }

    const GOOD_ID: &str = "qd-0123456789ab";

    #[test]
    fn merge_new_adds_and_adopts_fields() {
        let mut pool = vec![acct("qd-ffffffffffff", "u-old", "旧账号")];
        let payload = serde_json::json!({
            "id": GOOD_ID, "uid": "u-1", "nickname": "张三", "plan": "pro",
            "group_id": "g1", "note": "备注",
            "device_profile": serde_json::to_value(profile("abc123")).unwrap(),
        });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(is_new);
        assert_eq!(final_id, GOOD_ID);
        assert_eq!(pool.len(), 2);
        let x = &pool[1];
        assert_eq!(x.uid, "u-1");
        assert_eq!(x.plan, "pro");
        assert_eq!(x.group_id, "g1");
        assert_eq!(
            x.device_profile.as_ref().unwrap().machine_id,
            "abc123",
            "新增账号采用导出指纹"
        );
    }

    #[test]
    fn merge_hit_by_uid_updates_in_place_keeping_original_id() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "u-1", "本地名")];
        let payload = serde_json::json!({
            "id": GOOD_ID, "uid": "u-1", "nickname": "云端名", "plan": "pro+",
        });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(!is_new);
        assert_eq!(final_id, "qd-aaaaaaaaaaaa", "命中 uid 原位更新保留原 id");
        let x = &pool[0];
        assert_eq!(x.nickname, "云端名");
        assert_eq!(x.plan, "pro+");
        assert_eq!(pool.len(), 1, "不重复入池");
    }

    #[test]
    fn merge_hit_by_id_when_uid_empty() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "", "")];
        let payload = serde_json::json!({ "id": "qd-aaaaaaaaaaaa", "nickname": "云端名" });
        let (final_id, is_new) = merge_account(&mut pool, &payload).unwrap();
        assert!(!is_new);
        assert_eq!(final_id, "qd-aaaaaaaaaaaa");
    }

    #[test]
    fn merge_update_never_overwrites_device_profile() {
        let mut pool = vec![QoderAccount {
            device_profile: Some(profile("local-mid")),
            ..acct("qd-aaaaaaaaaaaa", "u-1", "")
        }];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "device_profile": serde_json::to_value(profile("cloud-mid")).unwrap(),
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(
            pool[0].device_profile.as_ref().unwrap().machine_id,
            "local-mid",
            "指纹红线：本地已有档案绝不覆盖"
        );
    }

    #[test]
    fn merge_update_backfills_missing_device_profile() {
        let mut pool = vec![acct("qd-aaaaaaaaaaaa", "u-1", "")];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "device_profile": serde_json::to_value(profile("cloud-mid")).unwrap(),
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(
            pool[0].device_profile.as_ref().unwrap().machine_id,
            "cloud-mid",
            "本地为空才补入"
        );
    }

    #[test]
    fn merge_update_keeps_local_group_and_note() {
        let mut pool = vec![QoderAccount {
            group_id: "local-g".into(),
            note: "本地备注".into(),
            ..acct("qd-aaaaaaaaaaaa", "u-1", "")
        }];
        let payload = serde_json::json!({
            "id": "qd-aaaaaaaaaaaa", "uid": "u-1",
            "group_id": "cloud-g", "note": "云端备注",
        });
        merge_account(&mut pool, &payload).unwrap();
        assert_eq!(pool[0].group_id, "local-g", "group_id 仅新增采用");
        assert_eq!(pool[0].note, "本地备注", "note 仅新增采用");
    }

    #[test]
    fn merge_rejects_bad_ids() {
        for (id, why) in [
            ("", "缺少 id"),
            ("qd-ZZ0123456789", "非十六进制"),
            ("qd-0123456789", "长度 10 不是 12"),
            ("wb-0123456789ab", "wb 前缀"),
            ("qd-0123456789ab/../x", "路径注入字符"),
        ] {
            let mut pool = Vec::new();
            let payload = serde_json::json!({ "id": id });
            assert!(merge_account(&mut pool, &payload).is_err(), "应拒绝: {why}");
        }
    }
}
