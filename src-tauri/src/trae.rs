//! TRAE (字节跳动) —— 原生渠道。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 登录（模仿 OFM buildTraeLoginURL / exchangeTraeCallback）：打开
//!   `{consoleHost}/authorization?` 带 machine_id / device_id / auth_callback_url /
//!   login_trace_id / x_* 等参数（**不走 PKCE**）；授权页在回调 query 里直接回吐
//!   `refreshToken`（或 `userJwt.Token`）→ 用 `RefreshToken` 调
//!   POST {oauthHost}/cloudide/api/v3/trae/oauth/ExchangeToken
//!   （体 `{ClientID, RefreshToken, ClientSecret:"-", UserID:""}`）→ 再
//!   POST /cloudide/api/v3/trae/GetUserInfo（体 `{ReqSource:"IDE", IDEVersion}`）补账号。
//! - chat：POST {agentHost}/…（TRAE SOLO 协议），头是一整套伪装 IDE 身份：
//!   X-Cloudide-Token / X-Ide-Token（= access_token）、X-Uid、X-App-Id、
//!   X-Ide-Version、X-Device-Type、X-OS-Version、X-Device-Brand，
//!   X-Machine-Id（sha256 轮换派生）、X-Device-Id（userId 播种派生）。
//! - 设备指纹派生（seededStream = sha256("{salt}:{userId}" + counter_be32) 取字节）：
//!   deriveDeviceId15（15 位数字，盐 "devid"）、deriveMarketUserId（UUIDv4 形态，
//!   盐 "market"）、deriveSessionId（32 字节 hex，盐 "sess"）、
//!   deriveRotatingMachineId = sha256("{base}#machine{gen}")[:32]。
//!   登录用的 machine_id / device_id 为每次登录随机生成的 32-hex（模仿 OFM
//!   generateMachineId / generateDeviceId），与 uid 无关（uid 登录后才知）。
//! - 目录：POST {agentHost}/api/ide/v1/batch_get_detail_param。
//! - 签到/额度：POST {ugHost}/trae/api/v2/ug/checkin_credits/{status,claim}；
//!   余额 GET /trae/api/v2/pay/ide_user_ent_usage。
//! - 超额判定：业务码 1005(plan limit) / 4008(quota exceeded) / 4011(soft rate)。

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::{ModelEntry, TokenCreds};

pub const TRAE_SOURCE: &str = "trae";
pub const AGENT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const UG_HOST: &str = "https://api.trae.cn";
pub const OAUTH_HOST: &str = "https://api.trae.com.cn";
pub const CONSOLE_HOST: &str = "https://www.trae.cn";
/// ⚠️ `clientId` 与 `appId` 是**两个不同的值**，用途也不同：
/// - `clientId`（OFM business.mjs L39248）= `en1oxy7wnw8j9n` → 用于**授权页
///   `/authorization` 的 `client_id`** 与 **ExchangeToken 体的 `ClientID`**。
///   传错会直接被服务端判 `Invalid client.`
/// - `appId`（L39249）= `6eefa01c-…` → 只用于 **chat 请求头 `X-App-Id`**。
pub const CLIENT_ID: &str = "en1oxy7wnw8j9n";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const IDE_VERSION: &str = "0.1.52";
/// ideVersionCode / appVersionCode（OFM business.mjs L39250），catalog 请求头需带。
pub const IDE_VERSION_CODE: &str = "20260811";
pub const PLUGIN_VERSION: &str = "2.3.62834";
pub const BATCH_MODELS_PATH: &str = "/api/ide/v1/batch_get_detail_param";
/// SOLO chat 端点（OFM send() L40639 agentHost + 此路径，实测唯一可用路径；
/// 旧 /mcp/v1/chat 已 404）。强制 stream:true。
pub const SOLO_CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";
/// 通道缺省值（OFM TRAE.product.function = "solo_work_lite"）。
pub const TRAE_FUNCTION_DEFAULT: &str = "solo_work_lite";
pub const EXCHANGE_PATH: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";
/// 授权页路径（模仿 OFM buildTraeLoginURL：consoleHost + "/authorization"）。
pub const AUTHORIZATION_PATH: &str = "/authorization";
/// 本地回调路径（模仿 OFM：TRAE 授权页回跳到 /authorize，并在 query 里直接回吐
/// refreshToken / userJwt，不走 PKCE code 交换）。
pub const CALLBACK_PATH: &str = "/authorize";
pub const USER_INFO_PATH: &str = "/cloudide/api/v3/trae/GetUserInfo";
pub const CHECKIN_STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";
pub const CHECKIN_CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
pub const ENT_USAGE_PATH: &str = "/trae/api/v2/pay/ide_user_ent_usage";
pub const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 60_000;

