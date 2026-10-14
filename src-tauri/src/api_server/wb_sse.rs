//! WorkBuddy 上游 SSE 解析与协议输出（T2.1/F-28）
//!
//! WB 上游 `/v2/chat/completions` 只回 OpenAI 风格 SSE（`data: {chunk}` …
//! `data: [DONE]`），与 SOLO 上游的事件格式（event:output/thought/token_usage）
//! 不同，故独立实现：
//! - 流式：OpenAI chunk 归一化后按客户端协议转发（OpenAI / text / Anthropic）；
//! - 非流式：本地聚合为单个 OpenAI completion（tool_calls delta 按 index 合并、
//!   帧归一化），再按协议转换（§3.9 ①）。
//!
//! 输入统一为行迭代器（便于首字超时等上层封装），不直接持有 Reader。

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::wb_upstream::InterruptibleLines;
use crate::api_server::routes::stream_msg_or_fallback;

/// 上游停滞期间的断连轮询间隔：每 500ms 醒来检查一次客户端是否断连
/// （tx.is_closed()），对齐 sse.rs LINE_POLL——避免「断连 + 上游停滞」时
/// 僵尸流死等上游下一行（最长 300s 读超时）、持续占用 WB 账号并发槽
const LINE_POLL: Duration = Duration::from_millis(500);

/// WB 上游单个 SSE 事件（已归一化）
#[derive(Debug)]
pub enum WbEvent {
    /// 标准 OpenAI chunk：choices[0] 的 delta + finish_reason + 顶层 usage
    Chunk {
        delta: Value,
        finish: String,
        usage: Option<Value>,
        /// 客户端请求 id（原样透传）
        id: String,
    },
    Error {
        code: i64,
        msg: String,
    },
    Done,
}

/// 行迭代 → 事件流的解析器（SSE 多行 data 合并；`: ` 注释行跳过）
pub struct WbSseParser<L: Iterator<Item = String>> {
    lines: L,
    /// 跨行 data 缓冲
    data: String,
    done: bool,
}

impl<L: Iterator<Item = String>> WbSseParser<L> {
    pub fn new(lines: L) -> Self {
        Self { lines, data: String::new(), done: false }
    }

    pub fn next_event(&mut self) -> Option<WbEvent> {
        if self.done {
            return None;
        }
        loop {
            let line = self.lines.next()?;
            if let Some(ev) = self.feed_line(&line) {
                return Some(ev);
            }
        }
    }

    /// 单行喂入：返回该行触发的事件（跨行 data 拼接 + 紧凑流兼容 + `:` 注释跳过）。
    /// 供 next_event 与 next_event_polling 共用（逻辑与拆分前逐行等价）
    fn feed_line(&mut self, line: &str) -> Option<WbEvent> {
        let line = line.trim_end();
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let payload = std::mem::take(&mut self.data);
            if let Some(ev) = parse_data(&payload) {
                if matches!(ev, WbEvent::Done) {
                    self.done = true;
                }
                return Some(ev);
            }
            return None;
        }
        if line.starts_with(':') {
            return None; // SSE 注释（keep-alive 等）
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data.push_str(rest.trim_start());
            // 紧凑流兼容（部分上游不按「空行分隔」发帧）：
            // 缓冲已是完整载荷（[DONE] 或完整 JSON）→ 立即产出，不等空行；
            // 否则继续按 SSE 规范做多行 data 拼接
            let t = self.data.trim();
            let complete = t == "[DONE]"
                || (t.starts_with('{') && serde_json::from_str::<Value>(t).is_ok());
            if complete {
                let payload = std::mem::take(&mut self.data);
                if let Some(ev) = parse_data(&payload) {
                    if matches!(ev, WbEvent::Done) {
                        self.done = true;
                    }
                    return Some(ev);
                }
            }
        }
        // 其余行（event:/id:/retry:/未知）忽略：WB 上游标准 OpenAI 流
        None
    }
}

impl WbSseParser<InterruptibleLines> {
    /// 停滞感知取事件（WB 路由流式专用）：行源为可中断行源，上游停滞窗口内
    /// 周期性醒来检查客户端断连（client_gone，通常为 tx.is_closed()）——
    /// 断连即返回 None 终止转发，释放上游连接与账号并发槽；未断连则继续等待
    pub fn next_event_polling(
        &mut self,
        poll: Duration,
        client_gone: &dyn Fn() -> bool,
    ) -> Option<WbEvent> {
        if self.done {
            return None;
        }
        loop {
            let line = match self.lines.next_timeout(poll) {
                Ok(Some(line)) => line,
                Ok(None) => return None, // 流结束（读错误折叠为 EOF）
                Err(()) => {
                    if client_gone() {
                        return None; // 客户端已断连：终止，不再读上游
                    }
                    continue; // 仅停滞未断连：继续等待
                }
            };
            if let Some(ev) = self.feed_line(&line) {
                return Some(ev);
            }
        }
    }
}

/// 解析单条 data 载荷
fn parse_data(data: &str) -> Option<WbEvent> {
    let t = data.trim();
    if t.is_empty() {
        return None;
    }
    if t == "[DONE]" {
        return Some(WbEvent::Done);
    }
    let v: Value = match serde_json::from_str(t) {
        Ok(v) => v,
        Err(_) => return None,
    };
    // 错误帧：{"error": {...}}（与 OpenAI 一致；兼容顶层 code/message）
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .or_else(|| err.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("upstream error")
            .to_string();
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        return Some(WbEvent::Error { code, msg });
    }
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first());
    let delta = choice
        .and_then(|c| c.get("delta"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let usage = v.get("usage").filter(|u| u.is_object()).cloned();
    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
    Some(WbEvent::Chunk { delta, finish, usage, id })
}

type Sender = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

// ==================== 上游 usage 缓存 tokens 宽口径提取（回填上游 3.7.10）====================
//
// 背景：上游 usage 里缓存读/写 tokens 的键名形态多样（OpenAI 标准、Anthropic 口径、
// 上游私有别名、嵌套容器），此前各出口只认窄口径甚至硬编码 0，导致**下游缓存读恒为 0**
// （CPA 等下游据此判断缓存失效，多轮工具循环无法正确累计）。
//
// 修法：提取收敛为本模块的**单一宽口径实现**，其余出口一律复用（不再各写一份窄口径）。
// 键集对齐用量统计既有口径；**仅认正值**——0 值陈旧别名不得遮蔽同结构中有效的其他别名
// （上游同一响应常同时携带 `cached_tokens: 0` 与 `prompt_cache_hit_tokens: N`）。

/// 从对象中按候选键取**首个正值**（0/负数/非数字一律跳过，让后续别名有机会命中）。
fn positive_of(u: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        let n = u.get(*k).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        })?;
        (n > 0).then_some(n)
    })
}

