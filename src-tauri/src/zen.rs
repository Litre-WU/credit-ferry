//! Zen / OpenCode free-lane upstream — the anonymous `opencode.ai` gateway.
//!
//! Wire facts (ported from the Our Free Model service, `src/upstream.js` +
//! `src/catalog.js`, and re-verified live from this machine):
//! - The free lane lives at `https://opencode.ai/inference/v1` (the old
//!   `/zen/v1` chat path answers 403 FreeTierError for every free id).
//! - `GET /inference/v1/models` returns the full mixed roster (~85 ids);
//!   only the free slice answers anonymously — judged by
//!   `/(?:^|[-_])free(?:$|[-_.])/` plus ALWAYS_FREE (`union-alpha`,
//!   `space-bunny-free`); everything else is a paid id that 403s here.
//! - `POST /inference/v1/chat/completions` demands the full client
//!   fingerprint or the gateway refuses with 403 FreeTierError:
//!   `authorization: Bearer public` (the pooled anonymous credential — there
//!   is no per-user secret), `user-agent: opencode/1.18.31` (>=1.17; a UA
//!   containing `deepseek-harness` is refused by NAME), `x-opencode-client:
//!   cli`, `x-opencode-session` AND `x-opencode-session-id` (same value —
//!   omitting either 403s), `x-opencode-request`, `x-opencode-project: global`.
//! - Session ids must be MINTED in the gateway's canonical shape
//!   (`ses_` + 12 hex of `~((ms<<12)|seq)` + 14 base62). A sha256 digest or
//!   any other random hex is refused even with every other header right.
//! - Quota is accounted PER SESSION: minting a fresh id per request trips
//!   429 FreeUsageLimitError faster than reusing one. So one conversation
//!   maps onto one stable minted id, persisted in `zen-sessions.json`
//!   (seed = first-message identity, which every OpenAI client replays).
//! - The tool gate (measured 2026-10-14): `bash` AND `read` must be declared,
//!   each with a parameter schema carrying at least one property; empty
//!   `properties: {}` is refused, as is either name alone. `glob`/`grep` are
//!   kept because the real client sends all four. Missing slots are filled by
//!   promoting a donor that can genuinely answer (`pwsh` → `bash`), and only
//!   then by a self-disabling decoy.
//! - Capabilities come from a vetted local table (the upstream listing
//!   discloses ids and nothing else); same regex ladder as OFM's CAPABILITIES.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::ModelEntry;

