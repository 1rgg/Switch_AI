//! Qoder 模型目录每日同步（p3-3 收尾接线）。
//!
//! 网关路由判定依赖 [`qoder_upstream`] 目录（`resolve`/`list`）；静态兜底表是
//! 蓝本快照，倍率/名单会漂移（2026-10-01 实抓实证 GLM-5.3 0.6→0.8 等 7 处）。
//! 本任务每日拉取 `model/list`（真 COSY 签名 GET，凭证走 `ensure_fresh` 全防护）
//! → [`qoder_upstream::adopt_remote`] 原子替换 CN 区缓存。
//! - 空池空转；全部账号无凭证（未登录/需重登）静默跳过不计失败；
//! - 网络/服务端失败返 Err 交调度器 30 分钟冷却重试；
//! - 目录是账号无关的全局数据，无需逐账号拉取：按池序逐可用账号尝试，
//!   任一账号成功即收口；单个账号失败记日志后试下一个，全部失败才 Err（P3）。

use serde_json::{json, Value};

use crate::state::AppState;

use super::{http_agent, qoder_common, qoder_sign, qoder_upstream};

/// model/list 路径（拼在 [`qoder_upstream::QoderRegion::gateway`] 后；
/// GET 空体，COSY 签名覆盖见 qoder_sign——sigpath 自动剥离 /algo 前缀与查询串）
const MODEL_LIST_PATH: &str = "algo/api/v2/model/list";

/// 区 model/list 完整 URL（抽纯函数便于单测锚定端点形态）。
///
/// `QoderRegion` 与 `QoderRealm` 在这里做**唯一一次**桥接：前者服务目录/网关
/// 层的双桶缓存，后者服务业务层（签到/凭证/登录）。两者的 Global/Intl 语义一致。
fn model_list_url(region: qoder_upstream::QoderRegion) -> String {
    format!("{}{MODEL_LIST_PATH}", region.gateway())
}

/// 双区桥接：业务区域 → 目录区域
pub fn catalog_region_for(realm: qoder_common::QoderRealm) -> qoder_upstream::QoderRegion {
    match realm {
        qoder_common::QoderRealm::Cn => qoder_upstream::QoderRegion::Cn,
        qoder_common::QoderRealm::Intl => qoder_upstream::QoderRegion::Global,
    }
}

/// 调度器/CLI 共用入口：**逐区**模型目录同步。
///
/// 账号按自身区域分组，各区分别用该区账号拉取该区的 `model/list` 并写入该区
/// 目录缓存——国际版账号的清单与国内版不是同一份，混用会把国际版模型写进国内桶。
pub fn run_task(state: &AppState) -> Result<Value, String> {
    let accounts = crate::commands::qoder::load_pool(state);
    if accounts.is_empty() {
        return Ok(json!({ "ok": true, "skipped": "无 Qoder 账号" }));
    }
    let mut results: Vec<Value> = Vec::new();
    let mut any_ok = false;
    let mut last_err: Option<String> = None;
    for realm in qoder_common::QoderRealm::all() {
        let group: Vec<_> = accounts
            .iter()
            .filter(|a| qoder_common::resolve_realm(&a.region, "") == realm)
            .collect();
        if group.is_empty() {
            continue;
        }
        match run_task_for_realm(state, realm, &group) {
            Ok(v) => {
                // 只有**真的采纳了模型**才算成功；`ok:true + skipped`（无凭证可试）
                // 不是成功——否则会吞掉顶层 skipped 契约（测试钉住）
                if v.get("models").and_then(Value::as_u64).is_some() {
                    any_ok = true;
                }
                results.push(v);
            }
            Err(e) => {
                last_err = Some(e.clone());
                results.push(json!({ "ok": false, "realm": realm.as_str(), "error": e }));
            }
        }
    }
    // 至少一个区域成功同步 → ok（附双区明细）
    if any_ok {
        return Ok(json!({ "ok": true, "realm_results": results }));
    }
    // 没有任何区域同步成功：区分「无凭证可试」（保留既有 skipped 契约：
    // 调度器据此判定为跳过而非失败）与「尝试过但失败」（上抛错误交冷却重试）
    let no_creds = results
        .iter()
        .all(|r| r.get("skipped").and_then(Value::as_str).is_some());
    if no_creds {
        return Ok(json!({
            "ok": true,
            "skipped": "无可用 Qoder 凭证",
            "realm_results": results,
        }));
    }
    Err(last_err.unwrap_or_else(|| "Qoder 目录同步失败".into()))
}