/// seededStream：sha256("{salt}:{seed}" + counter_be32) 连续取字节到 nbytes。
fn seeded_stream(seed: &str, salt: &str, nbytes: usize) -> Vec<u8> {
    let prefix = format!("{}:{}", salt, seed);
    let mut out = Vec::with_capacity(nbytes);
    let mut counter: u32 = 0;
    while out.len() < nbytes {
        let mut hasher = Sha256::new();
        hasher.update(prefix.as_bytes());
        hasher.update(counter.to_be_bytes());
        out.extend_from_slice(&hasher.finalize());
        counter += 1;
    }
    out.truncate(nbytes);
    out
}

/// deriveDeviceId15：15 位数字（seededDigits 盐 "devid"）。
pub fn derive_device_id(user_id: &str) -> String {
    let bytes = seeded_stream(user_id, "devid", 16);
    let digits: String = bytes.iter().map(|b| char::from(b'0' + (b % 10))).collect();
    digits.chars().take(15).collect()
}

/// deriveMarketUserId：UUIDv4 形态（位运算把 6/8 字节改成 v4 标记）。
pub fn derive_market_user_id(user_id: &str) -> String {
    let mut bs = seeded_stream(user_id, "market", 16);
    bs[6] = (bs[6] & 15) | 64;
    bs[8] = (bs[8] & 63) | 128;
    let hex: String = bs.iter().map(|b| format!("{:02x}", b)).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// deriveSessionId：32 字节 hex（盐 "sess"）。
pub fn derive_session_id(user_id: &str) -> String {
    seeded_stream(user_id, "sess", 32).iter().map(|b| format!("{:02x}", b)).collect()
}

/// deriveRotatingMachineId = sha256("{base}#machine{gen}") 的 hex 前 32 字符。
pub fn derive_rotating_machine_id(base: &str, generation: u64) -> String {
    if generation == 0 {
        return base.to_string();
    }
    let mut hasher = Sha256::new();
    hasher.update(format!("{}#machine{}", base, generation).as_bytes());
    let full = hex(&hasher.finalize());
    full[..32].to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Percent-encode a string for use inside a query parameter value.
fn pct(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' =>
                out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 模仿 OFM machineTraceId：machineId+deviceId 拼接，取末 16 字符（不足左补 0）。
pub fn machine_trace_id(machine_id: &str, device_id: &str) -> String {
    let joined = format!("{}{}", machine_id, device_id);
    if joined.len() >= 16 {
        joined[joined.len() - 16..].to_string()
    } else {
        format!("{:0>16}", joined)
    }
}

/// 过期时间换算（模仿 OFM buildTraeCredential/applyTraeRefresh 的优先级）：
/// TokenExpireAt（>1e12 即 ms，否则 ×1e3）> TokenExpireDuration（秒）> JWT exp。
fn trae_expiry_ms(result: &serde_json::Map<String, Value>, access_token: &str) -> i64 {
    let get_num = |k: &str| result.get(k).and_then(|v| v.as_i64());
    if let Some(v) = get_num("TokenExpireAt").or_else(|| get_num("tokenExpireAt")) {
        if v > 1_000_000_000_000 { return v; }
        if v > 0 { return v * 1_000; }
    }
    if let Some(d) = get_num("TokenExpireDuration").or_else(|| get_num("tokenExpireDuration")) {
        if d > 0 { return crate::store::now_ms() + d * 1_000; }
    }
    crate::store::jwt_exp_ms(access_token).unwrap_or(0)
}

/// Build the browser login URL（模仿 OFM buildTraeLoginURL）：
/// `consoleHost/authorization?` 带 machine_id / device_id / auth_callback_url /
/// login_trace_id / x_* 等参数；回调在 query 里直接回吐 refreshToken（或 userJwt），
/// **不走 PKCE code 交换**（OFM 实测 TRAE 现行流程即如此）。
pub fn login_url(port: u16, machine_id: &str, device_id: &str) -> String {
    let callback = pct(&format!("http://127.0.0.1:{}{}", port, CALLBACK_PATH));
    let trace = machine_trace_id(machine_id, device_id);
    let q: &[(&str, &str)] = &[
        ("login_version", "1"),
        ("auth_from", "solo"),
        ("login_channel", "native_ide"),
        ("plugin_version", PLUGIN_VERSION),
        ("auth_type", "local"),
        ("client_id", CLIENT_ID),
        ("redirect", "0"),
        ("login_trace_id", &trace),
        ("auth_callback_url", &callback),
        ("machine_id", machine_id),
        ("device_id", device_id),
        ("x_device_id", device_id),
        ("x_machine_id", machine_id),
        ("x_device_brand", "PC"),
        ("x_device_type", "PC"),
        ("x_os_version", "1.0"),
        ("x_app_version", IDE_VERSION),
        ("x_app_type", "stable"),
    ];
    let mut s = format!("{}{}?", CONSOLE_HOST, AUTHORIZATION_PATH);
    for (i, (k, v)) in q.iter().enumerate() {
        if i > 0 { s.push('&'); }
        s.push_str(k);
        s.push('=');
        s.push_str(v);
    }
    s
}

/// 分类 TRAE 错误：返回 QUOTA / RATE / OTHER（供换号与限流记账）。
pub fn classify_trae_error(msg: &str) -> &'static str {
    if msg.contains("1005") || (msg.contains("1005") && msg.contains("plan")) {
        return "quota";
    }
    if msg.contains("4008") || msg.contains("quota") || msg.contains("exceeded the quota") {
        return "quota";
    }
    if msg.contains("4011") || msg.contains("429") || msg.contains("too many") {
        return "rate";
    }
    "other"
}

/// 通道优先级（OFM business.mjs L39199 resolveChannelList fallback）——
/// 通用通道在前，专用通道兜底。同一 model id 出现在多通道时取优先级最高者。
const TRAE_CHANNELS: &[&str] = &[
    "solo_agent",
    "solo_work_lite",
    "solo_agent_remote",
    "solo_work_remote",
    "solo_agent_lite",
    "chat_v3",
    "builder_v3",
    "solo_coder",
    "solo_design_lite",
    "solo_design_remote",
    "git_ai",
    "code_reviewer",
    "code_review_summary",
    "multimodal",
    "system_diagnosis",
];

/// 请求的 function 全集（OFM fetchModels L46360 bodyObj.functions）——
/// 空 body 会让服务端只回 `allow_tenant_user_add_model` 而无 function_configs，
/// 故必须显式带上这组 function 名。
const TRAE_FUNCTIONS: &[&str] = &[
    "ui_builder_v2", "solo_coder", "chat_v3", "solo_builder", "builder_v3", "builder",
    "chat", "inline_chat", "git_ai", "custom_agent_generation", "utils", "code_reviewer",
    "code_review_summary", "solo_agent", "solo_agent_remote", "solo_work_remote",
    "solo_agent_lite", "solo_work_lite", "solo_design_lite", "solo_design_remote",
    "multimodal", "system_diagnosis",
];

fn s<'a>(entry: &'a Value, k: &str) -> Option<&'a str> {
    entry.get(k).and_then(|v| v.as_str())
}
fn b(entry: &Value, k: &str) -> Option<bool> {
    entry.get(k).and_then(|v| v.as_bool())
}