pub const ZEN_BASE: &str = "https://opencode.ai";
pub const ZEN_SOURCE: &str = "zen";
const CHAT_PATH: &str = "/inference/v1/chat/completions";
const MODELS_PATH: &str = "/inference/v1/models";
/// The gateway enforces a User-Agent version >= 1.17 and refuses the string
/// `deepseek-harness` by name — this exact value measured 2026-10-14.
pub const CLIENT_UA: &str = "opencode/1.18.31";
const BASE62: [u8; 62] = *b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Free-id rule, hand-rolled to avoid pulling a regex crate for one pattern.: `(?:^|[-_])free(?:$|[-_.])`.
/// Hand-rolled to avoid pulling a regex crate in for one pattern.
fn id_is_free(base: &str) -> bool {
    // ALWAYS_FREE ids answer without carrying the -free suffix.
    if base == "union-alpha" || base == "space-bunny-free" {
        return true;
    }
    let bytes = base.as_bytes();
    let needle = b"free";
    let mut from = 0usize;
    while let Some(pos) = find_sub(bytes, needle, from) {
        let ok_before = pos == 0 || bytes[pos - 1] == b'-' || bytes[pos - 1] == b'_';
        let end = pos + 4;
        let ok_after = end == bytes.len()
            || bytes[end] == b'-'
            || bytes[end] == b'_'
            || bytes[end] == b'.';
        if ok_before && ok_after {
            return true;
        }
        from = end;
    }
    false
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// The local capability baseline (mirrors OFM `CAPABILITIES`, first match wins).
fn capabilities_for(base: &str) -> (bool, u64, u64) {
    // (vision, context, max_output)
    if base.starts_with("mimo") && (base.contains("v2.6") || base.contains("v2.5")) {
        (true, 1_048_576, 131_072)
    } else if base.starts_with("mimo") {
        (true, 262_144, 131_072)
    } else if base.starts_with("muse-spark") || base.starts_with("muse_spark") {
        (true, 1_048_576, 131_072)
    } else if base.starts_with("nemotron") {
        // OFM's table paints all nemotron vision-false, but this lane's own
        // `-super-vl` ids answered an image probe correctly (measured
        // 2026-10-17) — the VL variant is the exception.
        if base.contains("-vl") {
            (true, 1_000_000, 131_072)
        } else {
            (false, 128_000, 32_768)
        }
    } else if base.starts_with("ling") {
        (false, 128_000, 32_768)
    } else if base.starts_with("space-bunny") || base.starts_with("space_bunny") {
        (true, 262_144, 65_536)
    } else if base.starts_with("union") {
        (true, 262_144, 131_072)
    } else if base.starts_with("deepseek") {
        (false, 128_000, 64_000)
    } else if base.starts_with("jev") {
        (false, 32_768, 4_096)
    } else if base.starts_with("beam") {
        (false, 1_048_576, 131_072)
    } else if base.starts_with("glm") && base.contains("flash") {
        (false, 128_000, 32_768)
    } else {
        (false, 131_072, 32_768)
    }
}

/// Human-facing display names, so a raw upstream id never reaches the picker
/// (same table as OFM's DISPLAY_NAMES for the ids the lane lists today).
fn display_name(base: &str) -> String {
    let known = match base {
        "big-pickle" => "Big Pickle",
        "beam-free" => "Beam 1.0",
        "mimo-v2.6-flash-free" => "MiMo V2.6 Flash",
        "nemotron-3.5-super-vl-free" => "Nemotron 3.5 Super VL",
        "nemotron-3.5-lightning-free" => "Nemotron 3.5 Lightning",
        "nemotron-3-ultra-free" => "Nemotron 3 Ultra",
        "jev-1.13-free" => "Jev 1.13",
        "longcat-2.5-preview-free" => "LongCat 2.5 Preview",
        "deepseek-v4.1-flash-free" => "DeepSeek V4.1 Flash",
        "deepseek-v4-flash-free" => "DeepSeek V4 Flash",
        "glm-5.3-flash-free" => "GLM 5.3 Flash",
        "space-bunny-free" => "Space Bunny",
        "union-alpha" => "Union Alpha",
        "ling-3.0-flash-fin-free" => "Ling 3.0 Flash Fin",
        "ling-3.1-flash-free" => "Ling 3.1 Flash",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    // title-case the bare id: `foo-2.3-bar` → `Foo 2.3 Bar`
    let cleaned: String = base.chars().map(|c| if c == '-' || c == '_' || c == '.' { ' ' } else { c }).collect();
    cleaned
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(|w| {
            if w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                w.to_string()
            } else {
                let mut ch = w.chars();
                match ch.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                    None => w.to_string(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `ses_` + 12 hex of `~((ms<<12)|seq)` (low 48 bits, big-endian) + 14 base62.
/// This exact shape is what the gateway's session-id check accepts.
pub fn mint_session_id(ms: u64, seq: u32) -> String {
    let value = !((ms << 12) | (seq as u64 & 0xfff));
    format!("ses_{}{}", hex_low48(value), base62_random(14))
}

/// `msg_` + 12 hex of `((ms<<12)|1)` + 14 base62.
pub fn mint_request_id(ms: u64) -> String {
    let value = (ms << 12) | 1;
    format!("msg_{}{}", hex_low48(value), base62_random(14))
}

fn hex_low48(value: u64) -> String {
    let mut out = String::with_capacity(12);
    for i in 0..6u32 {
        let byte = ((value >> (40 - 8 * i)) & 0xff) as u8;
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn base62_random(n: usize) -> String {
    let bytes = crate::store::random_bytes(n);
    bytes.iter().map(|&b| BASE62[(b % 62) as usize] as char).collect()
}

/// Canonical session-shape gate.
pub fn session_shape_ok(id: &str) -> bool {
    let rest = match id.strip_prefix("ses_") {
        Some(r) => r,
        None => return false,
    };
    if rest.len() != 26 {
        return false;
    }
    let (hex, tail) = rest.split_at(12);
    hex.bytes().all(|b| b.is_ascii_hexdigit())
        && tail.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Parse `{data:[{id}]}` (or a bare array / `{models:[…]}`) into id strings.
fn parse_listing(payload: &Value) -> Vec<String> {
    let rows = payload
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| payload.get("models").and_then(|m| m.as_array()))
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .filter_map(|r| match r {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
            _ => None,
        })
        .collect()
}

/// The free roster: filter by the free-id rule, dedupe, attach local caps.
pub fn build_zen_catalog(ids: &[String]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in ids {
        let id = raw.trim();
        if id.is_empty() || !id_is_free(id) {
            continue;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        let (vision, context, max_output) = capabilities_for(id);
        out.push(ModelEntry {
            upstream_id: id.to_string(),
            name: display_name(id),
            source: ZEN_SOURCE.to_string(),
            vision,
            context,
            max_output,
            multiplier: Some(0.0),
            billing_status: Some("免费车道（匿名公共池）".to_string()),
        });
    }
    out
}

/// Identity seed for one conversation: the first message's role+text, which
/// every OpenAI client replays on every turn of the same conversation.
pub fn conversation_seed(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    let first = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let mut hasher = Sha256::new();
    hasher.update(b"credit-ferry-zen\0");
    hasher.update(first.to_string().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest.iter().take(16) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// The four names the free tier wants declared (case-canonical).
const FINGERPRINT_TOOLS: [&str; 4] = ["bash", "glob", "grep", "read"];

fn tool_name(tool: &Value) -> &str {
    if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
        if !n.trim().is_empty() {
            return n.trim();
        }
    }
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .unwrap_or("")
}

fn is_quartet(name: &str) -> bool {
    let lower = name.to_lowercase();
    FINGERPRINT_TOOLS.contains(&lower.as_str())
}

/// Ensure one slot's schema declares at least one property (the gate refuses
/// `properties: {}`); leave names/descriptions as the caller wrote them.
fn with_declared_property(tool: &mut Value) {
    let schema_key = if tool.get("input_schema").is_some() {
        "input_schema"
    } else {
        "parameters"
    };
    let holder = if tool.get("function").and_then(|f| f.get(schema_key)).is_some() {
        tool.get_mut("function").unwrap()
    } else {
        tool
    };
    let props_nonempty = holder
        .get(schema_key)
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
        .map(|o| !o.is_empty())
        .unwrap_or(false);
    if props_nonempty {
        return;
    }
    let mut schema = holder.get(schema_key).cloned().unwrap_or(json!({}));
    if !schema.is_object() {
        schema = json!({});
    }
    schema["type"] = json!("object");
    schema["properties"] = json!({
        "input": { "type": "string", "description": "This tool is currently unavailable and must not be used." }
    });
    holder[schema_key] = schema;
}

/// Satisfy the free-tier fingerprint gate on `body.tools` (chat shape):
/// canonicalise case, drop duplicates, promote donors (`pwsh`→`bash`),
/// append self-disabling decoys for empty slots, stamp a placeholder property
/// onto `bash`/`read`. Returns the rename map (canonical → caller spelling)
/// so the response side could restore names; the pass-through lane leaves
/// decoy calls unknown to the client exactly like OFM's does for bare sessions.
pub fn apply_fingerprint(body: &mut Value) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let tools: Vec<Value> = body.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let had_client_tools = !tools.is_empty();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for mut tool in tools {
        let current = tool_name(&tool).to_string();
        let lower = current.to_lowercase();
        if !is_quartet(&current) {
            out.push(tool);
            continue;
        }
        let key = lower.clone();
        if !seen.insert(key.clone()) {
            continue; // upstream rejects Bash + bash as a duplicate
        }
        if current != key {
            map.insert(key.clone(), current.clone());
            let fnname = tool.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
            if !fnname.is_empty() {
                tool["function"]["name"] = json!(key);
            } else {
                tool["name"] = json!(key);
            }
        }
        out.push(tool);
    }

    // Promote a donor that can genuinely answer for a missing slot.
    let donors: HashMap<&str, &[&str]> = [("bash", &["pwsh"][..])].into_iter().collect();
    let mut promoted: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in FINGERPRINT_TOOLS {
        if seen.contains(name) {
            continue;
        }
        let donor_list = donors.get(name).copied().unwrap_or(&[]);
        let index = out.iter().position(|tool| {
            let original = tool_name(tool).to_lowercase();
            if original.is_empty() || is_quartet(&original) {
                return false;
            }
            !promoted.contains(&original) && donor_list.contains(&original.as_str())
        });
        let Some(i) = index else { continue };
        let original = tool_name(&out[i]).to_lowercase();
        promoted.insert(original.clone());
        map.insert(name.to_string(), original);
        let has_fn = out[i].get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).is_some();
        if has_fn {
            out[i]["function"]["name"] = json!(name);
        } else {
            out[i]["name"] = json!(name);
        }
        seen.insert(name.to_string());
    }

    // Self-disabling decoys for whatever is still missing.
    for name in FINGERPRINT_TOOLS {
        if seen.contains(name) {
            continue;
        }
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "This tool is currently unavailable and must not be used.",
                "parameters": { "type": "object", "properties": {} }
            }
        }));
    }

    // bash/read must carry a non-empty schema.
    for tool in out.iter_mut() {
        let name = tool_name(tool);
        if name == "bash" || name == "read" {
            with_declared_property(tool);
        }
    }

    if body.get("tool_choice").and_then(|v| v.as_str()).unwrap_or("").is_empty()
        && body.get("tool_choice").is_none()
    {
        // Only pin `none` when the caller fielded no tools at all — decoys
        // must never be callable, and a real caller's own choice stays put.
        if !had_client_tools {
            body["tool_choice"] = json!("none");
        }
    }
    body["tools"] = Value::Array(out);
    map
}

pub struct ZenClient {
    http: Client,
    listing_http: Client,
}

impl ZenClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        let listing_http = Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { http, listing_http }
    }

    /// The mixed roster filtered to the free slice (`GET /inference/v1/models`).
    pub async fn catalog(&self) -> Result<Vec<ModelEntry>, String> {
        let resp = self
            .listing_http
            .get(format!("{}{}", ZEN_BASE, MODELS_PATH))
            .header("authorization", "Bearer public")
            .header("user-agent", CLIENT_UA)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("zen listing: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("zen listing body: {}", e))?;
        if !status.is_success() {
            return Err(format!("upstream HTTP {}", status.as_u16()));
        }
        Ok(build_zen_catalog(&parse_listing(&value)))
    }

    /// POST one turn with the full client fingerprint. `session` must already
    /// be a minted, canonical-shaped id from the session map.
    pub async fn chat(&self, body: &Value, session: &str) -> Result<reqwest::Response, String> {
        let request_id = mint_request_id(crate::store::now_ms() as u64);
        let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
        let accept = if stream { "text/event-stream" } else { "*/*" };
        self.http
            .post(format!("{}{}", ZEN_BASE, CHAT_PATH))
            .header("content-type", "application/json")
            .header("authorization", "Bearer public")
            .header("user-agent", CLIENT_UA)
            .header("x-opencode-client", "cli")
            .header("x-opencode-session", session)
            .header("x-opencode-session-id", session)
            .header("x-opencode-request", &request_id)
            .header("x-opencode-project", "global")
            .header("accept", accept)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("zen chat: {}", e))
    }
}

/// Fold a whole SSE chat stream into one `chat.completion` JSON object.
///
/// The lane answers 403 FreeTierError for `stream:false`, but clients that
/// asked for a non-streaming response still need a single JSON body — so the
/// ferry forces the stream upstream and folds it back here. Concatenates
/// `delta.content` and `delta.tool_calls` arguments, takes the finish reason
/// and the last usage object seen.
pub fn fold_sse(raw: &str) -> Value {
    let mut content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut finish = Value::Null;
    let mut usage: Option<Value> = None;
    let mut id = String::from("chatcmpl");
    let mut model = String::new();
    let mut created = 0i64;
    let mut role = String::from("assistant");

    for line in raw.lines() {
        let Some(rest) = line.strip_prefix("data:") else { continue };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else { continue };
        if let Some(x) = v.get("id").and_then(|i| i.as_str()) {
            id = x.to_string();
        }
        if let Some(x) = v.get("model").and_then(|m| m.as_str()) {
            model = x.to_string();
        }
        if let Some(x) = v.get("created").and_then(|c| c.as_i64()) {
            created = x;
        }
        if v.get("usage").filter(|u| !u.is_null()).is_some() {
            usage = v.get("usage").cloned();
        }
        let Some(choice) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first().cloned()) else {
            continue;
        };
        if choice.get("finish_reason").filter(|f| !f.is_null()).is_some() {
            finish = choice["finish_reason"].clone();
        }
        let Some(delta) = choice.get("delta") else { continue };
        if let Some(r) = delta.get("role").and_then(|r| r.as_str()) {
            role = r.to_string();
        }
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            content.push_str(c);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for call in calls {
                let idx = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                while tool_calls.len() <= idx {
                    tool_calls.push(json!({"id": null, "type": "function",
                        "function": {"name": "", "arguments": ""}}));
                }
                let slot = &mut tool_calls[idx];
                if let Some(c_id) = call.get("id").and_then(|v| v.as_str()) {
                    slot["id"] = json!(c_id);
                }
                if let Some(f) = call.get("function") {
                    if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
                        slot["function"]["name"] = json!(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(|v| v.as_str()) {
                        let cur = slot["function"]["arguments"].as_str().unwrap_or("").to_string();
                        slot["function"]["arguments"] = json!(cur + a);
                    }
                }
            }
        }
    }
    tool_calls.retain(|t| !t["function"]["name"].as_str().unwrap_or("").is_empty()
        || !t["function"]["arguments"].as_str().unwrap_or("").is_empty());

    let mut message = json!({ "role": role });
    if content.is_empty() && !tool_calls.is_empty() {
        message["content"] = Value::Null;
    } else {
        message["content"] = json!(content);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    let mut out = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [ { "index": 0, "message": message, "finish_reason": finish } ],
    });
    if let Some(u) = usage {
        out["usage"] = u;
    }
    out
}

