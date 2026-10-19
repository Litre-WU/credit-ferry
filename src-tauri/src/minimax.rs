//! MiniMax Code —— 原生渠道。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 登录（device code flow）：POST {accountHost}/oauth2/device/code
//!   （clientId=mcode-public, audience=agent-backend, scope=agent.default）
//!   → device_code + verification_uri，用户在浏览器授权 → 轮询
//!   POST {accountHost}/oauth2/token 换 access_token（`mmoat_` 前缀）。
//! - chat：POST {apiHost}/mavis/api/v1/llm/v1/messages，**Anthropic 兼容协议**
//!   （2026-10-19 实测）：请求体 {model, messages(Anthropic 形态), system,
//!   tools{name,description,input_schema}, maxTokens, temperature, stream}；
//!   stream:true 回标准 Anthropic SSE（message_start / content_block_start /
//!   content_block_delta(text_delta|thinking_delta|input_json_delta) /
//!   message_delta(stop_reason,usage) / message_stop）；stream 缺省回单条
//!   JSON message（content[] 块 + usage.input_tokens/output_tokens +
//!   stop_reason + base_resp 信封）。错误信封 base_resp.status_code ≠ 0。
//! - 目录：GET {apiHost}/mavis/api/v1/models。
//! - 签到/额度（保活参考）：/minimax-cloud/api/v1/signin/{status,claim}、
//!   credit/details。
//!
//! 对客户端呈现 OpenAI 形态：`chat()` 发请求前做 OpenAI→Anthropic 请求体
//! 转换；响应侧 `translate_minimax_completion`（非流式）与
//! `AnthropicToOpenAI` 状态机（流式）转回 OpenAI 形态。

use std::time::Duration;

use base64::Engine as _;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::{ModelEntry, TokenCreds};

pub const MINIMAX_SOURCE: &str = "minimax";
pub const ACCOUNT_HOST: &str = "https://account.minimax.cn";
pub const API_HOST: &str = "https://agent.minimax.cn";
pub const CLIENT_ID: &str = "mcode-public";
pub const AUDIENCE: &str = "agent-backend";
pub const SCOPE: &str = "agent.default";
pub const DEVICE_CODE_PATH: &str = "/oauth2/device/code";
pub const TOKEN_PATH: &str = "/oauth2/token";
pub const MODELS_PATH: &str = "/mavis/api/v1/models";
pub const INFER_PATH: &str = "/mavis/api/v1/llm/v1/messages";
pub const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 60_000;

/// base_resp 信封解包：status_code ≠ 0 视为业务错。
pub fn unwrap_envelope(payload: &Value) -> Result<&Value, String> {
    let base = payload.get("base_resp").and_then(|v| v.as_object());
    if let Some(b) = base {
        let code = b.get("status_code").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if code != 0.0 {
            let msg = b.get("status_msg").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("业务错误 {}: {}", code as i64, msg));
        }
    }
    Ok(payload.get("data").filter(|d| d.is_object()).unwrap_or(payload))
}