/// context_window_tokens.dev → .max（OFM readContextWindowField）。
fn trae_context(entry: &Value) -> Option<u64> {
    let raw = entry.get("context_window_tokens").or_else(|| entry.get("ContextWindowTokens"))?;
    raw.get("dev").or_else(|| raw.get("Dev"))
        .or_else(|| raw.get("max")).or_else(|| raw.get("Max"))
        .and_then(|v| v.as_u64()).filter(|n| *n > 0)
}

/// model_detail_list[] 里优先 __dev 后缀的 max_tokens（OFM readDetailMaxTokens）。
fn trae_max_output(entry: &Value) -> Option<u64> {
    let details = entry.get("model_detail_list").or_else(|| entry.get("ModelDetailList"))?.as_array()?;
    let chosen = details.iter()
        .filter(|d| d.is_object())
        .find(|d| s(d, "model_name").unwrap_or("").ends_with("__dev"))
        .or_else(|| details.iter().find(|d| d.is_object()))?;
    chosen.get("max_tokens").or_else(|| chosen.get("MaxTokens"))
        .and_then(|v| v.as_u64()).filter(|n| *n > 0)
}

/// 一条 config_info_list 条目 → ModelEntry（OFM parseTraeConfigEntry）。
/// 返回 None 表示非 chat 模型（usage 非 chat_completion）或被禁用/隐藏。
pub fn trae_entry_of(row: &Value) -> Option<ModelEntry> {
    let id = s(row, "config_name").or_else(|| s(row, "ConfigName")).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return None;
    }
    let usage = s(row, "usage").or_else(|| s(row, "Usage")).unwrap_or("");
    if !usage.is_empty() && usage != "chat_completion" {
        return None;
    }
    // config_switch=false 关闭；is_invisible_to_user=true 隐藏。缺字段按放行。
    if b(row, "config_switch").or_else(|| b(row, "ConfigSwitch")) == Some(false) {
        return None;
    }
    if b(row, "is_invisible_to_user").or_else(|| b(row, "IsInvisibleToUser")) == Some(true) {
        return None;
    }
    let display = row.get("display_config").or_else(|| row.get("DisplayConfig"));
    let name = display
        .and_then(|d| s(d, "display_name").or_else(|| s(d, "DisplayName")))
        .filter(|t| !t.is_empty())
        .unwrap_or(&id)
        .to_string();
    let vision = display.and_then(|d| b(d, "multimodal").or_else(|| b(d, "Multimodal"))).unwrap_or(false);
    let context = trae_context(row).unwrap_or(200_000);
    let max_output = trae_max_output(row).unwrap_or(32_000);
    Some(ModelEntry {
        upstream_id: id,
        name,
        source: TRAE_SOURCE.to_string(),
        vision,
        context,
        max_output,
        multiplier: None,
        billing_status: None,
        channel: None,
    })
}

