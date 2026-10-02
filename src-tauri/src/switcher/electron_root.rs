//! Electron 根级 Chromium 布局快照（Qoder Work 独立客户端，2026-10-02 实测：
//! 数据目录 %APPDATA%\com.qodercn.app.stable，默认会话位于 userData 根——
//! 无 Default/Profile N 子目录，与豆包 chromium 布局（多 Profile 子目录）不同构，
//! 单独成管线）。
//!
//! 快照白名单（相对 userData 根）：
//!   必选  Local State（cookie 解密密钥元数据，缺失则恢复后 cookie 无法解密）、
//!         Network/Cookies*（登录会话）
//!   建议  Local Storage/leveldb/（web 侧登录/偏好 KV）、Session Storage/、
//!         Preferences、Shared Dictionary/（Chromium 130+ 共享字典库）
//!   元数据 snapshot_meta.json（C3 对齐）：schemaVersion + 布局标记
//!
//! 注：Qoder Work 未实测出 machineid 类指纹文件，§5.10 指纹注入对本布局不生效
//!（machine_id_override 恒 None），多账号隔离完全依赖 Cookie/Local Storage 快照。

use serde_json::json;

use super::copy::{copy_snapshot_item, resolve_slot, rotate_bak};
use super::{ProgressSink, Session, StepStatus};

/// 根级白名单条目（文件或目录，Copy-SnapshotItem 语义自适应）
const ROOT_ITEMS: [&str; 6] = [
    "Local State",
    "Network",
    "Local Storage",
    "Session Storage",
    "Preferences",
    "Shared Dictionary",
];