pub fn minimax_entry_of(row: &Value) -> ModelEntry {
    let id = row.get("id").or_else(|| row.get("model")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let name = row.get("name").or_else(|| row.get("display_name")).and_then(|v| v.as_str())
        .filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
    // 真实结构（2026-10-19 /mavis/api/v1/models 实测）：limit{context,output}
    // + modalities.input[] 含 image/video → 多模态；兼容旧字段名。
    let context = row.pointer("/limit/context").and_then(|v| v.as_u64())
        .or_else(|| row.get("context_window").and_then(|v| v.as_u64()))
        .or_else(|| row.get("contextWindow").and_then(|v| v.as_u64()))
        .unwrap_or(1_000_000);
    let max_output = row.pointer("/limit/output").and_then(|v| v.as_u64())
        .or_else(|| row.get("max_tokens").and_then(|v| v.as_u64()))
        .or_else(|| row.get("maxOutputTokens").and_then(|v| v.as_u64()))
        .unwrap_or(32_768);
    let vision = row.get("modalities").and_then(|m| m.get("input")).and_then(|v| v.as_array())
        .map(|a| a.iter().any(|s| s.as_str().map(|t| t != "text").unwrap_or(false)))
        .unwrap_or(false)
        || row.get("attachment").and_then(|v| v.as_bool()).unwrap_or(false);
    ModelEntry {
        upstream_id: id,
        name,
        source: MINIMAX_SOURCE.to_string(),
        vision,
        context,
        max_output,
        multiplier: None,
        billing_status: None,
        channel: None,
    }
}

/// /mavis/api/v1/models 响应 → 行数组。真实结构（2026-10-19 实测）：
/// `{version, ttlSeconds, providers:[{providerId, config:{models:{<id>:{…}}}}]}`
/// —— models 是 id→定义 的**字典**，把 key 注入成 `id` 字段后展平成行。
/// 兼容旧形态：顶层数组 / data.models 数组直接透传。
pub fn flatten_minimax_models(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(arr) = value.get("models").and_then(|v| v.as_array()) {
        return arr.clone();
    }
    let mut out = Vec::new();
    if let Some(providers) = value.get("providers").and_then(|v| v.as_array()) {
        for p in providers {
            let Some(models) = p.get("config").and_then(|c| c.get("models")).and_then(|m| m.as_object()) else { continue };
            for (id, def) in models {
                let mut row = def.clone();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("id".to_string(), Value::String(id.clone()));
                    // 显示名缺省用 key（部分条目 name 为空串）。
                    let named = obj.get("name").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
                    if !named {
                        obj.insert("name".to_string(), Value::String(id.clone()));
                    }
                }
                out.push(row);
            }
        }
    }
    out
}

pub fn build_minimax_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let e = minimax_entry_of(r);
        if e.upstream_id.is_empty() || !seen.insert(e.upstream_id.clone()) {
            continue;
        }
        out.push(e);
    }
    out
}

/// 极简 percent-encoding（RFC3986 非保留字符透传），用于 form 编码字段值。
fn urlencode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// OpenAI ↔ Anthropic 翻译层（上游协议实测为 Anthropic 兼容，见模块头注释）
// ---------------------------------------------------------------------------

/// OpenAI messages → Anthropic messages + system。
///
/// - `system`/`developer` 角色折叠进顶层 system 字符串；
/// - assistant 的 `tool_calls` → `tool_use` content 块（arguments 对象
///   序列化成 Anthropic 期望的 `input` 对象）；
/// - `tool` 角色消息 → user 消息里的 `tool_result` 块，按顺序聚合；
/// - content 数组里 OpenAI 的 `text`/`image_url` 段原样保留（Anthropic
///   同样接受 {type:text,image}，但 image_url.url → image.source 的
///   完整重排超出本期范围——上游目录标注这三个模型均 vision=false，
///   server.rs 的 strip_image_content 会先剥图，这里只处理文本段）。
pub fn openai_messages_to_anthropic(body: &Value) -> (Vec<Value>, Value) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let empty_arr = Vec::new();
    let arr = body.get("messages").and_then(|v| v.as_array()).unwrap_or(&empty_arr);

    let mut i = 0usize;
    while i < arr.len() {
        let m = &arr[i];
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(t) = text_of(m.get("content")) {
                    system_parts.push(t);
                }
            }
            "tool" => {
                // 收集连续的 tool 结果成一条 user 消息（Anthropic 要求
                // tool_result 块与 user 角色同层）。
                let mut blocks: Vec<Value> = Vec::new();
                while i < arr.len() && arr[i].get("role").and_then(|v| v.as_str()) == Some("tool") {
                    let t = arr[i].get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let content = arr[i].get("content").cloned().unwrap_or(Value::String(String::new()));
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": t,
                        "content": content,
                    }));
                    i += 1;
                }
                if !blocks.is_empty() {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
                continue;
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text_of(m.get("content")) {
                    if !t.is_empty() {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                if let Some(r) = text_of(m.get("reasoning_content")) {
                    if !r.is_empty() {
                        blocks.push(json!({ "type": "thinking", "thinking": r }));
                    }
                }
                if let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for c in calls {
                        let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = c.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args_raw = c.pointer("/function/arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                        let input: Value = serde_json::from_str(args_raw).unwrap_or(Value::Object(Default::default()));
                        blocks.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
                    }
                }
                // OpenAI 允许 assistant 消息只有 content 字符串；Anthropic
                // 需要非空 content，两者都空时塞一个空文本块保持角色交替。
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                messages.push(json!({ "role": "assistant", "content": blocks }));
            }
            _ => {
                // user（及其它未知角色按 user 处理）
                let content = match m.get("content") {
                    Some(Value::String(s)) => json!(s.clone()),
                    Some(Value::Array(parts)) => {
                        let blocks: Vec<Value> = parts
                            .iter()
                            .filter_map(|p| {
                                let pt = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if pt == "text" {
                                    p.get("text").and_then(|v| v.as_str())
                                        .map(|t| json!({ "type": "text", "text": t }))
                                } else {
                                    // 图片等原样透传（正常已被 strip_image_content 剥除）
                                    Some(p.clone())
                                }
                            })
                            .collect();
                        if blocks.is_empty() {
                            Value::String(String::new())
                        } else if blocks.len() == 1
                            && blocks[0].get("type").and_then(|v| v.as_str()) == Some("text")
                        {
                            // 单个文本段折叠成字符串（Anthropic 接受两种形态）。
                            blocks[0]["text"].clone()
                        } else {
                            Value::Array(blocks)
                        }
                    }
                    _ => Value::String(String::new()),
                };
                messages.push(json!({ "role": "user", "content": content }));
            }
        }
        i += 1;
    }
    let system = if system_parts.is_empty() {
        Value::Null
    } else {
        Value::String(system_parts.join("\n\n"))
    };
    (messages, system)
}