/// 缓存读 tokens（cache read / cache hit）：顶层别名 + 嵌套
/// `prompt_tokens_details.cached_tokens` + `inputTokensDetails[].cached_tokens`。
pub(crate) fn cached_tokens_of(u: &Value) -> u64 {
    positive_of(
        u,
        &[
            "cache_read_input_tokens",
            "cache_read_token",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
            "cache_read_tokens",
        ],
    )
    .or_else(|| {
        u.get("prompt_tokens_details")
            .and_then(|d| positive_of(d, &["cached_tokens"]))
    })
    .or_else(|| {
        u.get("inputTokensDetails")
            .and_then(|d| d.as_array())
            .and_then(|ds| ds.iter().find_map(|d| positive_of(d, &["cached_tokens"])))
    })
    .unwrap_or(0)
}

/// 上游缓存写 tokens（cache_creation 口径）：宽口径提取，仅认正值。
pub(crate) fn cache_write_tokens_of(u: &Value) -> u64 {
    positive_of(
        u,
        &[
            "cache_creation_input_tokens",
            "cache_write_tokens",
            "cacheWriteTokens",
            "cache_creation_tokens",
        ],
    )
    .or_else(|| {
        u.get("prompt_tokens_details")
            .and_then(|d| positive_of(d, &["cache_write_tokens"]))
    })
    .unwrap_or(0)
}

/// 遮蔽键清除清单：宽口径提取器认的所有别名 + 2 个嵌套容器。
///
/// 多轮累计写回前须清除这些键——否则上一轮的 `cached_tokens` 会与本次
/// 宽口径提取值并存，下游按优先级取到陈旧值（提取器键集变化时本清单须同步）。
pub(crate) const CACHE_SHADOW_KEYS: [&str; 8] = [
    "cache_read_input_tokens",
    "cache_read_token",
    "cacheReadInputTokens",
    "prompt_cache_hit_tokens",
    "cached_tokens",
    "cache_read_tokens",
    "prompt_tokens_details",
    "inputTokensDetails",
];

/// 从 usage 对象中清除全部缓存读/写遮蔽键（就地修改）。
pub(crate) fn clear_cache_shadow_keys(u: &mut Value) {
    let Some(m) = u.as_object_mut() else { return };
    for k in CACHE_SHADOW_KEYS {
        m.remove(k);
    }
    for k in [
        "cache_creation_input_tokens",
        "cache_write_tokens",
        "cacheWriteTokens",
        "cache_creation_tokens",
    ] {
        m.remove(k);
    }
}

/// 上游 usage → **OpenAI 标准形态**归一化投影（回填上游 3.7.10）。
///
/// 为什么必须投影而非原样透传：上游 usage 用的是自有键（Anthropic 口径的
/// `cache_read_input_tokens`、私有别名等），严格 OpenAI 客户端只认
/// `prompt_tokens_details.cached_tokens`——原样转发等于下游看不到缓存命中
/// （实测下游缓存读恒为 0）。投影保留提示/补全总数，缓存读统一落到标准嵌套位。
pub(crate) fn normalize_openai_usage(u: &Value) -> Value {
    let (it, ot) = u64_pair(u);
    let mut out = json!({
        "prompt_tokens": it,
        "completion_tokens": ot,
        "total_tokens": it + ot,
        "prompt_tokens_details": { "cached_tokens": cached_tokens_of(u) },
    });
    let cw = cache_write_tokens_of(u);
    if cw > 0 {
        out["cache_creation_input_tokens"] = json!(cw);
    }
    out
}


// ==================== Responses SSE 投影（T4.1/F-40） ====================

/// Responses SSE 事件帧：event + data（type 与 event 同名，Codex 按 data.type 解析）
fn resp_send(tx: &Sender, event: &str, data: Value) {
    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        event, data
    ))));
}

/// 构造 Responses response 对象（created/in_progress/completed/failed 共用骨架）
fn responses_object(id: &str, model: &str, status: &str, output: Vec<Value>, usage: Option<&Value>) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": status,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "incomplete_details": null,
        "error": null,
    });
    if let Some(u) = usage {
        let (it, ot) = u64_pair(u);
        // 缓存读/写 tokens 透传（回填上游 3.7.10）：此前硬编码 0，下游缓存读恒为 0。
        // reasoning_tokens 上游若无对应字段仍为 0（该字段本模块暂不提取）
        obj["usage"] = json!({
            "input_tokens": it,
            "input_tokens_details": {"cached_tokens": cached_tokens_of(u)},
            "output_tokens": ot,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": it + ot,
        });
        let cw = cache_write_tokens_of(u);
        if cw > 0 {
            obj["usage"]["cache_creation_input_tokens"] = json!(cw);
        }
    }
    obj
}