/// 单区域目录同步：按池序逐可用账号尝试，任一成功即收口（P3 语义不变）。
fn run_task_for_realm(
    state: &AppState,
    realm: qoder_common::QoderRealm,
    accounts: &[&crate::commands::qoder::QoderAccount],
) -> Result<Value, String> {
    let region = catalog_region_for(realm);
    let agent = http_agent(20);
    let url = model_list_url(region);
    // P3：单个账号失败不再整体 Err——记日志后试下一个可用账号，全部失败才上抛
    let mut last_err: Option<String> = None;
    for a in accounts {
        let id = a.id.as_str();
        // 24h 惰性窗口（对齐探针/常规任务语义）：临期先刷再用
        let (creds, _refreshed, _note) = qoder_common::ensure_fresh(state, &agent, id, 24);
        // 无凭证/需重登账号跳过，试下一个（needs_relogin 账号的 token 可能已死）
        if creds.access_token.is_empty() || creds.uid.is_empty() {
            continue;
        }
        let identity = qoder_sign::CosyIdentity {
            user_id: &creds.uid,
            auth_token: &creds.access_token,
            name: &creds.nickname,
            email: "",
            machine_id: &creds.machine_id,
        };
        let headers = match qoder_sign::build_cosy_headers(None, &url, &identity) {
            Ok(h) => h,
            Err(e) => {
                last_err = Some(format!("model/list 签名构造失败: {e}"));
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!(
                        "[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}",
                        last_err.as_deref().unwrap_or("")
                    ),
                );
                continue;
            }
        };
        let resp = headers
            .into_iter()
            .fold(agent.get(&url), |r, (k, v)| r.set(&k, &v))
            .call();
        match resp {
            Ok(resp) if resp.status() == 200 => {
                // Q2：响应读取失败 / 非 JSON / 目录采纳失败与签名/HTTP 非 200/网络
                // 失败同款容错——记 last_err 后落循环尾部日志试下一账号，仅在全部
                // 可用账号尝试完毕仍无成功时上抛最后错误（不再因单账号解析失败
                // 提前 Err，与函数头「单个账号失败记日志后试下一个」声明一致）
                match resp
                    .into_string()
                    .map_err(|e| format!("model/list 响应读取失败: {e}"))
                    .and_then(|body| {
                        serde_json::from_str::<Value>(&body)
                            .map_err(|e| format!("model/list 响应非 JSON: {e}"))
                    })
                    .and_then(|payload| {
                        qoder_upstream::adopt_remote(region, &payload)
                            .map_err(|e| format!("model/list 目录采纳失败: {e}"))
                    })
                {
                    Ok(count) => {
                        return Ok(json!({
                            "ok": true, "models": count, "account": id, "realm": realm.as_str(),
                        }))
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Ok(resp) => {
                last_err = Some(format!("model/list HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("model/list 网络失败: {e}"));
            }
        }
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] 账号 {id} 目录同步失败，尝试下一账号: {}", last_err.as_deref().unwrap_or("")),
        );
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(json!({
            "ok": true, "skipped": "无可用 Qoder 凭证", "realm": realm.as_str(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir()
            .join(format!("aiwork_qoder_catalog_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            qoder_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            wb_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            doubao_pool_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        }
    }

    #[test]
    fn url_anchored_to_per_region_gateway() {
        assert_eq!(
            model_list_url(qoder_upstream::QoderRegion::Cn),
            "https://gateway.qoder.com.cn/algo/api/v2/model/list"
        );
        // 国际版网关（api3.qoder.sh）——漏桥接会把国际版目录拉到国内站
        assert_eq!(
            model_list_url(qoder_upstream::QoderRegion::Global),
            "https://api3.qoder.sh/algo/api/v2/model/list"
        );
    }

    /// 业务区域 ↔ 目录区域桥接（唯一桥接点，回归钉）
    #[test]
    fn realm_maps_to_catalog_region() {
        assert_eq!(
            catalog_region_for(qoder_common::QoderRealm::Cn),
            qoder_upstream::QoderRegion::Cn
        );
        assert_eq!(
            catalog_region_for(qoder_common::QoderRealm::Intl),
            qoder_upstream::QoderRegion::Global
        );
    }

    #[test]
    fn empty_pool_skips() {
        let st = temp_state("empty");
        let out = run_task(&st).expect("空池应 Ok");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无 Qoder 账号"));
    }

    #[test]
    fn accounts_without_creds_skip_silently() {
        let st = temp_state("nocred");
        // 池里放一个账号（qoder_accounts 表），但 token store 为空
        // → ensure_fresh 返回 no_credential → 跳过
        crate::store::docs::qoder_pool_save(
            &crate::store::db(&st.data_dir),
            &json!({ "accounts": [ { "id": "qd-testnocred", "nickname": "t" } ] }),
        )
        .expect("写池失败");
        let out = run_task(&st).expect("无凭证应 Ok 跳过");
        assert_eq!(out["ok"], true);
        assert!(out["skipped"].as_str().unwrap_or_default().contains("无可用 Qoder 凭证"));
    }
}