/// function_configs[] → 去重后的目录（OFM parseTraeBatchModelList）。
pub fn build_trae_catalog(groups: &[Value]) -> Vec<ModelEntry> {
    let rank_of = |c: &str| TRAE_CHANNELS.iter().position(|x| *x == c);
    let mut by_id: std::collections::HashMap<String, (ModelEntry, usize)> = std::collections::HashMap::new();
    for g in groups {
        let channel = match s(g, "function").or_else(|| s(g, "Function")) {
            Some(c) if rank_of(c).is_some() => c.to_string(),
            _ => continue,
        };
        let rank = rank_of(&channel).unwrap();
        let list = match g.get("config_info_list").or_else(|| g.get("ConfigInfoList")).and_then(|v| v.as_array()) {
            Some(l) => l,
            None => continue,
        };
        for item in list {
            if let Some(mut e) = trae_entry_of(item) {
                e.source = TRAE_SOURCE.to_string();
                e.channel = Some(channel.clone());
                match by_id.get_mut(&e.upstream_id) {
                    Some((_, best_rank)) if *best_rank <= rank => {}
                    Some((slot, best_rank)) => { *slot = e.clone(); *best_rank = rank; }
                    None => { by_id.insert(e.upstream_id.clone(), (e, rank)); }
                }
            }
        }
    }
    by_id.into_values().map(|(e, _)| e).collect()
}

/// OpenAI 形态请求体 → SOLO 形态（OFM transformToSOLOBody + transformSOLOMessage）。
/// `channel` 是该模型的匹配通道（function_configs[].function）。
fn build_solo_body(body: &Value, channel: &str) -> Value {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    // config_name 取模型名（TRAE 用它定位模型），去掉 __ 后缀。
    let base = model.split("__").next().unwrap_or(model);
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = body.get("system").and_then(|v| v.as_str()) {
        if !sys.is_empty() {
            messages.push(json!({ "role": "system", "content": [{ "type": "text", "text": sys }] }));
        }
    }
    if let Some(arr) = body.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = solo_content(m.get("content"));
            let mut out = json!({ "role": role, "content": content });
            // assistant 的 function_call 形态：tool_calls → function_call（OFM transformSOLOMessage）。
            if role == "assistant" {
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    if let Some(tc) = tcs.first() {
                        if let Some(f) = tc.get("function") {
                            out["function_call"] = f.clone();
                        }
                    }
                }
            }
            messages.push(out);
        }
    }
    let mut solo = json!({
        "config_name": base,
        "model": base,
        "function": channel,
        "stream": true,
        "messages": messages,
    });
    if let Some(mt) = body.get("max_tokens") { solo["max_tokens"] = mt.clone(); }
    if let Some(tp) = body.get("temperature") { solo["temperature"] = tp.clone(); }
    // tools.parameters 序列化成 JSON 字符串（normalizeTools）。
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            let mut out = Vec::new();
            for t in tools {
                let mut t = t.clone();
                if let Some(params) = t.pointer("/function/parameters") {
                    if params.is_object() {
                        t["function"]["parameters"] = Value::String(params.to_string());
                    }
                }
                out.push(t);
            }
            solo["tools"] = Value::Array(out);
        }
    }
    solo
}

/// 把消息 content 转成 SOLO 的 parts 数组：string→[{type:text}]，
/// 已是数组（多模态）原样透传（image_url 等），null→null。
fn solo_content(content: Option<&Value>) -> Value {
    match content {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => json!([{ "type": "text", "text": s }]),
        Some(Value::Array(parts)) => {
            // 保留 text 段并转为 {type,text}，image_url 段透传。
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(|v| v.as_str()) {
                    Some("text") => out.push(json!({ "type": "text", "text": p.get("text").and_then(|v| v.as_str()).unwrap_or("") })),
                    Some("image_url") => out.push(p.clone()),
                    _ => {}
                }
            }
            Value::Array(out)
        }
        Some(other) => other.clone(),
    }
}

