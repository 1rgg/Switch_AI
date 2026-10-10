# 上游 v3.7.4 缺陷排查与修复回填报告

对照仓库：`smart-open/TraeWorkAssistant` 的 `v3.7.4`（自 `v3.7.3` 以来的变更批）。
本仓库：`1rgg/Switch_AI`（二次开发分支）。

## 一、结论：这些问题**确实存在**

本分支相对上游独立演进，**未合并上游 v3.7.3 / v3.7.4 的若干修复**（代码根因逐条验证在案）：

| # | 上游问题（v3.7.4 发布说明） | 本仓库是否存在 | 依据 |
|---|---|---|---|
| 1 | 上游错误空 message 透传 `{"message":""}`（issue #71） | ✅ 存在 | `openai_error` / `anthropic_error` / `send_stream_error` / `sse.rs` / `wb_sse.rs` 均直接透传上游 message，空串原样下发 |
| 2 | 账号池读改写无互斥 → lost-update 丢账号 | ✅ 存在 | `AppState` 无 `wb_pool_lock` / `doubao_pool_lock`；WB/豆包池十余处「load→改→save」并发无保护 |
| 3 | Buddy Token 统计重复计算（虚高约 13.8%） | ✅ 存在 | `PARSE_REV=3`，仅「同会话内」去重；同一批请求以新会话 id 落到另一 uid 目录下会整份多计 |
| 4 | Responses→Chat 投影工具结果配对错误（issue #69，400 `11148`） | ✅ 存在 | `convert_input_items` 为 1:1 直投影，`developer` 消息降级为 `user` 会插进 `tool_calls` 与结果之间 |
| 5 | PE 版本读取 Translation 字节序错误 → 版本号显示为构建号 | ✅ 存在 | 无 `pe_version.rs`；`version_of` 每次 spawn powershell，且走的是 PowerShell 路径（构建号问题在 Electron 系客户端同样命中） |
| 6 | device_map 缺条目时发送空 `x-device-id` | ✅ 存在 | `pool.rs` 回落 `String::new()`，形成显性指纹异常 |
| 7 | 代理日志分片字典序错排（`.log.10` 排到 `.log.2` 前） | ✅ 存在 | `proxy_log_files` 用 `files.sort()`，且列表/详情白名单只收 `.log` 不收 `.log.N` |

## 二、已修复内容（本分支已落地）

共改动 28 个文件 + 新增 1 个文件，`cargo check --all-targets` 无 error，`cargo test` **820 passed / 0 failed**（基线 807，新增 13 条回归用例全部通过）。

1. **空 message 兜底（issue #71）**
   - 新增 `routes::msg_or_fallback` / `stream_msg_or_fallback`（HTTP 语义 code 用状态码文案，业务码不冒充状态码）。
   - 接入 `openai_error` / `anthropic_error` / `send_stream_error`，以及 `sse.rs`（3 处 inline）、`wb_sse.rs`、`qoder_route.rs`、`wb_route.rs` 的流式错误帧。

2. **账号池双池互斥防 lost-update**
   - `AppState` 新增 `wb_pool_lock` / `doubao_pool_lock`，全部 `AppState` 构造点已同步。
   - WB 池写点：`workbuddy_account_save/remove/import_auth/accounts_import`、`groups_remove`、`account_move`、`oauth_flow`、`credits::write_back_pool_balances`、`wb_checkin::sync_pool_expiry`、`wb_common::mark_needs_relogin`。
   - 豆包池写点：`doubao_account_save/remove`、`keepalive_run`、`update_quota_cache`、`set_credential`、`credential_auto_apply`、`vault::migrate_ns_on_startup`。
   - 两阶段回写（锁外网络 + 短临界区按 uid 字段级合并，删号不复活）：`workbuddy_refresh_token`、`credits::backfill_edition_from_payment_type`、`doubao_quota::run_batch`（`merge_quota_updates_into_pool`）、`doubao_session::run_renewal`（`merge_diffs_into_pool`）。

3. **Buddy Token 统计跨文件去重**
   - `PARSE_REV 3→4`；`FileCacheEntry` 增 `ids`；`parse_codebuddy_index` 增 `skip` 参数；`aggregate_files` 按「实际计入的 request id」跨文件去重（整份重复整文件跳过 / 部分重复过滤重解析）。

4. **Responses→Chat 投影配对重写（issue #69）**
   - `convert_input_items` 改为配对窗口算法：连续 `function_call` 合并进同一 assistant、窗口内非 tool 消息推迟补回、结果按 `tool_calls` 顺序统一回填、缺失补占位；新增 `emit_tool_results` / `flush_deferred` / `append_assistant_text`。
   - `tool_output_text`：tool 输出只取 text，非文本折叠为占位符，避免 data URL 撑爆载荷。

5. **PE 版本直读 + Translation 字节序修复**
   - 新增 `src-tauri/src/pe_version.rs`（VERSIONINFO 直读，低 16 位为语言 ID）；`Cargo.toml` 补 `Win32_Storage_FileSystem` feature；`main.rs` 注册模块。
   - `env.rs::version_of` 改为三级：`(路径, mtime, 大小)` 缓存 → PE 直读 → powershell 兜底（保留原实现）。

6. **device_id 非空派生**：`pool.rs` 缺 `device_map` 条目时回落 `derive_device(uid).device_id`，不再发空 `x-device-id`。

7. **代理日志分片数值排序**：新增 `proxy_log_shard_key` / `is_log_shard_day`，列表与详情白名单统一按「(日期, 分片序号) 数值」排序与放行。

## 三、未回填项（纯性能/诊断，未纳入本次缺陷批次）

- 概览页「切板块即显」的其余两项：进程运行态改 sysinfo 枚举、注册表兜底 3s TTL（需在 `switcher/proc.rs` 新增 `any_running` / `running_exe_exact`）。
- Buddy 积分取数账号间并发（≤4 路）+ stale-while-revalidate + 单账号 panic `catch_unwind` 兜底 / `IN_FLIGHT` Drop 守卫。
- 空完成取证：`make_upstream_request` 返回上游响应头摘要（logid 等）用于申诉（诊断增强）。

如需一并回填，可作为后续批次处理。

## 四、验证

```
cd /d/code/switch/Switch_AI
export PATH="$HOME/.cargo/bin:$PATH"
RUSTFLAGS='--cfg has_std' cargo check --manifest-path src-tauri/Cargo.toml --all-targets   # 无 error
RUSTFLAGS='--cfg has_std' cargo test  --manifest-path src-tauri/Cargo.toml                   # 820 passed / 0 failed
```

> 本地编译需 `RUSTFLAGS='--cfg has_std'` 绕过本机 `indexmap 1.9.3` 的 `autocfg` 探测失败（仅影响本地，CI 正常）。