/// 提取消息 content 的纯文本（string 直取；array 拼接 text 段）。
fn text_of(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if p.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                        if !out.is_empty() { out.push('\n'); }
                        out.push_str(t);
                    }
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// OpenAI tools（function 包装）→ Anthropic tools（平铺 + input_schema）。
pub fn openai_tools_to_anthropic(tools: &Value) -> Option<Vec<Value>> {
    let arr = tools.as_array()?;
    if arr.is_empty() { return None; }
    let mut out = Vec::new();
    for t in arr {
        if let Some(f) = t.get("function") {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() { continue; }
            let mut o = json!({ "name": name });
            if let Some(d) = f.get("description").and_then(|v| v.as_str()) {
                o["description"] = json!(d);
            }
            o["input_schema"] = f.get("parameters").cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            out.push(o);
        } else {
            // 已经是 Anthropic 形态则透传
            out.push(t.clone());
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Anthropic stop_reason → OpenAI finish_reason。
fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        _ => "stop",
    }
}

/// Anthropic usage → OpenAI usage 形状。
fn map_usage(u: Option<&Value>) -> Option<Value> {
    let u = u?;
    let prompt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    if prompt == 0 && completion == 0 { return None; }
    Some(json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    }))
}

/// 非流式：Anthropic message JSON → OpenAI chat.completion。
pub fn translate_minimax_completion(value: &Value) -> Value {
    let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("chatcmpl");
    let model = value.get("model").cloned().unwrap_or(Value::String(String::new()));
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = value.get("content").and_then(|v| v.as_array()) {
        for b in blocks {
            match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        if !content.is_empty() { content.push('\n'); }
                        content.push_str(t);
                    }
                }
                "thinking" => {
                    if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                        if !reasoning.is_empty() { reasoning.push('\n'); }
                        reasoning.push_str(t);
                    }
                }
                "tool_use" => {
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let args = b.get("input").cloned().unwrap_or(json!({}));
                    tool_calls.push(json!({
                        "id": b.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": { "name": name, "arguments": args.to_string() },
                    }));
                }
                _ => {}
            }
        }
    }
    let finish = value.get("stop_reason").and_then(|v| v.as_str())
        .map(map_stop_reason).unwrap_or("stop");
    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut out = json!({
        "id": id,
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
    });
    if let Some(u) = map_usage(value.get("usage")) {
        out["usage"] = u;
    }
    out
}

/// 流式：Anthropic SSE 事件流 → OpenAI chat.completion.chunk 序列。
///
/// 一个会话一个状态机实例；`feed(chunk)` 接收上游原始字节，返回应当
/// 立即下发给客户端的 OpenAI SSE 文本（可能为空）。`finish()` 收尾：
/// 兜底补 finish 事件与 [DONE]。
pub struct AnthropicToOpenAI {
    model: String,
    id: String,
    created: i64,
    usage: Option<Value>,
    // tool_use 块 index → OpenAI tool_calls 数组下标
    tool_index_map: std::collections::HashMap<u64, usize>,
    next_tool_idx: usize,
    finish_sent: bool,
    role_sent: bool,
    done_emitted: bool,
    // 原始字节缓冲：网络 chunk 可能把多字节汉字/事件边界切断，按 \n\n
    // 在字节层切完整事件后再解码，避免半字符。
    buf: Vec<u8>,
}