/// SOLO 私有事件流文本 → 一条 OpenAI `chat.completion` JSON。
/// 折叠规则（对齐 OFM consumeSse + solosse.go 注释）：
/// - event:output → data.{response 正文, reasoning_content 思考, tool_calls}
/// - event:token_usage → data.{prompt_tokens, completion_tokens, reasoning_tokens}
/// - event:done → data.finish_reason
/// - event:error → 抛 Err(code+message)
fn translate_trae_sse(raw: &str) -> Result<Value, String> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut usage = json!({});
    let mut finish = "stop".to_string();
    let mut id = String::new();
    // SOLO 事件以空行分块；块内 event:/data: 行。
    for block in raw.split("\n\n") {
        let mut ev = String::new();
        let mut data = String::new();
        for line in block.lines() {
            let t = line.trim_end_matches('\r');
            if let Some(rest) = t.strip_prefix("event:") {
                ev = rest.trim().to_string();
            } else if let Some(rest) = t.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if ev.is_empty() { continue; }
        let parsed: Value = if data.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&data).unwrap_or(Value::Null)
        };
        match ev.as_str() {
            "output" => {
                if id.is_empty() {
                    if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                        id = format!("trae-{}", sid);
                    }
                }
                if let Some(rc) = parsed.get("response").and_then(|v| v.as_str()) { content.push_str(rc); }
                if let Some(rs) = parsed.get("reasoning_content").and_then(|v| v.as_str()) { reasoning.push_str(rs); }
                if let Some(tcs) = parsed.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs { tool_calls.push(tc.clone()); }
                }
            }
            "metadata" => {
                if id.is_empty() {
                    if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                        id = format!("trae-{}", sid);
                    }
                }
            }
            "token_usage" => {
                usage = json!({
                    "prompt_tokens": parsed.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    "completion_tokens": parsed.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    "total_tokens": parsed.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                });
            }
            "done" => {
                if let Some(fr) = parsed.get("finish_reason").and_then(|v| v.as_str()) {
                    finish = fr.to_string();
                }
            }
            "error" => {
                let code = parsed.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
                let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("stream error");
                return Err(format!("trae stream error code={}: {}", code, msg));
            }
            _ => {}
        }
    }
    if content.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
        return Err("trae: empty model response".into());
    }
    let mut message = json!({ "role": "assistant" });
    if !content.is_empty() { message["content"] = json!(content); }
    if !reasoning.is_empty() { message["reasoning_content"] = json!(reasoning); }
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls.iter().map(|tc| {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            // arguments 上游可能给对象，OpenAI 约定给 JSON 字符串。
            let mut f = f;
            if let Some(args) = f.get_mut("arguments") {
                if !args.is_string() && !args.is_null() {
                    *args = Value::String(args.to_string());
                }
            }
            json!({
                "id": tc.get("id").and_then(|v| v.as_str()).unwrap_or("call_0"),
                "type": "function",
                "function": f,
            })
        }).collect();
        message["tool_calls"] = Value::Array(calls);
        if finish == "stop" { finish = "tool_calls".to_string(); }
    }
    let choice = json!({ "index": 0, "message": message, "finish_reason": finish });
    Ok(json!({
        "id": if id.is_empty() { format!("trae-{}", uuid::Uuid::new_v4()) } else { id },
        "object": "chat.completion",
        "created": crate::store::now_ms() / 1000,
        "model": "",
        "choices": [choice],
        "usage": usage,
    }))
}

/// 截断辅助（探针与错误信息复用）。
fn short_text(s: &str) -> String {
    let t: String = s.chars().take(160).collect();
    t.replace('"', "'").replace('\n', " ")
}

pub struct TraeClient {
    http: Client,
    listing: Client,
}

impl TraeClient {
    pub fn new() -> Self {
        let http = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        let listing = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        Self { http, listing }
    }

