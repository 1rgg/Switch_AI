//! Qoder 命令域（F-80 M1，仿 commands/workbuddy/ 拆分）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐（同 doubao.rs 红线）。
//! 凭证红线：accessToken/PAT 等同密码——不进日志、不进 NDJSON、前端掩码展示。
//!
//! - `common`：账号池/设置读写、环境检测
//! - `accounts`：账号列表/改名/移除/PAT 导入（M1 最可靠凭证通道，M0 R-3 侦察结论）
//! - `checkin`：签到（NDJSON 管线）/ 签到结果 / 定时任务 / 启动补签
//! - `credits`：积分查询 / 快照时序
//! - `data_io`：账号池导出/导入（M4，对照 WorkBuddy F-46 扩展同语义）
//! - `env_reset`：环境重置/彻底登出（M4，对照 WorkBuddy F-14 同语义）

mod accounts;
mod checkin;
mod cli_status;
mod common;
mod credits;
mod data_io;
mod env_reset;
mod ide_store;
mod oauth;

pub use accounts::*;
pub use checkin::*;
pub use cli_status::*;
pub use common::*;
pub use credits::*;
pub use data_io::*;
pub use env_reset::*;
pub use ide_store::*;
pub use oauth::*;