/// 流式转发：WB SSE → 客户端协议帧（扩展返回）
///
/// 返回 (流内错误, 是否已发送过数据, 流内失败已就地下发, usage)：
/// - error_info：尚未向下游发送任何数据时捕获的流内错误（调用方据此换号重试）；
/// - failed_inline：流内失败事件（response.failed / error 帧）已就地透传客户端——
///   调用方不得再按成功收尾（不记成功、不清冷却、不绑定粘性），也不应重试
///   （错误已原样给到客户端，重试会造成重复流）。
/// - 行源须为可中断行源：上游停滞期间每 LINE_POLL 检查一次客户端断连
///   （tx.is_closed()），断连即终止转发（上游连接与账号并发槽随 Drop 释放）
// 宏内末次赋值（text_block_open）在收尾路径后不再读取，属预期行为（对齐 sse.rs）
#[allow(unused_assignments)]
pub fn stream_forward_ex(
    lines: InterruptibleLines,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, bool, Option<Value>) {
    let mut parser = WbSseParser::new(lines);
    let mut sent_any = false;
    let mut failed_inline = false;
    // 真实内容标记（正文/思考链/工具调用任一非空）：空完成（影子风控）检测依据
    // （issue #57）；占位帧（Responses created / 空 delta chunk）不计入
    let mut has_content = false;
    let mut usage: Option<Value> = None;
    let mut error_info: Option<(i64, String)> = None;

    macro_rules! send {
        ($s:expr) => {
            let _ = tx.blocking_send(Ok(bytes::Bytes::from($s)));
        };
    }

    // Anthropic 输出状态
    let mut message_started = false;
    let mut text_block_open = false;
    // T5.3/F-62：思考链 thinking 块（在文本块之前输出）
    let mut thinking_block_open = false;
    let mut block_index: i64 = -1;

    let anthropic_start = |tx: &Sender, message_started: &mut bool, chat_id: &str, model: &str| {
        if !*message_started {
            *message_started = true;
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: message_start\ndata: {}\n\n",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": chat_id, "type": "message", "role": "assistant",
                        "model": model, "content": [],
                        "stop_reason": null, "stop_sequence": null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                })
            ))));
        }
    };

    // Anthropic tool_use 块缓冲（index → (id,name,args)）
    let mut tool_buf: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
    // 上游最后携带的 finish_reason（P1 修复：Anthropic message_delta 按此映射 stop_reason）
    let mut last_finish = String::new();
    // OpenAI 流式出口：role 只允许出现在首个下发帧（issue #96 噪声帧治理）
    let mut openai_role_sent = false;
    // 正常收尾标记（issue #96 同提交）：Done 分支置位，EOF 兜底据此避免重复收尾
    let mut finished = false;

    // Responses 输出状态（T4.1/F-40）
    let mut resp_created = false;
    let mut resp_msg_open = false;
    let mut resp_text = String::new();
    let mut resp_output_index: i64 = -1;

    macro_rules! resp_ensure_created {
        () => {
            if !resp_created {
                resp_created = true;
                resp_send(
                    tx,
                    "response.created",
                    json!({
                        "type": "response.created",
                        "response": responses_object(chat_id, model, "in_progress", vec![], None),
                        "sequence_number": 0,
                    }),
                );
            }
        };
    }

    // 正常收尾序列（审查 P1-5）：上游 Done 与「EOF 断流但有内容」共用，
    // 按协议补齐终止帧，避免严格客户端因未闭合的 content_block / message 项挂起
    macro_rules! finish_stream {
        () => {
            match proto {
                crate::api_server::routes::Protocol::Anthropic => {
                    anthropic_start(tx, &mut message_started, chat_id, model);
                    if thinking_block_open {
                        thinking_block_open = false;
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            json!({"type":"content_block_stop","index":block_index})
                        ))));
                    }
                    if text_block_open {
                        text_block_open = false;
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            json!({"type":"content_block_stop","index":block_index})
                        ))));
                    }
                    // 工具块统一在文本块之后输出
                    for (i, (_k, (tid, name, args))) in tool_buf.iter().enumerate() {
                        let idx = block_index + 1 + i as i64;
                        let id = if tid.is_empty() { format!("toolu_{}_{}", chat_id, i) } else { tid.clone() };
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_start\ndata: {}\n\n",
                            json!({"type":"content_block_start","index":idx,
                                   "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}})
                        ))));
                        if !args.is_empty() {
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_delta\ndata: {}\n\n",
                                json!({"type":"content_block_delta","index":idx,
                                       "delta":{"type":"input_json_delta","partial_json":args}})
                            ))));
                        }
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_stop\ndata: {}\n\n",
                            json!({"type":"content_block_stop","index":idx})
                        ))));
                    }
                    let (it, ot) = usage.as_ref().map(u64_pair).unwrap_or((0, 0));
                    // 缓存读/写 tokens 透传（回填上游 3.7.10）：Anthropic 出口此前缺失
                    let mut a_usage = json!({"input_tokens": it, "output_tokens": ot});
                    if let Some(u) = usage.as_ref() {
                        let cr = cached_tokens_of(u);
                        let cw = cache_write_tokens_of(u);
                        if cr > 0 {
                            a_usage["cache_read_input_tokens"] = json!(cr);
                        }
                        if cw > 0 {
                            a_usage["cache_creation_input_tokens"] = json!(cw);
                        }
                    }
                    // P1 修复：stop_reason 按上游 finish_reason 映射，工具块存在时
                    // 必为 tool_use（原硬编码 end_turn 与已输出的 tool_use 块矛盾）
                    let stop_reason = if !tool_buf.is_empty() {
                        "tool_use"
                    } else {
                        finish_to_stop(&last_finish)
                    };
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "event: message_delta\ndata: {}\n\n",
                        json!({
                            "type":"message_delta",
                            "delta":{"stop_reason":stop_reason,"stop_sequence":null},
                            "usage":a_usage,
                        })
                    ))));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "event: message_stop\ndata: {}\n\n",
                        json!({"type":"message_stop"})
                    ))));
                }
                crate::api_server::routes::Protocol::Responses => {
                    // 未产出任何 chunk 也补 created，保证事件序列完整
                    resp_ensure_created!();
                    let mut output: Vec<Value> = Vec::new();
                    if resp_msg_open {
                        output.push(json!({
                            "id": format!("{}_msg_0", chat_id),
                            "type": "message",
                            "status": "completed",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": resp_text, "annotations": []}],
                        }));
                        resp_send(tx, "response.output_item.done", json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": output[0],
                        }));
                    }
                    // 工具调用缓冲统一在文本项之后完整输出
                    for (i, (_k, (tid, name, args))) in tool_buf.iter().enumerate() {
                        let idx = output.len() as i64;
                        let item_id = if tid.is_empty() {
                            format!("{}_fc_{}", chat_id, i)
                        } else {
                            tid.clone()
                        };
                        let mut item = json!({
                            "id": item_id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": if tid.is_empty() { format!("call_{}_{}", chat_id, i) } else { tid.clone() },
                            "arguments": args,
                        });
                        // 编码名还原为 {name, namespace} 双字段（issue #93）：流式与非流式同口径
                        match super::wb_responses::split_namespaced(&name) {
                            Some((ns, inner)) => {
                                item["name"] = json!(inner);
                                item["namespace"] = json!(ns);
                            }
                            None => item["name"] = json!(name),
                        }
                        let mut added = item.clone();
                        added["status"] = json!("in_progress");
                        added["arguments"] = json!("");
                        resp_send(tx, "response.output_item.added", json!({
                            "type": "response.output_item.added",
                            "output_index": idx,
                            "item": added,
                        }));
                        if !args.is_empty() {
                            resp_send(tx, "response.function_call_arguments.delta", json!({
                                "type": "response.function_call_arguments.delta",
                                "item_id": item["id"],
                                "output_index": idx,
                                "delta": args,
                            }));
                        }
                        resp_send(tx, "response.output_item.done", json!({
                            "type": "response.output_item.done",
                            "output_index": idx,
                            "item": item,
                        }));
                        output.push(item);
                    }
                    resp_send(tx, "response.completed", json!({
                        "type": "response.completed",
                        "response": responses_object(chat_id, model, "completed", output, usage.as_ref()),
                    }));
                }
                _ => {
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                }
            }
        };
    }

    loop {
        // 客户端断连快速检测（对齐 sse.rs「发送失败即断」）：send! 宏忽略发送
        // 失败，活跃流期间的断连依赖此处逐事件检查，最坏延迟一个事件的处理耗时；
        // 停滞期间的断连由 next_event_polling 的 Err 分支覆盖
        if tx.is_closed() {
            break;
        }
        match parser.next_event_polling(LINE_POLL, &|| tx.is_closed()) {
            None => break,
            Some(WbEvent::Done) => {
                if !has_content {
                    // 空完成（影子风控/上游异常）：不发任何收尾帧，以哨兵错误上抛
                    // 调用方换号重试（issue #57，不再把空响应伪装成正常完成）。
                    // 注：Responses 协议若已发 created（空 delta chunk 触发），换号
                    // 重试会重复 created——影子风控场景上游通常零 chunk，属可接受边界
                    error_info = Some((
                        super::EMPTY_COMPLETION_CODE,
                        super::EMPTY_COMPLETION_MSG.to_string(),
                    ));
                    break;
                }
                // Done 与 EOF 断流共用收尾序列（P1-5 提取为 finish_stream!）
                finished = true;
                finish_stream!();
                sent_any = true;
                break;
            }
            Some(WbEvent::Error { code, msg }) => {
                if sent_any {
                    // 已有数据流出：就地透传错误并收尾（failed_inline 标记给调用方）；
                    // 空 message 兜底（issue #71）：inline 与延迟透传同防线
                    let msg = stream_msg_or_fallback(&msg, code);
                    match proto {
                        crate::api_server::routes::Protocol::Anthropic => {
                            let err = json!({"type":"error","error":{"type":"api_error","message":msg}});
                            send!(format!("event: error\ndata: {}\n\n", err));
                        }
                        crate::api_server::routes::Protocol::Responses => {
                            // 流内错误 → response.failed（Responses 无 [DONE] 帧）
                            resp_ensure_created!();
                            let mut resp = responses_object(chat_id, model, "failed", vec![], None);
                            resp["error"] = json!({"code": code.to_string(), "message": msg});
                            resp_send(tx, "response.failed", json!({
                                "type": "response.failed",
                                "response": resp,
                            }));
                        }
                        _ => {
                            let body = json!({"error":{"message":msg,"type":"api_error","code":code}});
                            send!(format!("data: {}\n\n", body));
                            send!("data: [DONE]\n\n");
                        }
                    }
                    sent_any = true;
                    failed_inline = true;
                } else {
                    error_info = Some((code, msg));
                }
                break;
            }
            Some(WbEvent::Chunk { delta, finish, usage: u, id }) => {
                if !finish.is_empty() {
                    last_finish = finish.clone();
                }
                // 本帧是否携带 usage（issue #96：纯噪声帧跳过判定依据，
                // 须在 u 合并进函数级 usage 前捕获）
                let frame_has_usage = u.is_some();
                if let Some(x) = u {
                    usage = Some(x);
                }
                // 真实内容标记（issue #57 空完成检测）：空 delta chunk 不计入
                if delta.get("content").and_then(|c| c.as_str()).map_or(false, |s| !s.is_empty())
                    || delta
                        .get("reasoning_content")
                        .and_then(|c| c.as_str())
                        .map_or(false, |s| !s.is_empty())
                    || delta
                        .get("tool_calls")
                        .and_then(|t| t.as_array())
                        .map_or(false, |a| !a.is_empty())
                {
                    has_content = true;
                }
                if proto == crate::api_server::routes::Protocol::Anthropic {
                    anthropic_start(tx, &mut message_started, chat_id, model);
                    // T5.3/F-62：思考链增量 → thinking block（先于文本块）
                    if let Some(t) = delta.get("reasoning_content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                        if !thinking_block_open {
                            // 文本块已开则先关闭（上游先文本后思考的异常次序兜底）
                            if text_block_open {
                                text_block_open = false;
                                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                    "event: content_block_stop\ndata: {}\n\n",
                                    json!({"type":"content_block_stop","index":block_index})
                                ))));
                            }
                            block_index += 1;
                            thinking_block_open = true;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_start\ndata: {}\n\n",
                                json!({"type":"content_block_start","index":block_index,
                                       "content_block":{"type":"thinking","thinking":""}})
                            ))));
                        }
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({"type":"content_block_delta","index":block_index,
                                   "delta":{"type":"thinking_delta","thinking":t}})
                        ))));
                        sent_any = true;
                    }
                    // 文本增量
                    if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                        // thinking 块先收口，再开文本块
                        if thinking_block_open {
                            thinking_block_open = false;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_stop\ndata: {}\n\n",
                                json!({"type":"content_block_stop","index":block_index})
                            ))));
                        }
                        if !text_block_open {
                            block_index += 1;
                            text_block_open = true;
                            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                                "event: content_block_start\ndata: {}\n\n",
                                json!({"type":"content_block_start","index":block_index,
                                       "content_block":{"type":"text","text":""}})
                            ))));
                        }
                        let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({"type":"content_block_delta","index":block_index,
                                   "delta":{"type":"text_delta","text":t}})
                        ))));
                        sent_any = true;
                    }
                    // 工具调用增量：按 index 缓冲，finish 时统一输出
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(tool_buf.len() as i64);
                            let e = tool_buf.entry(idx).or_default();
                            if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                                if !i.is_empty() {
                                    e.0 = i.to_string();
                                }
                            }
                            if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                                if !n.is_empty() {
                                    e.1 = n.to_string();
                                }
                            }
                            if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                                match a {
                                    Value::String(s) => e.2.push_str(s),
                                    v if !v.is_null() => {
                                        if let Ok(s) = serde_json::to_string(v) {
                                            e.2.push_str(&s);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        sent_any = true;
                    }
                } else {
                    match proto {
                        crate::api_server::routes::Protocol::OpenAi => {
                            // OpenAI chunk 归一化透传（rewrite id/model）。
                            // issue #96：Buddy 上游每个 delta 携带与内容无关的噪声
                            // 字段——空 tool_calls 数组、逐帧重复 role、空串
                            // content/reasoning_content/refusal、恒 null 的
                            // extra_fields/function_call。AI SDK v5 系客户端把
                            // "delta 存在 tool_calls 字段（哪怕空数组）"当作本轮
                            // reasoning 结束信号，空数组被放大为"每个思考碎片单独
                            // 成段"（深度思考模型一次响应被撕成上百张思考卡）。
                            // 透传前按 OpenAI 流式惯例做 delta 归一化。
                            let mut delta = delta;
                            if let Some(obj) = delta.as_object_mut() {
                                // 关键修复：空 tool_calls 数组整体移除
                                if obj
                                    .get("tool_calls")
                                    .and_then(|v| v.as_array())
                                    .is_some_and(|a| a.is_empty())
                                {
                                    obj.remove("tool_calls");
                                }
                                // role 只保留在首个下发帧，其余帧移除
                                if openai_role_sent {
                                    obj.remove("role");
                                } else if obj.contains_key("role") {
                                    openai_role_sent = true;
                                }
                                // 空字符串字段剥离
                                for key in ["content", "reasoning_content", "refusal"] {
                                    if obj.get(key).and_then(|v| v.as_str()) == Some("") {
                                        obj.remove(key);
                                    }
                                }
                                // 恒空噪声字段剥离
                                for key in ["extra_fields", "function_call"] {
                                    if obj.get(key).is_some_and(|v| v.is_null()) {
                                        obj.remove(key);
                                    }
                                }
                            }
                            // 剥离后为空、且本帧无 finish_reason 与 usage 的纯噪声帧
                            // 整帧跳过：has_content 基于非空语义判定不受影响，
                            // 空完成哨兵检测（issue #57）语义不变
                            let frame_empty = delta.as_object().is_none_or(|o| o.is_empty());
                            if !(frame_empty && finish.is_empty() && !frame_has_usage) {
                                let mut choice = json!({"index":0,"delta":delta});
                                if !finish.is_empty() {
                                    choice["finish_reason"] = json!(finish);
                                }
                                let mut chunk = json!({
                                    "id": if id.is_empty() { chat_id.to_string() } else { id },
                                    "object": "chat.completion.chunk",
                                    "created": now_ts(),
                                    "model": model,
                                    "choices": [choice],
                                });
                                if let Some(x) = &usage {
                                    // 归一化投影（回填上游 3.7.10）：上游自有键（Anthropic 口径等）
                                    // 原样下发会让下游认不到缓存命中
                                    chunk["usage"] = normalize_openai_usage(x);
                                }
                                send!(format!("data: {}\n\n", chunk));
                                sent_any = true;
                            }
                        }
                        crate::api_server::routes::Protocol::OpenAiText => {
                            // delta.content → text 块；tool_calls 无对应字段跳过
                            if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                                let mut choice = json!({"text":t,"index":0});
                                if !finish.is_empty() {
                                    choice["finish_reason"] = json!(finish);
                                }
                                let mut chunk = json!({
                                    "id": chat_id,
                                    "object": "text_completion",
                                    "created": now_ts(),
                                    "model": model,
                                    "choices": [choice],
                                });
                                if let Some(x) = &usage {
                                    // 同上：归一化投影（text_completion 出口同口径）
                                    chunk["usage"] = normalize_openai_usage(x);
                                }
                                send!(format!("data: {}\n\n", chunk));
                                sent_any = true;
                            }
                        }
                        crate::api_server::routes::Protocol::Anthropic => unreachable!(),
                        crate::api_server::routes::Protocol::Responses => {
                            resp_ensure_created!();
                            // created 帧已下发：后续流内错误必须就地 response.failed，
                            // 不得再走「未发送数据」换号重试路径（否则客户端收到重复流）
                            sent_any = true;
                            // 文本增量 → message 输出项 + output_text.delta
                            if let Some(t) = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
                                if !resp_msg_open {
                                    resp_msg_open = true;
                                    resp_output_index += 1;
                                    resp_send(tx, "response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "output_index": resp_output_index,
                                        "item": {
                                            "id": format!("{}_msg_0", chat_id),
                                            "type": "message",
                                            "status": "in_progress",
                                            "role": "assistant",
                                            "content": [],
                                        },
                                    }));
                                }
                                resp_text.push_str(t);
                                resp_send(tx, "response.output_text.delta", json!({
                                    "type": "response.output_text.delta",
                                    "item_id": format!("{}_msg_0", chat_id),
                                    "output_index": resp_output_index,
                                    "content_index": 0,
                                    "delta": t,
                                }));
                            }
                            // 工具调用增量：按 index 缓冲，completed 时统一输出完整项
                            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                                for tc in tcs {
                                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(tool_buf.len() as i64);
                                    let e = tool_buf.entry(idx).or_default();
                                    if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                                        if !i.is_empty() {
                                            e.0 = i.to_string();
                                        }
                                    }
                                    if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                                        if !n.is_empty() {
                                            e.1 = n.to_string();
                                        }
                                    }
                                    if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                                        match a {
                                            Value::String(s) => e.2.push_str(s),
                                            v if !v.is_null() => {
                                                if let Ok(s) = serde_json::to_string(v) {
                                                    e.2.push_str(&s);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 上游断流（EOF 未收到 Done/Error 事件）优雅收尾（审查 P1-5，对齐
    // sse.rs SOLO finish_stream 语义）：已有内容 → 按协议补齐终止帧，避免
    // 严格客户端因未闭合的 content_block / message 项挂起；零内容不伪装
    // 成功，走下方空完成哨兵换号重试。
    // `!finished` 门控（回填上游 3.7.11）：Done 分支已收尾过，此处再补一次会
    // 让正常结束的成功流**重复下发** [DONE] / message_stop
    if !finished && error_info.is_none() && !failed_inline && has_content && !tx.is_closed() {
        finish_stream!();
        sent_any = true;
    }

    // 空完成兜底（issue #57）：EOF 且零内容零错误 → 哨兵上抛
    // （原样收尾会让客户端收到 "empty provider response"）。
    // 客户端在线门控已移除（回填上游 3.7.11）：断连时同样须上抛哨兵——否则
    // 调用方按"成功"处理并绑定粘性会话，把一个什么都没产出的账号钉在会话上
    if error_info.is_none() && !has_content && !failed_inline {
        error_info = Some((
            super::EMPTY_COMPLETION_CODE,
            super::EMPTY_COMPLETION_MSG.to_string(),
        ));
    }

    (error_info, sent_any, failed_inline, usage)
}

/// 兼容封装：旧三元组返回（既有调用方不感知流内失败标记；
/// 新调用方请用 stream_forward_ex）
pub fn stream_forward(
    lines: InterruptibleLines,
    tx: &Sender,
    proto: crate::api_server::routes::Protocol,
    chat_id: &str,
    model: &str,
) -> (Option<(i64, String)>, bool, Option<Value>) {
    let (error_info, sent_any, _failed_inline, usage) =
        stream_forward_ex(lines, tx, proto, chat_id, model);
    (error_info, sent_any, usage)
}

/// 非流式聚合：WB SSE → 内部 OpenAI completion（tool_calls 按 index 合并）
/// 返回 (completion|None, error|None)
pub fn aggregate<L: Iterator<Item = String>>(
    lines: L,
    chat_id: &str,
) -> (Option<Value>, Option<(i64, String)>) {
    let mut parser = WbSseParser::new(lines);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = String::new();
    let mut usage: Option<Value> = None;
    // index → (id, name, args)
    let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

    loop {
        match parser.next_event() {
            None => break,
            Some(WbEvent::Done) => break,
            Some(WbEvent::Error { code, msg }) => return (None, Some((code, msg))),
            Some(WbEvent::Chunk { delta, finish: f, usage: u, .. }) => {
                if let Some(x) = u {
                    usage = Some(x);
                }
                if !f.is_empty() {
                    finish = f;
                }
                if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
                    content.push_str(t);
                }
                if let Some(t) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                    reasoning.push_str(t);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc
                            .get("index")
                            .and_then(|i| i.as_i64())
                            .unwrap_or(tools.len() as i64);
                        let e = tools.entry(idx).or_default();
                        if let Some(i) = tc.get("id").and_then(|i| i.as_str()) {
                            if !i.is_empty() {
                                e.0 = i.to_string();
                            }
                        }
                        if let Some(n) = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                            if !n.is_empty() {
                                e.1 = n.to_string();
                            }
                        }
                        if let Some(a) = tc.get("function").and_then(|f| f.get("arguments")) {
                            match a {
                                Value::String(s) => e.2.push_str(s),
                                v if !v.is_null() => {
                                    if let Ok(s) = serde_json::to_string(v) {
                                        e.2.push_str(&s);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tools.is_empty() {
        let calls: Vec<Value> = tools
            .values()
            .enumerate()
            .map(|(i, (id, name, args))| {
                json!({
                    "id": if id.is_empty() { format!("call_{}_{}", chat_id, i) } else { id.clone() },
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect();
        message["tool_calls"] = json!(calls);
    }
    let mut resp = json!({
        "id": chat_id,
        "object": "chat.completion",
        "created": now_ts(),
        "model": "",
        "choices": [{ "index": 0, "message": message, "finish_reason": if finish.is_empty() { "stop" } else { &finish } }],
    });
    if let Some(u) = usage {
        resp["usage"] = u;
    }
    (Some(resp), None)
}

/// OpenAI finish_reason → Anthropic stop_reason 映射（P1 修复）
fn finish_to_stop(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

/// 内部 OpenAI completion → Anthropic Messages 响应（非流式 /v1/messages）
pub fn completion_to_anthropic(v: &Value, msg_id: &str, model: &str) -> Value {
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();
    // T5.3/F-62：reasoning_content → thinking block（置于 text 之前）
    if let Some(t) = message
        .get("reasoning_content")
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    {
        content.push(json!({"type":"thinking","thinking":t}));
    }
    if let Some(t) = message.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type":"text","text":t}));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let args: Value = tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": tc.get("id").cloned().unwrap_or(json!("toolu_0")),
                "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                "input": args,
            }));
        }
    }
    if content.is_empty() {
        content.push(json!({"type":"text","text":""}));
    }
    let (it, ot) = v.get("usage").map(u64_pair).unwrap_or((0, 0));
    // 缓存读/写 tokens 透传（回填上游 3.7.10）：非流式 Anthropic 出口此前缺失
    let mut a_usage = json!({"input_tokens": it, "output_tokens": ot});
    if let Some(u) = v.get("usage") {
        let cr = cached_tokens_of(u);
        let cw = cache_write_tokens_of(u);
        if cr > 0 {
            a_usage["cache_read_input_tokens"] = json!(cr);
        }
        if cw > 0 {
            a_usage["cache_creation_input_tokens"] = json!(cw);
        }
    }
    // P1 修复：stop_reason 读上游 finish_reason 映射（原硬编码 end_turn 丢失
    // tool_use/max_tokens 语义）；有 tool_calls 时强制 tool_use 兜底
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty());
    let stop_reason = if has_tool_calls {
        "tool_use"
    } else {
        finish_to_stop(choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or(""))
    };
    json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": a_usage,
    })
}

/// 内部 OpenAI completion → OpenAI legacy text_completion（非流式 /v1/completions）
pub fn completion_to_text(v: &Value, model: &str) -> Value {
    let text = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let finish = v
        .pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
        .to_string();
    let mut resp = json!({
        "id": v.get("id").cloned().unwrap_or(json!("cmpl-0")),
        "object": "text_completion",
        "created": now_ts(),
        "model": model,
        "choices": [{ "text": text, "index": 0, "logprobs": null, "finish_reason": finish }],
    });
    if let Some(u) = v.get("usage") {
        resp["usage"] = u.clone();
    }
    resp
}

fn u64_pair(u: &Value) -> (u64, u64) {
    // 宽口径解析（回填上游 e932c19）：浮点/字符串形态上游同样会下发，
    // 旧实现只认 as_u64，后两者静默归零
    let get = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| {
                let v = u.get(*k)?;
                v.as_u64()
                    .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
                    .or_else(|| v.as_f64().map(|f| f.max(0.0).round() as u64))
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f.max(0.0).round() as u64))
            })
            .unwrap_or(0)
    };
    (
        get(&["prompt_tokens", "input_tokens"]),
        get(&["completion_tokens", "output_tokens"]),
    )
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试行源：包装为可中断行源（stream_forward_ex 现要求 InterruptibleLines；
    /// 其同时实现 Iterator，aggregate 等泛型调用点无需区分）
    fn lines(v: &[&str]) -> InterruptibleLines {
        InterruptibleLines::from_iterator(Box::new(
            v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter(),
        ))
    }

    /// 停滞 + 断连：上游永不产出、客户端已断连 → next_event_polling 应在一个
    /// 轮询窗口内返回 None（而非死等上游下一行 / 300s 读超时）
    #[test]
    fn next_event_polling_aborts_on_disconnect_during_stall() {
        let (_tx_keep, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        // sender 存活但无数据：桥接线程阻塞在行迭代器上，模拟上游停滞
        let stall = Box::new(rx.into_iter().filter_map(|r| r.ok()));
        let mut parser = WbSseParser::new(InterruptibleLines::from_iterator(stall));
        let client_gone = || true;
        let start = std::time::Instant::now();
        assert!(parser.next_event_polling(LINE_POLL, &client_gone).is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "断连后应在一个轮询窗口内返回，实测 {:?}",
            start.elapsed()
        );
    }

    /// 活跃流 + 断连：上游持续产出且无 [DONE]（不断流），客户端通道已关闭 →
    /// 逐事件快速检测应立即终止（sent_any=false），而非读完整条流才结束
    #[test]
    fn stream_forward_aborts_immediately_when_client_gone() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        drop(rx); // 客户端已断连
        let data: Vec<String> = (0..1000)
            .flat_map(|i| {
                vec![
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{}\"}}}}]}}", i),
                    String::new(),
                ]
            })
            .collect();
        let src = InterruptibleLines::from_iterator(Box::new(data.into_iter()));
        let start = std::time::Instant::now();
        let (_err, sent_any, _fi, _u) = stream_forward_ex(
            src, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m",
        );
        assert!(!sent_any, "断连后不得有任何事件下发");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "活跃流断连应立即终止，实测 {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn parser_handles_multi_line_data_and_done() {
        let mut p = WbSseParser::new(lines(&[
            ": keep-alive",
            "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"he\"}}]}",
            "",
            "data: {\"id\":\"x\",\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            "",
            "data: [DONE]",
            "",
        ]));
        assert!(matches!(p.next_event(), Some(WbEvent::Chunk { .. })));
        match p.next_event() {
            Some(WbEvent::Chunk { delta, finish, usage, .. }) => {
                assert_eq!(delta["content"], json!("llo"));
                assert_eq!(finish, "stop");
                assert!(usage.is_some());
            }
            _ => panic!("expect chunk"),
        }
        assert!(matches!(p.next_event(), Some(WbEvent::Done)));
        assert!(p.next_event().is_none());
    }

    #[test]
    fn parser_detects_error_frame() {
        let mut p = WbSseParser::new(lines(&[
            "data: {\"error\":{\"message\":\"敏感内容\",\"code\":1001}}",
            "",
        ]));
        match p.next_event() {
            Some(WbEvent::Error { code, msg }) => {
                assert_eq!(code, 1001);
                assert_eq!(msg, "敏感内容");
            }
            _ => panic!("expect error"),
        }
    }

    #[test]
    fn aggregate_merges_tool_call_deltas_by_index() {
        let out = aggregate(
            lines(&[
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"get\",\"arguments\":\"{\\\"a\\\"\"}}]}}]}",
                "",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":1}\"}}]}}]}",
                "",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}",
                "",
                "data: [DONE]",
                "",
            ]),
            "chat-1",
        );
        let (resp, err) = out;
        assert!(err.is_none());
        let r = resp.unwrap();
        let msg = &r["choices"][0]["message"];
        assert_eq!(msg["content"], json!("done"));
        assert_eq!(msg["tool_calls"][0]["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(msg["tool_calls"][0]["function"]["name"], json!("get"));
        assert_eq!(r["usage"]["prompt_tokens"], json!(10));
        assert_eq!(r["choices"][0]["finish_reason"], json!("tool_calls"));
    }

    #[test]
    fn aggregate_reports_error_before_any_content() {
        let (resp, err) = aggregate(
            lines(&["data: {\"error\":{\"message\":\"boom\",\"code\":9}}", ""]),
            "c",
        );
        assert!(resp.is_none());
        assert_eq!(err.unwrap(), (9, "boom".to_string()));
    }

    #[test]
    fn converters_roundtrip() {
        let (resp, _) = aggregate(lines(&["data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}", "data: [DONE]", ""]), "c1");
        let v = resp.unwrap();
        let anth = completion_to_anthropic(&v, "msg_1", "glm-5.3");
        assert_eq!(anth["type"], json!("message"));
        assert_eq!(anth["content"][0]["text"], json!("你好"));
        let txt = completion_to_text(&v, "glm-5.3");
        assert_eq!(txt["choices"][0]["text"], json!("你好"));
        assert_eq!(txt["object"], json!("text_completion"));
    }

    /// Responses 流式投影：created → item.added → text.delta → item.done → completed
    #[test]
    fn stream_forward_emits_responses_event_sequence() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, usage) =
            stream_forward(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_1", "glm-5.3");
        assert!(err.is_none());
        assert!(sent_any);
        assert!(usage.is_some());
        drop(tx);

        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.output_item.added"));
        assert!(body.contains("event: response.output_text.delta"));
        assert!(body.contains("\"delta\":\"he\""));
        assert!(body.contains("event: response.output_item.done"));
        let completed = body.split("event: response.completed").nth(1).unwrap_or("");
        assert!(completed.contains("\"status\":\"completed\""));
        assert!(completed.contains("\"input_tokens\":3"));
        assert!(completed.contains("\"output_tokens\":2"));
        assert!(!body.contains("[DONE]"), "Responses 流不应出现 OpenAI [DONE] 帧");
    }

    /// 回归（F-40 审查修复）：文本已流出后遇错误帧 → created 已发即 sent_any=true，
    /// 错误就地 response.failed，不返回 error_info（否则上层换号重试会造成重复流）；
    /// failed_inline=true 供调用方跳过成功收尾（不记成功/不清冷却/不绑粘性）
    #[test]
    fn stream_forward_responses_instream_error_after_content_fails_inplace() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分输出\"}}]}",
            "",
            "data: {\"error\":{\"message\":\"中途失败\",\"code\":1001}}",
            "",
        ]);
        let (err, sent_any, failed_inline, _usage) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_9", "m");
        assert!(err.is_none(), "流内错误已就地下发，不得上抛触发换号重试");
        assert!(sent_any);
        assert!(failed_inline, "流内失败必须以 failed_inline 上报调用方");
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.failed"));
        assert!(body.contains("中途失败"));
    }

    /// 正常完成流：failed_inline 必须为 false（不得误报流内失败）
    #[test]
    fn stream_forward_success_reports_no_inline_failure() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, failed_inline, usage) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Responses, "resp_2", "m");
        assert!(err.is_none());
        assert!(sent_any);
        assert!(!failed_inline);
        assert!(usage.is_none(), "上游未下发 usage 时不得伪造");
    }

    /// 流内失败标记对 Anthropic 协议同样生效（event: error 就地下发）
    #[test]
    fn stream_forward_anthropic_instream_error_marks_failed_inline() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"部分\"}}]}",
            "",
            "data: {\"error\":{\"message\":\"中途失败\",\"code\":7}}",
            "",
        ]);
        let (err, _sent_any, failed_inline, _usage) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_3", "m");
        assert!(err.is_none());
        assert!(failed_inline);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        assert!(body.contains("event: error"));
    }

    /// 流开始前遇错误：错误上抛（error_info）、无 failed_inline（调用方换号重试）
    #[test]
    fn stream_forward_error_before_stream_uplifts() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&["data: {\"error\":{\"message\":\"boom\",\"code\":9}}", ""]);
        let (err, sent_any, failed_inline, _usage) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::OpenAi, "c", "m");
        assert_eq!(err.unwrap(), (9, "boom".to_string()));
        assert!(!sent_any);
        assert!(!failed_inline);
    }

    /// Anthropic message_delta 的 usage 附带 input_tokens（上游已知时）
    #[test]
    fn anthropic_message_delta_includes_input_tokens_when_known() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}",
            "",
            "data: [DONE]",
            "",
        ]);
        let (err, sent_any, failed_inline, _u) =
            stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_4", "m");
        assert!(err.is_none() && sent_any && !failed_inline);
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        let md = body.split("event: message_delta").nth(1).unwrap_or("");
        assert!(md.contains("\"input_tokens\":7"), "message_delta 需携带 input_tokens: {md}");
        assert!(md.contains("\"output_tokens\":3"));
    }

    /// 上游未下发 usage 时 message_delta 的 input_tokens 保持 0
    #[test]
    fn anthropic_message_delta_input_tokens_zero_when_unknown() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(256);
        let lines = lines(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            "",
            "data: [DONE]",
            "",
        ]);
        let _ = stream_forward_ex(lines, &tx, crate::api_server::routes::Protocol::Anthropic, "msg_5", "m");
        drop(tx);
        let mut body = String::new();
        while let Ok(frame) = rx.try_recv() {
            body.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        let md = body.split("event: message_delta").nth(1).unwrap_or("");
        assert!(md.contains("\"input_tokens\":0"));
    }

    // ============ 缓存 tokens 宽口径提取（回填上游 3.7.10）============

    /// 宽口径提取：顶层别名 + 嵌套形态都要认。
    #[test]
    fn cached_tokens_of_accepts_wide_key_forms() {
        assert_eq!(cached_tokens_of(&json!({"cache_read_input_tokens": 64})), 64);
        assert_eq!(cached_tokens_of(&json!({"prompt_cache_hit_tokens": 32})), 32);
        assert_eq!(cached_tokens_of(&json!({"cached_tokens": 8})), 8);
        // OpenAI 标准嵌套
        assert_eq!(
            cached_tokens_of(&json!({"prompt_tokens_details": {"cached_tokens": 16}})),
            16
        );
        // 上游私有嵌套数组
        assert_eq!(
            cached_tokens_of(&json!({"inputTokensDetails": [{"cached_tokens": 4}]})),
            4
        );
        // 无缓存字段
        assert_eq!(cached_tokens_of(&json!({"prompt_tokens": 100})), 0);
    }

    /// 关键回归：0 值陈旧别名**不得遮蔽**同结构中有效的其他别名。
    /// 上游同一响应常同时携带 `cached_tokens: 0` 与 `prompt_cache_hit_tokens: N`，
    /// 旧窄口径会先命中 0 并据此判定「无缓存命中」。
    #[test]
    fn cached_tokens_of_skips_zero_aliases() {
        let u = json!({"cached_tokens": 0, "prompt_cache_hit_tokens": 128});
        assert_eq!(cached_tokens_of(&u), 128, "0 值别名不得遮蔽有效别名");
    }

    /// 宽口径写侧提取。
    #[test]
    fn cache_write_tokens_of_accepts_wide_key_forms() {
        assert_eq!(cache_write_tokens_of(&json!({"cache_creation_input_tokens": 12})), 12);
        assert_eq!(cache_write_tokens_of(&json!({"cache_write_tokens": 5})), 5);
        assert_eq!(cache_write_tokens_of(&json!({"prompt_tokens": 1})), 0);
    }

    /// OpenAI 归一化投影：上游自有键 → 下游只认的标准嵌套位。
    #[test]
    fn normalize_openai_usage_projects_to_standard_shape() {
        let u = json!({
            "prompt_tokens": 100, "completion_tokens": 20,
            "cache_read_input_tokens": 64, "cache_creation_input_tokens": 8,
        });
        let out = normalize_openai_usage(&u);
        assert_eq!(out["prompt_tokens"], json!(100));
        assert_eq!(out["completion_tokens"], json!(20));
        assert_eq!(out["total_tokens"], json!(120));
        assert_eq!(out["prompt_tokens_details"]["cached_tokens"], json!(64));
        assert_eq!(out["cache_creation_input_tokens"], json!(8));
    }

    /// 遮蔽键清除：清单必须覆盖提取器认的全部别名，否则多轮累计会残留陈旧值。
    #[test]
    fn clear_cache_shadow_keys_removes_all_extractor_aliases() {
        let mut u = json!({
            "prompt_tokens": 1,
            "cached_tokens": 9,
            "prompt_cache_hit_tokens": 9,
            "cache_read_input_tokens": 9,
            "prompt_tokens_details": {"cached_tokens": 9},
            "cache_creation_input_tokens": 9,
        });
        clear_cache_shadow_keys(&mut u);
        assert_eq!(cached_tokens_of(&u), 0, "清除后不应再命中任何缓存别名");
        assert_eq!(cache_write_tokens_of(&u), 0);
        assert_eq!(u["prompt_tokens"], json!(1), "非缓存字段须保留");
    }

    /// 宽口径数值解析：浮点/字符串形态上游同样下发，旧 as_u64 会静默归零。
    #[test]
    fn u64_pair_accepts_float_and_string_forms() {
        assert_eq!(u64_pair(&json!({"prompt_tokens": 10, "completion_tokens": 2})), (10, 2));
        assert_eq!(u64_pair(&json!({"prompt_tokens": 10.0})), (10, 0));
        assert_eq!(u64_pair(&json!({"prompt_tokens": "20"})), (20, 0));
    }
}