    /// 登录：用回调回吐的 refreshToken 换正式令牌（模仿 OFM exchangeTraeCallback）。
    /// ExchangeToken 请求体：`{ ClientID, RefreshToken, ClientSecret:"-", UserID:"" }`。
    pub async fn exchange(&self, refresh_token: &str) -> Result<TokenCreds, String> {
        let body = json!({
            "ClientID": CLIENT_ID,
            "RefreshToken": refresh_token,
            "ClientSecret": "-",
            "UserID": ""
        });
        let resp = self.http.post(format!("{}{}", OAUTH_HOST, EXCHANGE_PATH))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body).send().await.map_err(|e| format!("trae exchange: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("trae exchange body: {}", e))?;
        if !status.is_success() {
            let m = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("exchange HTTP {}: {}", status.as_u16(), m));
        }
        let empty = serde_json::Map::new();
        let result = value.get("Result").or_else(|| value.get("result"))
            .and_then(|v| v.as_object()).unwrap_or(&empty);
        let access_token = result.get("Token").or_else(|| result.get("token"))
            .or_else(|| result.get("accessToken")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err("exchange 响应缺少 Token".into());
        }
        let refresh = result.get("RefreshToken").or_else(|| result.get("refreshToken"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let expires_at = trae_expiry_ms(result, &access_token);
        Ok(TokenCreds {
            access_token,
            refresh_token: refresh,
            expires_at,
            ..Default::default()
        })
    }

    /// 补账号信息（模仿 OFM GetUserInfo）：POST，头 X-Cloudide-Token，体
    /// `{ ReqSource:"IDE", IDEVersion }`；响应 Result 含 UserID/ScreenName/
    /// EnterpriseID/NonPlainTextMobile/NonPlainTextEmail。
    pub async fn fetch_user_info(&self, creds: &TokenCreds) -> Result<TokenCreds, String> {
        let resp = self.http.post(format!("{}{}", OAUTH_HOST, USER_INFO_PATH))
            .header("X-Cloudide-Token", &creds.access_token)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&json!({ "ReqSource": "IDE", "IDEVersion": IDE_VERSION }))
            .send().await.map_err(|e| format!("trae userinfo: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("trae userinfo body: {}", e))?;
        if !status.is_success() {
            return Err(format!("userinfo HTTP {}", status.as_u16()));
        }
        let empty = serde_json::Map::new();
        let result = value.get("Result").or_else(|| value.get("result"))
            .and_then(|v| v.as_object()).unwrap_or(&empty);
        let uid = result.get("UserID").or_else(|| result.get("userId")).or_else(|| result.get("uid"))
            .and_then(|v| v.as_str()).unwrap_or(&creds.uid).to_string();
        let nickname = result.get("ScreenName").or_else(|| result.get("screenName"))
            .and_then(|v| v.as_str()).unwrap_or(&creds.nickname).to_string();
        let enterprise_id = result.get("EnterpriseID").or_else(|| result.get("enterpriseId"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let phone = result.get("NonPlainTextMobile").or_else(|| result.get("nonPlainTextMobile"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let email = result.get("NonPlainTextEmail").or_else(|| result.get("nonPlainTextEmail"))
            .and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut extra = creds.extra.clone();
        if !enterprise_id.is_empty() { extra["enterprise_id"] = json!(enterprise_id); }
        if !phone.is_empty() { extra["phone"] = json!(phone); }
        if !email.is_empty() { extra["email"] = json!(email); }
        Ok(TokenCreds { uid, nickname, extra, ..creds.clone() })
    }

    /// chat：SOLO 私有协议（2026-10-19 全实测）。
    /// - 端点 `POST {agentHost}/api/agent/v3/llm_utils_chat`，强制 `stream:true`；
    ///   旧的 `/mcp/v1/chat` 路径 404，非真实通道（已废弃）。
    /// - 请求体把 OpenAI 形态转成 SOLO 形态（transformToSOLOBody）：模型名做
    ///   `config_name`+`model`、带上匹配通道 `function`、字符串 content 拆成
    ///   `[{type:"text",text}]`、tools.parameters 序列化成 JSON 字符串。
    /// - 响应是 SOLO 私有事件流（`event:output/token_usage/done/error`），**不是**
    ///   OpenAI 格式，必须在本地折叠成一条 `chat.completion` JSON 再交给调用方
    ///   （`Upstream::Json`，server.rs 会按 stream 与否统一渲染）。
    /// - `event:error` 在流内携带 code=4001（参数非法，常见于通道与模型不匹配）、
    ///   1005/4008（额度）等——这里映射成 Err，让探针与服务层如实看到失败。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value, machine_gen: u64) -> Result<Value, String> {
        let uid = &creds.uid;
        let base_machine = creds.extra.get("machineId").and_then(|v| v.as_str()).unwrap_or(uid);
        let device_id = creds.extra.get("deviceId").and_then(|v| v.as_str()).unwrap_or("");
        // 通道：优先用条目带来的 function，缺省 solo_work_lite（OFM product.function）。
        let channel = body.get("__channel").and_then(|v| v.as_str())
            .filter(|c| !c.is_empty())
            .unwrap_or(TRAE_FUNCTION_DEFAULT).to_string();
        let solo = build_solo_body(body, &channel);
        let url = format!("{}{}", AGENT_HOST, SOLO_CHAT_PATH);
        let mut req = self.http.post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("User-Agent", format!("Trae/{}", IDE_VERSION))
            .header("Authorization", format!("Cloud-IDE-JWT {}", creds.access_token))
            .header("X-Cloudide-Token", &creds.access_token)
            .header("X-Ide-Token", &creds.access_token)
            .header("X-Uid", uid)
            .header("X-App-Id", APP_ID)
            .header("X-App-Version", "default")
            .header("X-Ide-Version", IDE_VERSION)
            .header("X-Ide-Version-Code", IDE_VERSION_CODE)
            .header("X-App-Version-Code", IDE_VERSION_CODE)
            .header("X-Ide-Version-Type", "stable")
            .header("X-Device-Type", "macos")
            .header("X-OS-Version", "14.0")
            .header("X-Device-Brand", "Apple")
            .header("Request-Traffic-Type", "prod")
            .header("X-Machine-Id", derive_rotating_machine_id(base_machine, machine_gen));
        if !device_id.is_empty() {
            req = req.header("X-Device-Id", device_id);
        }
        let resp = req.json(&solo).send().await.map_err(|e| format!("trae chat: {}", e))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("trae chat HTTP {}: {}", status.as_u16(), short_text(&text)));
        }
        let raw = resp.text().await.map_err(|e| format!("trae chat body: {}", e))?;
        translate_trae_sse(&raw)
    }

    /// 目录：POST batch_get_detail_param（带全 function 名 + SOLO 头）。
    /// 空 body 服务端只回 `allow_tenant_user_add_model`、无 function_configs →
    /// 解析为 0；必须按 OFM fetchModels 的 bodyObj 结构请求（2026-10-19 实测 22 组）。
    pub async fn catalog(&self, creds: &TokenCreds) -> Result<Vec<ModelEntry>, String> {
        let base_machine = creds.extra.get("machineId").and_then(|v| v.as_str()).unwrap_or(&creds.uid);
        let device_id = creds.extra.get("deviceId").and_then(|v| v.as_str()).unwrap_or("");
        let body = json!({
            "functions": TRAE_FUNCTIONS,
            "agent_type": "",
            "current_config_info": { "config_name": "", "is_custom_model": false },
            "mode_type": 0,
            "access_type": 0,
            "ab_force_vids": "",
            "ab_autotest_advanced_mode": 0,
            "show_custom_model": true
        });
        let mut req = self.listing.post(format!("{}{}", AGENT_HOST, BATCH_MODELS_PATH))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", format!("Trae/{}", IDE_VERSION))
            .header("Authorization", format!("Cloud-IDE-JWT {}", creds.access_token))
            .header("X-Cloudide-Token", &creds.access_token)
            .header("X-Ide-Token", &creds.access_token)
            .header("X-Uid", &creds.uid)
            .header("X-App-Id", APP_ID)
            .header("X-App-Version", "default")
            .header("X-Ide-Version", IDE_VERSION)
            .header("X-Ide-Version-Code", IDE_VERSION_CODE)
            .header("X-App-Version-Code", IDE_VERSION_CODE)
            .header("X-Ide-Version-Type", "stable")
            .header("X-Device-Type", "macos")
            .header("X-OS-Version", "14.0")
            .header("X-Device-Brand", "Apple")
            .header("Request-Traffic-Type", "prod")
            .header("X-Machine-Id", derive_rotating_machine_id(base_machine, 0));
        if !device_id.is_empty() {
            req = req.header("X-Device-Id", device_id);
        }
        let resp = req.json(&body).send().await.map_err(|e| format!("trae models: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("trae models body: {}", e))?;
        if !status.is_success() {
            return Err(format!("models HTTP {}", status.as_u16()));
        }
        let groups = value.get("function_configs").or_else(|| value.get("FunctionConfigs"))
            .and_then(|v| v.as_array()).cloned().unwrap_or_default();
        Ok(build_trae_catalog(&groups))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_fingerprint_is_deterministic() {
        let uid = "user-123";
        // 同 seed 两次派生一致（登录态换了机器也不会飘）。
        assert_eq!(derive_device_id(uid), derive_device_id(uid));
        assert_eq!(derive_device_id(uid).len(), 15);
        assert!(derive_device_id(uid).chars().all(|c| c.is_ascii_digit()));
        let uuid = derive_market_user_id(uid);
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4'); // v4 版本位
        assert_eq!(derive_session_id(uid).len(), 64);
    }

    #[test]
    fn machine_id_rotates_by_generation() {
        let base = "machine-base";
        assert_eq!(derive_rotating_machine_id(base, 0), base);
        let g1 = derive_rotating_machine_id(base, 1);
        let g2 = derive_rotating_machine_id(base, 2);
        assert_eq!(g1.len(), 32);
        assert_ne!(g1, g2);
    }

    #[test]
    fn errors_classified() {
        assert_eq!(classify_trae_error("{\"code\":1005,\"msg\":\"plan\"}"), "quota");
        assert_eq!(classify_trae_error("code 4008 quota exceeded"), "quota");
        assert_eq!(classify_trae_error("4011"), "rate");
        assert_eq!(classify_trae_error("network hiccup"), "other");
    }

    #[test]
    fn catalog_deduped_by_channel_priority() {
        // 真实结构（2026-10-19 实测）：function_configs[].config_info_list[]，
        // 条目字段是 config_name/display_config.display_name/usage/config_switch。
        let mk = |name: &str, usage: &str, on: bool| json!({
            "config_name": name,
            "usage": usage,
            "config_switch": on,
            "display_config": { "display_name": name },
            "context_window_tokens": { "dev": 128_000, "max": 256_000 },
            "model_detail_list": [ { "model_name": format!("{name}__dev"), "max_tokens": 8192 } ]
        });
        let groups = vec![
            // chat_v3 优先级低于 solo_agent —— 同 id 应保留 solo_agent 的条目
            json!({ "function": "chat_v3", "config_info_list": [ mk("deepseek-v4", "chat_completion", true) ] }),
            json!({ "function": "solo_agent", "config_info_list": [
                mk("deepseek-v4", "chat_completion", true),
                mk("glm-5.2", "chat_completion", true),
                mk("hidden-one", "chat_completion", false),      // 关闭 → 剔除
                mk("test-entry", "test", true),                  // 非 chat → 剔除
            ]}),
            json!({ "function": "unknown_channel", "config_info_list": [ mk("x", "chat_completion", true) ] }),
        ];
        let cat = build_trae_catalog(&groups);
        let ids: Vec<&str> = cat.iter().map(|e| e.upstream_id.as_str()).collect();
        // 保留 deepseek-v4 + glm-5.2；hidden-one（关闭）/test-entry（非 chat）/x（未知通道）剔除
        assert_eq!(ids.len(), 2, "去重+过滤后应剩 2 个: {ids:?}");
        assert!(ids.contains(&"deepseek-v4") && ids.contains(&"glm-5.2"));
        // solo_agent 通道的条目胜出（rank 更小）
        let d = cat.iter().find(|e| e.upstream_id == "deepseek-v4").unwrap();
        assert_eq!(d.context, 128_000);
        assert_eq!(d.max_output, 8192);
    }

    #[test]
    fn entry_of_filters_and_maps() {
        // usage 缺省视为放行（部分通道不带 usage 字段）
        let e = trae_entry_of(&json!({"config_name": "a", "config_switch": true}));
        assert!(e.is_some() && e.unwrap().upstream_id == "a");
        // is_invisible_to_user 剔除
        assert!(trae_entry_of(&json!({"config_name": "b", "is_invisible_to_user": true})).is_none());
        // 空 id 剔除
        assert!(trae_entry_of(&json!({"config_name": ""})).is_none());
    }

    #[test]
    fn solo_body_maps_openai_to_wire() {
        let openai = json!({
            "model": "deepseek-v4__dev",
            "messages": [
                { "role": "system", "content": "You are helpful." },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 128
        });
        let solo = build_solo_body(&openai, "solo_coder");
        // config_name 去掉 __ 后缀；model 同值；stream 恒 true。
        assert_eq!(solo["config_name"], json!("deepseek-v4"));
        assert_eq!(solo["model"], json!("deepseek-v4"));
        assert_eq!(solo["stream"], json!(true));
        assert_eq!(solo["function"], json!("solo_coder"));
        assert_eq!(solo["max_tokens"], json!(128));
        // 字符串 content → [{type:"text",text}]
        let msgs = solo["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"][0]["type"], json!("text"));
        assert_eq!(msgs[1]["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn translate_folds_output_events() {
        let raw = "event:metadata\ndata:{\"session_id\":\"sess-1\"}\n\n\
                   event:output\ndata:{\"response\":\"OK\",\"reasoning_content\":\"think\"}\n\n\
                   event:token_usage\ndata:{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}\n\n\
                   event:done\ndata:{\"finish_reason\":\"stop\"}\n\n";
        let v = translate_trae_sse(raw).expect("translate");
        assert_eq!(v["id"], json!("trae-sess-1"));
        let msg = &v["choices"][0]["message"];
        assert_eq!(msg["content"], json!("OK"));
        assert_eq!(msg["reasoning_content"], json!("think"));
        assert_eq!(v["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(v["usage"]["prompt_tokens"], json!(11));
    }

    #[test]
    fn translate_surfaces_stream_error() {
        // 通道与模型不匹配 → 流内 code=4001，必须映射成 Err（探针如实看到失败）。
        let raw = "event:error\ndata:{\"code\":4001,\"message\":\"param is invalid\"}\n\n\
                   event:done\ndata:{\"finish_reason\":\"stop\"}\n\n";
        let e = translate_trae_sse(raw).expect_err("must error");
        assert!(e.contains("4001"), "{e}");
    }
}