impl AnthropicToOpenAI {
    pub fn new() -> Self {
        Self {
            model: String::new(),
            id: "chatcmpl".into(),
            created: 0,
            usage: None,
            tool_index_map: Default::default(),
            next_tool_idx: 0,
            finish_sent: false,
            role_sent: false,
            done_emitted: false,
            buf: Vec::new(),
        }
    }

    fn render(&self, delta: Value, finish_reason: Value) -> String {
        let mut obj = json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
        });
        if let Some(map) = obj.as_object_mut() {
            if let Some(u) = &self.usage {
                map.insert("usage".into(), u.clone());
            }
        }
        format!("data: {}\n\n", obj)
    }

    /// 收到一段上游字节，返回应下发的 OpenAI SSE 文本。
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = String::new();
        self.buf.extend_from_slice(bytes);
        // 在字节层找 \n\n 分隔（换行是 ASCII，绝不含多字节序列），
        // 切出的每个 block 都是完整事件，可安全 from_utf8。
        while let Some(pos) = find_double_newline(&self.buf) {
            let block_bytes: Vec<u8> = self.buf.drain(..(pos.0 + pos.1)).collect();
            match std::str::from_utf8(&block_bytes) {
                Ok(s) => out.push_str(&self.handle_event(s)),
                Err(_) => {}
            }
        }
        out
    }

    fn handle_event(&mut self, block: &str) -> String {
        let mut data = String::new();
        for line in block.lines() {
            let t = line.trim_end_matches('\r');
            if let Some(rest) = t.strip_prefix("data:") {
                if !data.is_empty() { data.push('\n'); }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if data.is_empty() { return String::new(); }
        let Ok(v) = serde_json::from_str::<Value>(&data) else { return String::new(); };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let mut out = String::new();
        match ty {
            "message_start" => {
                if let Some(msg) = v.get("message") {
                    if let Some(id) = msg.get("id").and_then(|x| x.as_str()) {
                        self.id = format!("mm-{}", id);
                    }
                    if let Some(m) = msg.get("model").and_then(|x| x.as_str()) {
                        self.model = m.to_string();
                    }
                }
                if !self.role_sent {
                    self.role_sent = true;
                    out.push_str(&self.render(json!({ "role": "assistant" }), Value::Null));
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                let block_ty = v.pointer("/content_block/type").and_then(|x| x.as_str()).unwrap_or("");
                if block_ty == "tool_use" {
                    let slot = self.next_tool_idx;
                    self.next_tool_idx += 1;
                    self.tool_index_map.insert(idx, slot);
                    let name = v.pointer("/content_block/name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let cid = v.pointer("/content_block/id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    out.push_str(&self.render(
                        json!({ "tool_calls": [{ "index": slot, "id": cid, "type": "function",
                            "function": { "name": name, "arguments": "" } }] }),
                        Value::Null,
                    ));
                }
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                let dtype = d.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) = d.get("text").and_then(|x| x.as_str()) {
                            out.push_str(&self.render(json!({ "content": t }), Value::Null));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = d.get("thinking").and_then(|x| x.as_str()) {
                            out.push_str(&self.render(json!({ "reasoning_content": t }), Value::Null));
                        }
                    }
                    "input_json_delta" => {
                        let idx = v.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                        let slot = self.tool_index_map.get(&idx).copied().unwrap_or(0);
                        if let Some(t) = d.get("partial_json").and_then(|x| x.as_str()) {
                            out.push_str(&self.render(
                                json!({ "tool_calls": [{ "index": slot,
                                    "function": { "arguments": t } }] }),
                                Value::Null,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(u) = map_usage(v.get("usage")) {
                    self.usage = Some(u);
                }
                if !self.finish_sent {
                    if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                        let finish = map_stop_reason(sr);
                        self.finish_sent = true;
                        out.push_str(&self.render(json!({}), json!(finish)));
                    }
                }
            }
            "message_stop" => {
                if !self.finish_sent {
                    self.finish_sent = true;
                    out.push_str(&self.render(json!({}), json!("stop")));
                }
                // [DONE] 统一由 finish() 发，上游异常断流也保证收尾。
            }
            "error" => {
                let msg = v.pointer("/error/message").and_then(|x| x.as_str())
                    .or_else(|| v.get("message").and_then(|x| x.as_str()))
                    .unwrap_or("upstream error");
                out.push_str(&format!(
                    "data: {}\n\n",
                    json!({ "error": { "message": format!("minimax: {}", msg) } })
                ));
                self.finish_sent = true;
            }
            _ => {} // ping 等忽略
        }
        out
    }

    /// 流结束兜底：补 finish + [DONE]，即使上游没发 message_stop。幂等。
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.finish_sent {
            self.finish_sent = true;
            out.push_str(&self.render(json!({}), json!("stop")));
        }
        if !self.done_emitted {
            self.done_emitted = true;
            out.push_str("data: [DONE]\n\n");
        }
        out
    }

    /// 已聚合的 usage（Anthropic message_delta 带进来的）；服务端账本用。
    pub fn usage(&self) -> Option<&Value> {
        self.usage.as_ref()
    }
}

/// 在字节切片里找事件分隔符：`\n\n` 或 `\r\n\r\n`，返回 (起始下标, 长度)。
fn find_double_newline(buf: &[u8]) -> Option<(usize, usize)> {
    for p in 0..buf.len().saturating_sub(1) {
        if buf[p] == b'\n' && buf[p + 1] == b'\n' {
            return Some((p, 2));
        }
        if p + 3 < buf.len() && buf[p] == b'\r' && buf[p + 1] == b'\n' && buf[p + 2] == b'\r' && buf[p + 3] == b'\n' {
            return Some((p, 4));
        }
    }
    None
}

pub struct MinimaxClient {
    http: Client,
    listing: Client,
}

impl MinimaxClient {
    pub fn new() -> Self {
        let http = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        let listing = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        Self { http, listing }
    }

    /// 登录第 1 步：申请 device code。
    /// 模仿 OFM startMinimaxDeviceAuthorization：POST form-urlencoded，带 PKCE S256。
    /// 返回 (device_code, verification_uri_complete, code_verifier)。
    pub async fn start_device_login(&self) -> Result<(String, String, String), String> {
        let code_verifier = crate::store::new_hex(32);
        let digest = Sha256::digest(code_verifier.as_bytes());
        let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        let body = format!(
            "client_id={}&scope={}&audience={}&code_challenge={}&code_challenge_method=S256",
            CLIENT_ID, SCOPE, AUDIENCE, code_challenge
        );
        let resp = self.http.post(format!("{}{}", ACCOUNT_HOST, DEVICE_CODE_PATH))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send().await.map_err(|e| format!("minimax device code: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("minimax device code body: {}", e))?;
        if !status.is_success() {
            return Err(format!("device code HTTP {}", status.as_u16()));
        }
        let data = unwrap_envelope(&value)?;
        let device_code = data.get("device_code").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let verify = data.get("verification_uri_complete")
            .or_else(|| data.get("verification_uri"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        if device_code.is_empty() {
            return Err("device code 响应缺少 device_code".into());
        }
        Ok((device_code, verify, code_verifier))
    }

    /// 登录第 2 步：轮询 token（device_code + client_id 换 access_token）。
    pub async fn poll_device_token(&self, device_code: &str, code_verifier: &str) -> Result<TokenCreds, String> {
        let resp = self.http.post(format!("{}{}", ACCOUNT_HOST, TOKEN_PATH))
            .header("Content-Type", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={}&client_id={}&code_verifier={}",
                urlencode(device_code), urlencode(CLIENT_ID), urlencode(code_verifier)
            ))
            .send().await.map_err(|e| format!("minimax token: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("minimax token body: {}", e))?;
        if !status.is_success() {
            return Err(format!("token HTTP {}", status.as_u16()));
        }
        let data = unwrap_envelope(&value).map_err(|_| "用户尚未完成授权（或 code 过期）".to_string())?;
        let access_token = data.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err("token 响应缺少 access_token（可能还在等待授权）".into());
        }
        let expires_in = data.get("expires_in").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let expires_at = if expires_in > 0.0 {
            crate::store::now_ms() + (expires_in as i64) * 1000
        } else {
            crate::store::jwt_exp_ms(&access_token).unwrap_or(0)
        };
        Ok(TokenCreds {
            access_token,
            refresh_token: data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            expires_at,
            ..Default::default()
        })
    }

    /// 刷新（RT 单次消费，调用方持 refresh_gate）。
    pub async fn refresh(&self, creds: &mut TokenCreds) -> Result<(), String> {
        if creds.refresh_token.is_empty() {
            return Err("minimax 无 refresh_token，请重新登录".into());
        }
        let resp = self.http.post(format!("{}{}", ACCOUNT_HOST, TOKEN_PATH))
            .header("Content-Type", "application/json")
            .json(&json!({
                "client_id": CLIENT_ID, "refresh_token": creds.refresh_token,
                "grant_type": "refresh_token",
            }))
            .send().await.map_err(|e| format!("minimax refresh: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("minimax refresh body: {}", e))?;
        if !status.is_success() {
            return Err(format!("REFRESH_TERMINAL:refresh HTTP {}", status.as_u16()));
        }
        let data = unwrap_envelope(&value)?;
        if let Some(t) = data.get("access_token").and_then(|v| v.as_str()) {
            creds.access_token = t.to_string();
            if let Some(e) = data.get("expires_in").and_then(|v| v.as_f64()) {
                creds.expires_at = crate::store::now_ms() + (e as i64) * 1000;
            }
        }
        if let Some(r) = data.get("refresh_token").and_then(|v| v.as_str()) {
            creds.refresh_token = r.to_string();
        }
        Ok(())
    }

    /// 目录：GET /mavis/api/v1/models。
    pub async fn catalog(&self, creds: &TokenCreds) -> Result<Vec<ModelEntry>, String> {
        let resp = self.listing.get(format!("{}{}", API_HOST, MODELS_PATH))
            .header("Authorization", &format!("Bearer {}", creds.access_token))
            .header("Accept", "application/json")
            .send().await.map_err(|e| format!("minimax models: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("minimax models body: {}", e))?;
        if !status.is_success() {
            return Err(format!("models HTTP {}", status.as_u16()));
        }
        let data = unwrap_envelope(&value).unwrap_or(&value);
        let rows = flatten_minimax_models(data);
        Ok(build_minimax_catalog(&rows))
    }

    /// chat：OpenAI 形态请求体 → Anthropic 协议，发往 /mavis/api/v1/llm/v1/messages。
    /// 响应仍是 Anthropic 形态（流式 SSE / 非流式单条 message），由 server.rs
    /// 的 Upstream::HttpTranslate / translate_minimax_completion 转回 OpenAI。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value) -> Result<reqwest::Response, String> {
        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let (messages, system) = openai_messages_to_anthropic(body);
        let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(32_768);
        let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
        let mut payload = json!({
            "model": model, "messages": messages, "maxTokens": max_tokens,
        });
        if system.is_string() {
            payload["system"] = system;
        }
        if let Some(t) = body.get("temperature") {
            payload["temperature"] = t.clone();
        }
        if let Some(tools) = body.get("tools").and_then(openai_tools_to_anthropic) {
            payload["tools"] = Value::Array(tools);
        }
        // stream 是布尔透传：Anthropic messages API 认 body.stream；
        // server.rs 分支已显式写入，这里保底再同步一次。
        payload["stream"] = json!(stream);
        self.http.post(format!("{}{}", API_HOST, INFER_PATH))
            .header("Authorization", &format!("Bearer {}", creds.access_token))
            .header("Content-Type", "application/json")
            .header("Accept", if stream { "text/event-stream" } else { "application/json" })
            .json(&payload)
            .send().await.map_err(|e| format!("minimax chat: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_unwraps_base_resp() {
        // base_resp.status_code=0 → 正常解包 data。
        let ok = json!({ "base_resp": { "status_code": 0 }, "data": { "id": "m1" } });
        assert_eq!(unwrap_envelope(&ok).unwrap()["id"], "m1");
        // status_code≠0 → 业务错误。
        let bad = json!({ "base_resp": { "status_code": 1001, "status_msg": "auth failed" } });
        assert!(unwrap_envelope(&bad).is_err());
    }

    #[test]
    fn catalog_deduped() {
        let rows = vec![
            json!({ "id": "MiniMax-M3", "name": "M3", "context_window": 1_000_000 }),
            json!({ "id": "MiniMax-M3", "name": "dup" }),
        ];
        let cat = build_minimax_catalog(&rows);
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0].context, 1_000_000);
    }

    #[test]
    fn providers_envelope_flattens_and_maps_limits() {
        // 真实响应形态（2026-10-19 实测）：providers[].config.models{id:{…}}。
        let body = json!({
            "version": "1.0", "ttlSeconds": 300,
            "providers": [
                { "providerId": "minimax", "config": { "name": "MiniMax", "models": {
                    "MiniMax-M3": { "name": "M3", "limit": { "context": 512_000, "output": 128_000 },
                                    "modalities": { "input": ["text", "image"] } },
                    "MiniMax-M2.7": { "name": "", "limit": { "context": 200_000, "output": 128_000 },
                                      "modalities": { "input": ["text"] } }
                }}},
                { "providerId": "other", "config": { "models": {
                    "K1.5": { "name": "K1.5" } }}
                }
            ]
        });
        let rows = flatten_minimax_models(&body);
        assert_eq!(rows.len(), 3);
        let cat = build_minimax_catalog(&rows);
        assert_eq!(cat.len(), 3);
        let m3 = cat.iter().find(|e| e.upstream_id == "MiniMax-M3").unwrap();
        assert_eq!(m3.context, 512_000);
        assert_eq!(m3.max_output, 128_000);
        assert!(m3.vision, "modalities.input 含 image → vision");
        // name 空串回退到 key
        let m27 = cat.iter().find(|e| e.upstream_id == "MiniMax-M2.7").unwrap();
        assert_eq!(m27.name, "MiniMax-M2.7");
        assert!(!m27.vision);
        // limit 缺省值
        let k = cat.iter().find(|e| e.upstream_id == "K1.5").unwrap();
        assert_eq!(k.context, 1_000_000);
    }

    #[test]
    fn anthropic_completion_translates_to_openai() {
        // 2026-10-19 实测的非流式 message 形态。
        let raw = json!({
            "id": "072512055fba", "type": "message", "role": "assistant", "model": "MiniMax-M3",
            "content": [{ "text": "1+1等于2。", "type": "text" }],
            "usage": { "input_tokens": 45, "output_tokens": 7,
                       "cache_read_input_tokens": 128, "service_tier": "standard" },
            "stop_reason": "end_turn", "base_resp": { "status_code": 0 }
        });
        let o = translate_minimax_completion(&raw);
        assert_eq!(o["object"], "chat.completion");
        assert_eq!(o["choices"][0]["message"]["content"], "1+1等于2。");
        assert_eq!(o["choices"][0]["finish_reason"], "stop");
        assert_eq!(o["usage"]["prompt_tokens"], 45);
        assert_eq!(o["usage"]["completion_tokens"], 7);
        assert_eq!(o["usage"]["total_tokens"], 52);
    }

    #[test]
    fn anthropic_tool_use_translates_to_openai_tool_calls() {
        let raw = json!({
            "id": "t1", "model": "m",
            "content": [
                { "type": "text", "text": "查一下" },
                { "type": "tool_use", "id": "call_9", "name": "get_weather",
                  "input": { "city": "北京" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });
        let o = translate_minimax_completion(&raw);
        assert_eq!(o["choices"][0]["finish_reason"], "tool_calls");
        let calls = o["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_9");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        // arguments 必须是字符串（OpenAI 约定）
        let args = calls[0]["function"]["arguments"].as_str().unwrap();
        assert!(args.contains("北京"));
    }

    #[test]
    fn sse_state_machine_translates_text_stream() {
        // 2026-10-19 实测的 Anthropic SSE 序列。
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"abc123\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"MiniMax-M3\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"因为\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"蓝光。\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":45,\"output_tokens\":29}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        // 逐字节喂入，验证缓冲对事件边界与多字节汉字切分安全。
        let mut sm = AnthropicToOpenAI::new();
        let mut text = String::new();
        let mut out_all = String::new();
        for b in raw.as_bytes() {
            let out = sm.feed(&[*b]);
            out_all.push_str(&out);
            for line in out.lines() {
                if let Some(p) = line.strip_prefix("data: ") {
                    if p == "[DONE]" { continue; }
                    let v: Value = serde_json::from_str(p).unwrap();
                    if let Some(t) = v.pointer("/choices/0/delta/content").and_then(|x| x.as_str()) {
                        text.push_str(t);
                    }
                }
            }
        }
        out_all.push_str(&sm.finish());
        assert_eq!(text, "因为蓝光。");
        assert!(out_all.contains("mm-abc123"));
        assert!(out_all.contains("MiniMax-M3"));
        assert!(out_all.contains("end_turn") == false); // OpenAI 侧不应出现 Anthropic 词
        assert!(out_all.contains("\"finish_reason\":\"stop\"") || out_all.contains("\"finish_reason\": \"stop\""));
        assert!(out_all.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn sse_state_machine_translates_tool_calls() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"t9\",\"model\":\"MiniMax-M3\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"ci\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ty\\\":\\\"北京\\\"}\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut sm = AnthropicToOpenAI::new();
        let mut out = sm.feed(raw.as_bytes());
        out.push_str(&sm.finish());
        let mut args = String::new();
        let mut finish = String::new();
        let mut got_id = false;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("data: ") {
                if p == "[DONE]" { continue; }
                let v: Value = serde_json::from_str(p).unwrap();
                if let Some(calls) = v.pointer("/choices/0/delta/tool_calls").and_then(|x| x.as_array()) {
                    for c in calls {
                        if let Some(a) = c.pointer("/function/arguments").and_then(|x| x.as_str()) {
                            args.push_str(a);
                        }
                        if c.get("id").and_then(|x| x.as_str()) == Some("call_1") {
                            got_id = true;
                        }
                    }
                }
                if let Some(f) = v.pointer("/choices/0/finish_reason").and_then(|x| x.as_str()) {
                    finish = f.to_string();
                }
            }
        }
        assert!(got_id, "content_block_start 应带出 tool id/name 首帧");
        assert_eq!(args, "{\"city\":\"北京\"}");
        assert_eq!(finish, "tool_calls");
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn sse_finish_emits_done_once() {
        // 上游没发 message_stop 直接断流：finish() 要兜底 finish + DONE。
        let raw = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"z\",\"model\":\"m\"}}\n\n\
                   event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        let mut sm = AnthropicToOpenAI::new();
        let mut out = sm.feed(raw.as_bytes());
        out.push_str(&sm.finish());
        out.push_str(&sm.finish()); // 二次调用也不重复 DONE
        assert_eq!(out.matches("[DONE]").count(), 1);
        assert!(out.contains("\"finish_reason\":\"stop\"") || out.contains("\"finish_reason\": \"stop\""));
    }

    #[test]
    fn openai_messages_convert_to_anthropic() {
        let body = json!({
            "model": "MiniMax-M3",
            "messages": [
                { "role": "system", "content": "你是一个助手" },
                { "role": "user", "content": "北京天气？" },
                { "role": "assistant", "content": null,
                  "tool_calls": [{ "id": "call_1", "type": "function",
                      "function": { "name": "get_weather", "arguments": "{\"city\":\"北京\"}" } }] },
                { "role": "tool", "tool_call_id": "call_1", "content": "晴 25 度" },
                { "role": "user", "content": [{ "type": "text", "text": "谢谢" }] }
            ]
        });
        let (messages, system) = openai_messages_to_anthropic(&body);
        assert_eq!(system, json!("你是一个助手"));
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        let blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"]["city"], "北京");
        assert_eq!(messages[2]["role"], "user");
        let tr = &messages[2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "call_1");
        // 单个 text 段简化为字符串（Anthropic 两种形态都接受）
        assert_eq!(messages[3]["content"], "谢谢");
    }

    #[test]
    fn tools_convert_to_anthropic_schema() {
        let tools = json!([
            { "type": "function", "function": {
                "name": "get_weather", "description": "查天气",
                "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } } }
        ]);
        let out = openai_tools_to_anthropic(&tools).unwrap();
        assert_eq!(out[0]["name"], "get_weather");
        assert_eq!(out[0]["description"], "查天气");
        assert!(out[0]["input_schema"]["properties"]["city"].is_object());
    }
}