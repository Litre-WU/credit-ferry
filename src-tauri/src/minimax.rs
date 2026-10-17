//! MiniMax Code —— 原生渠道。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 登录（device code flow）：POST {accountHost}/oauth2/device/code
//!   （clientId=mcode-public, audience=agent-backend, scope=agent.default）
//!   → device_code + verification_uri，用户在浏览器授权 → 轮询
//!   POST {accountHost}/oauth2/token 换 access_token（`mmoat_` 前缀）。
//! - chat：POST {apiHost}/mavis/api/v1/llm/v1/messages，**自定义协议**
//!   （非标准 OpenAI/Anthropic）：body {model, messages, system, tools,
//!   temperature, maxTokens, stop, effort…}；头 Bearer + Accept: text/event-stream。
//! - 目录：GET {apiHost}/mavis/api/v1/models。
//! - 签到/额度（保活参考）：/minimax-cloud/api/v1/signin/{status,claim}、
//!   credit/details；错误信封 base_resp.status_code ≠ 0 即业务失败。

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
    let context = row.get("context_window").or_else(|| row.get("contextWindow")).and_then(|v| v.as_u64()).unwrap_or(1_000_000);
    let max_output = row.get("max_tokens").or_else(|| row.get("maxOutputTokens")).and_then(|v| v.as_u64()).unwrap_or(32_768);
    ModelEntry {
        upstream_id: id,
        name,
        source: MINIMAX_SOURCE.to_string(),
        vision: false,
        context,
        max_output,
        multiplier: None,
        billing_status: None,
    }
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
        let rows: Vec<Value> = data.as_array().cloned()
            .or_else(|| data.get("models").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();
        Ok(build_minimax_catalog(&rows))
    }

    /// chat：自定义协议 → 标准 OpenAI 形态请求体，发往 /mavis/api/v1/llm/v1/messages。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value) -> Result<reqwest::Response, String> {
        let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let messages = body.get("messages").cloned().unwrap_or(json!([]));
        let system = body.get("system").cloned().unwrap_or(Value::Null);
        let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(32_768);
        let temperature = body.get("temperature").cloned().unwrap_or(Value::Null);
        let tools = body.get("tools").cloned().unwrap_or(Value::Null);
        let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
        let mut payload = json!({
            "model": model, "messages": messages, "maxTokens": max_tokens,
        });
        if !system.is_null() {
            payload["system"] = system;
        }
        if !temperature.is_null() {
            payload["temperature"] = temperature;
        }
        if !tools.is_null() {
            payload["tools"] = tools;
        }
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
}