fn write_meta(slot_dir: &std::path::Path) -> Result<(), String> {
    let meta = json!({
        "schemaVersion": 1,
        "layout": "electron-root",
        "created_at": crate::fs_utils::now_iso(),
    });
    std::fs::write(
        slot_dir.join("snapshot_meta.json"),
        serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// 备份（根级白名单整项拷贝；Local State 与 Cookies 均缺失视为应用从未登录/启动）。
/// 身份防线说明：electron_root L2 守卫依赖 Cookies qoderuid 解密预探测（缺失时
/// fail-open），因此覆盖已有槽位前必须 rotate_bak 两代轮转，误覆盖可回退 .bak/.bak2
/// （对齐 icube/chromium/authfile 三布局）。
/// 2026-10-02 审查修复：未登录预检（Local State/Cookies 双缺）前移到 rotate_bak
/// 之前——否则失败路径已把上一代好快照轮转出主槽（resolve_slot 虽有 .bak 回退，
/// 连续两次失败会推到 .bak2，找回链路变长）；拷贝后二次校验保留为防御（文件
/// 拷贝中途消失等罕见态）。
pub fn backup_electron_root(sess: &Session, slot: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let src = sess.prof.data_dir.clone();
    if !src.exists() {
        return Err(format!(
            "{} 数据目录不存在（{}），应用可能从未启动",
            sess.prof.app_name,
            src.display()
        ));
    }
    if !src.join("Local State").exists() && !src.join("Network").join("Cookies").exists() {
        return Err(format!(
            "{} 数据目录中未发现 Local State / Cookies（可能未登录），已取消备份",
            sess.prof.app_name
        ));
    }
    let dest = sess.prof.profiles_dir.join(slot);
    rotate_bak(&dest, slot, sink);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let mut copied = 0usize;
    for item in ROOT_ITEMS {
        if copy_snapshot_item(&src.join(item), &dest.join(item)) {
            copied += 1;
        }
    }
    if !dest.join("Local State").exists() && !dest.join("Network").join("Cookies").exists() {
        // 清理半成品快照，避免空槽位污染快照列表
        let _ = std::fs::remove_dir_all(&dest);
        return Err(format!(
            "{} 数据目录中未发现 Local State / Cookies（可能未登录），已取消备份",
            sess.prof.app_name
        ));
    }
    write_meta(&dest)?;
    sink.step(
        "backup",
        StepStatus::Ok,
        &format!("已备份当前登录态到 {slot} ({copied} 项)"),
    );
    Ok(())
}

/// 恢复前完整性校验：leveldb CURRENT → MANIFEST 指向校验 + Cookies 存在性警告
///（对齐 chromium 布局 C3，单会话无 Profile 指针修复需求）
fn test_snapshot_integrity(src: &std::path::Path, app: &str, sink: &dyn ProgressSink) -> Result<(), String> {
    let ls_current = src.join("Local Storage").join("leveldb").join("CURRENT");
    if ls_current.exists() {
        let pointee = std::fs::read_to_string(&ls_current)
            .map_err(|e| e.to_string())?
            .trim()
            .to_string();
        let manifest = src.join("Local Storage").join("leveldb").join(&pointee);
        if !pointee.is_empty() && !manifest.exists() {
            return Err(format!(
                "快照 leveldb 损坏：CURRENT 指向的 {pointee} 不在快照内（槽位 {}）",
                src.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    if !src.join("Network").join("Cookies").exists() {
        sink.step(
            "restore",
            StepStatus::Warn,
            &format!("快照内未检测到 Cookies——该快照可能保存的是未登录状态，恢复后 {app} 将未登录"),
        );
    }
    Ok(())
}

/// 恢复（对称回写白名单项；Local State 必随 Cookies 一起回写，否则 cookie 无法解密）。
/// 恢复项计数写入 sess.last_restored_count（switch_flow 恢复后校验用，对齐 icube）
pub fn restore_electron_root(
    sess: &mut Session,
    slot: &str,
    sink: &dyn ProgressSink,
) -> Result<(), String> {
    let (src, _slot_label) = resolve_slot(sess, slot, sink)?;
    test_snapshot_integrity(&src, sess.prof.app_name, sink)?;
    let dest = sess.prof.data_dir.clone();
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let mut restored = 0usize;
    for item in ROOT_ITEMS {
        if copy_snapshot_item(&src.join(item), &dest.join(item)) {
            restored += 1;
        }
    }
    sess.last_restored_count = restored as i64;
    sink.step(
        "restore",
        StepStatus::Ok,
        &format!("已恢复账号 {slot} 的登录态 ({restored} 项)"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::test_io_lock;

    struct QuietSink;
    impl ProgressSink for QuietSink {
        fn step(&self, _: &str, _: StepStatus, _: &str) {}
    }

    /// 备份→恢复往返：根级白名单条目（Local State / Network/Cookies / leveldb）完整落快照并对称回写
    #[test]
    fn backup_restore_roundtrip_root_items() {
        let _guard = test_io_lock();
        let base = std::env::temp_dir().join(format!("sw-electron-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data_dir = base.join("appdata");
        let profiles = base.join("profiles");
        std::fs::create_dir_all(data_dir.join("Network")).unwrap();
        std::fs::create_dir_all(data_dir.join("Local Storage").join("leveldb")).unwrap();
        std::fs::write(data_dir.join("Local State"), "{}").unwrap();
        std::fs::write(data_dir.join("Network").join("Cookies"), "cookie").unwrap();
        std::fs::write(data_dir.join("Local Storage").join("leveldb").join("CURRENT"), "MANIFEST-000001").unwrap();
        std::fs::write(data_dir.join("Local Storage").join("leveldb").join("MANIFEST-000001"), "m").unwrap();

        // 直接构造 Session 太重（依赖 profile_for），借 RunArgs → Session 走公共入口
        let args = crate::switcher::RunArgs {
            action: crate::switcher::Action::BackupCurrent,
            target_app: crate::switcher::TargetApp::QoderWork,
            user_id: Some("qd-test".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        {
            let mut sess = crate::switcher::Session::new(&args);
            // 测试注入：数据目录/快照根指向临时路径
            sess.prof.data_dir = data_dir.clone();
            sess.prof.profiles_dir = profiles.clone();
            backup_electron_root(&sess, "qd-test", &QuietSink).unwrap();
            let slot = profiles.join("qd-test");
            assert!(slot.join("Local State").exists());
            assert!(slot.join("Network").join("Cookies").exists());
            assert!(slot.join("snapshot_meta.json").exists());
            // 恢复：清空数据目录后回写
            std::fs::remove_dir_all(&data_dir).unwrap();
            sess.prof.data_dir = data_dir.clone();
            restore_electron_root(&mut sess, "qd-test", &QuietSink).unwrap();
            assert!(data_dir.join("Local State").exists());
            assert!(data_dir.join("Network").join("Cookies").exists());
            assert!(data_dir.join("Local Storage").join("leveldb").join("MANIFEST-000001").exists());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 未登录（无 Local State 且无 Cookies）→ 备份报错且不残留半成品槽位
    #[test]
    fn backup_rejects_when_never_logged_in() {
        let _guard = test_io_lock();
        let base = std::env::temp_dir().join(format!("sw-electron-root-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data_dir = base.join("appdata");
        let profiles = base.join("profiles");
        std::fs::create_dir_all(&data_dir).unwrap();
        let args = crate::switcher::RunArgs {
            action: crate::switcher::Action::BackupCurrent,
            target_app: crate::switcher::TargetApp::QoderWork,
            user_id: Some("qd-test".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        let mut sess = crate::switcher::Session::new(&args);
        sess.prof.data_dir = data_dir;
        sess.prof.profiles_dir = profiles.clone();
        let err = backup_electron_root(&sess, "qd-test", &QuietSink).unwrap_err();
        assert!(err.contains("未发现 Local State / Cookies"), "实际: {err}");
        assert!(!profiles.join("qd-test").exists(), "半成品快照应被清理");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 二次保存触发两代轮转：<slot>.bak 保留上一代快照（Work 无 live 身份守卫，
    /// 误覆盖场景下 .bak 是唯一找回手段——对齐 icube/chromium/authfile）
    #[test]
    fn second_backup_rotates_previous_slot() {
        let _guard = test_io_lock();
        let base = std::env::temp_dir().join(format!("sw-electron-root-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data_dir = base.join("appdata");
        let profiles = base.join("profiles");
        std::fs::create_dir_all(data_dir.join("Network")).unwrap();
        std::fs::write(data_dir.join("Local State"), "v1").unwrap();
        std::fs::write(data_dir.join("Network").join("Cookies"), "cookie-v1").unwrap();
        let args = crate::switcher::RunArgs {
            action: crate::switcher::Action::BackupCurrent,
            target_app: crate::switcher::TargetApp::QoderWork,
            user_id: Some("qd-test".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        let mut sess = crate::switcher::Session::new(&args);
        sess.prof.data_dir = data_dir.clone();
        sess.prof.profiles_dir = profiles.clone();
        backup_electron_root(&sess, "qd-test", &QuietSink).unwrap();
        // 模拟登录态变化后再次保存
        std::fs::write(&data_dir.join("Local State"), "v2").unwrap();
        backup_electron_root(&sess, "qd-test", &QuietSink).unwrap();
        assert_eq!(
            std::fs::read_to_string(profiles.join("qd-test").join("Local State")).unwrap(),
            "v2",
            "主槽应为最新快照"
        );
        assert_eq!(
            std::fs::read_to_string(profiles.join("qd-test.bak").join("Local State")).unwrap(),
            "v1",
            ".bak 应保留上一代快照"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 2026-10-02 审查修复回归：未登录预检（Local State/Cookies 双缺）必须发生在
    /// rotate_bak 之前——失败路径不得把主槽/.bak 的既有快照轮转出去（否则连续两次
    /// 失败会把好快照推到 .bak2，找回链路变长）
    #[test]
    fn backup_precheck_preserves_existing_slots_on_failure() {
        let _guard = test_io_lock();
        let base = std::env::temp_dir().join(format!("sw-electron-root-pre-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data_dir = base.join("appdata");
        let profiles = base.join("profiles");
        // 数据目录存在但为「未登录」态（无 Local State / 无 Network\Cookies）
        std::fs::create_dir_all(&data_dir).unwrap();
        // 预置既有好快照：主槽 + .bak
        std::fs::create_dir_all(profiles.join("qd-test").join("Network")).unwrap();
        std::fs::write(profiles.join("qd-test").join("Local State"), "good").unwrap();
        std::fs::write(profiles.join("qd-test").join("Network").join("Cookies"), "c-good").unwrap();
        std::fs::create_dir_all(profiles.join("qd-test.bak")).unwrap();
        std::fs::write(profiles.join("qd-test.bak").join("Local State"), "old").unwrap();

        let args = crate::switcher::RunArgs {
            action: crate::switcher::Action::BackupCurrent,
            target_app: crate::switcher::TargetApp::QoderWork,
            user_id: Some("qd-test".into()),
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            machine_id_override: None,
            data_dir: base.clone(),
        };
        let mut sess = crate::switcher::Session::new(&args);
        sess.prof.data_dir = data_dir;
        sess.prof.profiles_dir = profiles.clone();
        let err = backup_electron_root(&sess, "qd-test", &QuietSink).unwrap_err();
        assert!(err.contains("未发现 Local State / Cookies"), "实际: {err}");
        // 关键断言：主槽与 .bak 内容原样保留（未被轮转/清空）
        assert_eq!(
            std::fs::read_to_string(profiles.join("qd-test").join("Local State")).unwrap(),
            "good",
            "未登录预检失败时主槽快照不得被轮转"
        );
        assert_eq!(
            std::fs::read_to_string(profiles.join("qd-test.bak").join("Local State")).unwrap(),
            "old",
            ".bak 亦不得被波及"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