/// Load the persisted seed→session map from `zen-sessions.json`.
pub fn load_session_map(dir: &std::path::Path) -> HashMap<String, String> {
    let path = dir.join("zen-sessions.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    if let Some(obj) = value.get("sessions").and_then(|s| s.as_object()) {
        for (seed, id) in obj {
            if let Some(id) = id.as_str() {
                if session_shape_ok(id) {
                    out.insert(seed.clone(), id.to_string());
                }
            }
        }
    }
    out
}

/// Persist the map atomically (tmp + rename), capped so it cannot grow without
/// bound across thousands of conversations.
pub fn save_session_map(dir: &std::path::Path, map: &HashMap<String, String>) {
    if map.len() > 2000 {
        return; // stop growing; existing conversations keep working
    }
    let path = dir.join("zen-sessions.json");
    let body = json!({
        "version": 1,
        "at": crate::store::now_ms(),
        "sessions": map,
    });
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_string_pretty(&body).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_rule_matches_and_rejects() {
        assert!(id_is_free("deepseek-v4.1-flash-free"));
        assert!(id_is_free("mimo-v2.6-flash-free"));
        assert!(id_is_free("beam-free"));
        assert!(id_is_free("nemotron-3.5-lightning-free"));
        assert!(id_is_free("space-bunny-free")); // ALWAYS_FREE anyway
        assert!(id_is_free("union-alpha")); // ALWAYS_FREE
        assert!(!id_is_free("big-pickle")); // free-shaped? no: no "free" token
        assert!(!id_is_free("claude-opus-5"));
        assert!(!id_is_free("gpt-5.4"));
        assert!(!id_is_free("freedom")); // "free" must be a standalone token
        assert!(!id_is_free("deepseek-freefor")); // trailing must be -_. or EOL
    }

    #[test]
    fn big_pickle_is_not_free_lane_id() {
        // OFM's table lists big-pickle as a model but it lacks the -free tag;
        // the lane id itself comes from the listing and this filter.
        assert!(!id_is_free("big-pickle"));
    }

    #[test]
    fn minted_ids_have_the_canonical_shape() {
        let ms = 1_792_210_498_000u64;
        let s = mint_session_id(ms, 3);
        assert!(session_shape_ok(&s), "shape: {}", s);
        assert!(s.starts_with("ses_"));
        assert_eq!(s.len(), 4 + 12 + 14);
        let r = mint_request_id(ms);
        assert!(r.starts_with("msg_"));
        assert_eq!(r.len(), 4 + 12 + 14);
        assert!(!session_shape_ok("sha_0123456789abcdef0123456789ab"));
        assert!(!session_shape_ok("ses_short"));
        // hex segment encodes the negated timestamp — deterministic across calls
        assert_eq!(&s[4..16], &mint_session_id(ms, 3)[4..16]);
    }

    #[test]
    fn fingerprint_fills_missing_slots_with_decoys() {
        let mut body = json!({ "messages": [] });
        let map = apply_fingerprint(&mut body);
        let names: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(tool_name)
            .collect();
        for want in ["bash", "glob", "grep", "read"] {
            assert!(names.contains(&want), "{want} missing: {:?}", names);
        }
        assert!(map.is_empty()); // nothing renamed
        // bash/read carry a non-empty schema
        for tool in body["tools"].as_array().unwrap() {
            if matches!(tool_name(tool), "bash" | "read") {
                let props = tool["function"]["parameters"]["properties"]
                    .as_object()
                    .unwrap();
                assert!(!props.is_empty());
            }
        }
        assert_eq!(body["tool_choice"], json!("none"));
    }

    #[test]
    fn fingerprint_promotes_pwsh_into_bash_slot() {
        let mut body = json!({
            "messages": [],
            "tools": [
                { "type": "function", "function": { "name": "pwsh", "parameters": { "type":"object", "properties": { "command": {"type":"string"} } } } },
                { "type": "function", "function": { "name": "Read", "parameters": { "type":"object", "properties": { "path": {"type":"string"} } } } }
            ]
        });
        let map = apply_fingerprint(&mut body);
        let names: Vec<String> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| tool_name(t).to_string())
            .collect();
        assert!(names.contains(&"bash".to_string()), "pwsh promoted: {:?}", names);
        assert_eq!(map.get("bash").map(String::as_str), Some("pwsh"));
        assert_eq!(map.get("read").map(String::as_str), Some("Read"));
        // original Read was canonicalised to lowercase `read`
        assert!(names.contains(&"read".to_string()));
        assert!(!names.contains(&"pwsh".to_string()));
        // caller fielded tools → tool_choice untouched
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn fingerprint_stamps_empty_schema_on_bash() {
        let mut body = json!({
            "messages": [],
            "tools": [
                { "type": "function", "function": { "name": "bash", "parameters": { "type": "object", "properties": {} } } },
                { "type": "function", "function": { "name": "read", "parameters": { "type": "object", "properties": { "p": {"type":"string"} } } } }
            ]
        });
        apply_fingerprint(&mut body);
        let bash = body["tools"].as_array().unwrap().iter().find(|t| tool_name(t) == "bash").unwrap();
        assert!(!bash["function"]["parameters"]["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn catalog_filters_and_attaches_caps() {
        let ids: Vec<String> = [
            "big-pickle",
            "mimo-v2.6-flash-free",
            "nemotron-3.5-lightning-free",
            "deepseek-v4.1-flash-free",
            "claude-opus-5",
            "gpt-5.4",
            "space-bunny-free",
            "union-alpha",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cat = build_zen_catalog(&ids);
        let got: Vec<&str> = cat.iter().map(|e| e.upstream_id.as_str()).collect();
        assert_eq!(got, vec![
            "mimo-v2.6-flash-free",
            "nemotron-3.5-lightning-free",
            "deepseek-v4.1-flash-free",
            "space-bunny-free",
            "union-alpha",
        ]);
        let mimo = cat.iter().find(|e| e.upstream_id == "mimo-v2.6-flash-free").unwrap();
        assert!(mimo.vision);
        assert_eq!(mimo.context, 1_048_576);
        let nemotron = cat.iter().find(|e| e.upstream_id == "nemotron-3.5-lightning-free").unwrap();
        assert!(!nemotron.vision);
        assert_eq!(nemotron.name, "Nemotron 3.5 Lightning");
        assert_eq!(mimo.source, "zen");
    }

    #[test]
    fn conversation_seed_is_stable_and_first_message_scoped() {
        let a = json!({ "messages": [ { "role": "user", "content": "hello" }, { "role":"user", "content": "second" } ] });
        let b = json!({ "messages": [ { "role": "user", "content": "hello" } ] });
        let c = json!({ "messages": [ { "role": "user", "content": "different" } ] });
        assert_eq!(conversation_seed(&a), conversation_seed(&b));
        assert_ne!(conversation_seed(&a), conversation_seed(&c));
    }

    #[test]
    fn listing_parser_handles_shapes() {
        let v = json!({ "data": [ { "id": "a" }, { "id": "b" } ] });
        assert_eq!(parse_listing(&v), vec!["a", "b"]);
        let v2 = json!([ "x", "y" ]);
        assert_eq!(parse_listing(&v2), vec!["x", "y"]);
        let v3 = json!({ "models": [ { "id": "z" } ] });
        assert_eq!(parse_listing(&v3), vec!["z"]);
    }
}
