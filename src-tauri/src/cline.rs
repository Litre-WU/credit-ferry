//! Cline —— 原生渠道（OpenAI 兼容，设备码登录）。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 设备码登录（WorkOS device flow）：
//!   POST {workOsBase}/user_management/authorize/device（form: client_id）
//!   → {device_code, user_code, verification_uri(+complete), expires_in, interval}
//!   → POST /user_management/authenticate 轮询换 token。
//!   token 带 `workos:` 前缀（clineBearerValue 负责补）。
//! - chat：POST {apiBase}/api/v1/chat/completions，标准 OpenAI 协议；
//!   头 Authorization Bearer + 客户端身份（HTTP-Referer / X-Title /
//!   X-IS-MULTIROOT / X-CLIENT-TYPE: cline-sdk）。
//! - 目录：GET /api/v1/models；推荐位 /api/v1/ai/cline/recommended-models。
//! - 免费判定：id 后缀 `:free` 或前缀 `cline-free/`，展示名加「· 免费」。
//! - 超额判定：429 + 文案 daily free limit / free daily limit /
//!   daily limit reached（CLINE_DAILY_FREE_MARKERS 40986）。

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::{ModelEntry, TokenCreds};

pub const CLINE_SOURCE: &str = "cline";
pub const API_BASE: &str = "https://api.cline.bot";
pub const WORKOS_BASE: &str = "https://api.workos.com";
pub const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";
pub const DEVICE_AUTHORIZATION_PATH: &str = "/user_management/authorize/device";
pub const DEVICE_AUTHENTICATE_PATH: &str = "/user_management/authenticate";
pub const REFRESH_PATH: &str = "/api/v1/auth/refresh";
pub const CHAT_PATH: &str = "/api/v1/chat/completions";
pub const MODELS_PATH: &str = "/api/v1/models";
pub const TOKEN_PREFIX: &str = "workos:";
pub const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 60_000;
const FREE_ID_SUFFIX: &str = ":free";
const FREE_ID_PREFIX: &str = "cline-free/";
/// Cline 每日免费额度耗尽的文案（CLINE_DAILY_FREE_MARKERS）。
const DAILY_FREE_MARKERS: &[&str] = &["daily free limit", "free daily limit", "daily limit reached"];

pub fn is_daily_free_exhausted(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    DAILY_FREE_MARKERS.iter().any(|m| lower.contains(&m.to_lowercase()))
}

/// token 补 `workos:` 前缀（官方要求；已是前缀则不重复补）。
pub fn cline_bearer(access_token: &str) -> String {
    if access_token.starts_with(TOKEN_PREFIX) {
        access_token.to_string()
    } else {
        format!("{}{}", TOKEN_PREFIX, access_token)
    }
}

/// 免费模型判定：后缀 `:free` / 前缀 `cline-free/`。
pub fn is_free_model(id: &str) -> bool {
    id.ends_with(FREE_ID_SUFFIX) || id.starts_with(FREE_ID_PREFIX)
}

fn client_headers() -> Vec<(&'static str, String)> {
    vec![
        ("HTTP-Referer", "https://cline.bot".to_string()),
        ("X-Title", "Cline".to_string()),
        ("X-IS-MULTIROOT", "false".to_string()),
        ("X-CLIENT-TYPE", "cline-sdk".to_string()),
    ]
}

pub fn cline_entry_of(row: &Value) -> ModelEntry {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let base_name = row.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        .unwrap_or(&id).to_string();
    let is_free = row.get("isFree").and_then(|v| v.as_bool()).unwrap_or(false) || is_free_model(&id);
    let name = if is_free { format!("{} · 免费", base_name) } else { base_name };
    let context = row.get("contextWindow").or_else(|| row.get("context_length")).and_then(|v| v.as_u64()).unwrap_or(131_072);
    let max_output = row.get("maxOutputTokens").or_else(|| row.get("max_tokens")).and_then(|v| v.as_u64()).unwrap_or(32_768);
    ModelEntry {
        upstream_id: id,
        name,
        source: CLINE_SOURCE.to_string(),
        vision: false,
        context,
        max_output,
        multiplier: None,
        billing_status: if is_free { Some("免费".to_string()) } else { None },
    }
}

pub fn build_cline_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let e = cline_entry_of(r);
        if e.upstream_id.is_empty() || !seen.insert(e.upstream_id.clone()) {
            continue;
        }
        out.push(e);
    }
    out
}

pub struct ClineClient {
    http: Client,
    listing: Client,
}

impl ClineClient {
    pub fn new() -> Self {
        let http = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        let listing = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        Self { http, listing }
    }

    /// 设备码登录第 1 步：申请 device code。
    pub async fn start_device_auth(&self) -> Result<Value, String> {
        let resp = self.http
            .post(format!("{}{}", WORKOS_BASE, DEVICE_AUTHORIZATION_PATH))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("client_id={}", WORKOS_CLIENT_ID))
            .send().await.map_err(|e| format!("cline device auth: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("cline device auth body: {}", e))?;
        if !status.is_success() {
            let d = value.get("error_description").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("设备码授权失败（HTTP {}）{}", status.as_u16(), d));
        }
        Ok(json!({
            "deviceCode": value.get("device_code").and_then(|v| v.as_str()).unwrap_or(""),
            "userCode": value.get("user_code").and_then(|v| v.as_str()).unwrap_or(""),
            "verificationUri": value.get("verification_uri").and_then(|v| v.as_str()).unwrap_or(""),
            "verificationUriComplete": value.get("verification_uri_complete").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    /// 设备码登录第 2 步：轮询 authenticate 换 token。
    pub async fn poll_device_token(&self, device_code: &str) -> Result<TokenCreds, String> {
        let resp = self.http
            .post(format!("{}{}", WORKOS_BASE, DEVICE_AUTHENTICATE_PATH))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("client_id={}&device_code={}", WORKOS_CLIENT_ID, device_code))
            .send().await.map_err(|e| format!("cline authenticate: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("cline authenticate body: {}", e))?;
        if !status.is_success() {
            // 授权未完成是正常轮询态（authorization_pending）。
            return Err("等待用户完成授权".to_string());
        }
        let access_token = value.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err("响应缺少 access_token".into());
        }
        let expires_in = value.get("expires_in").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let expires_at = if expires_in > 0.0 {
            crate::store::now_ms() + (expires_in as i64) * 1000
        } else {
            crate::store::jwt_exp_ms(&access_token).unwrap_or(0)
        };
        Ok(TokenCreds {
            access_token,
            refresh_token: value.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            expires_at,
            nickname: value.get("email").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            extra: value.clone(),
            ..Default::default()
        })
    }

    /// 刷新（RT 单次消费，调用方持 refresh_gate）。
    pub async fn refresh(&self, creds: &mut TokenCreds) -> Result<(), String> {
        if creds.refresh_token.is_empty() {
            return Err("cline 无 refresh_token，请重新登录".into());
        }
        let resp = self.http.post(format!("{}{}", API_BASE, REFRESH_PATH))
            .header("Content-Type", "application/json")
            .json(&json!({ "refresh_token": creds.refresh_token }))
            .send().await.map_err(|e| format!("cline refresh: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("cline refresh body: {}", e))?;
        if !status.is_success() {
            return Err(format!("REFRESH_TERMINAL:refresh HTTP {}", status.as_u16()));
        }
        let data = value.get("data").unwrap_or(&value);
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

    /// 目录：GET /api/v1/models。
    pub async fn catalog(&self, creds: &TokenCreds) -> Result<Vec<ModelEntry>, String> {
        let mut req = self.listing.get(format!("{}{}", API_BASE, MODELS_PATH))
            .header("Authorization", &format!("Bearer {}", cline_bearer(&creds.access_token)))
            .header("Accept", "application/json");
        for (k, v) in client_headers() {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| format!("cline models: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("cline models body: {}", e))?;
        if !status.is_success() {
            return Err(format!("models HTTP {}", status.as_u16()));
        }
        let data = value.get("data").unwrap_or(&value);
        let rows: Vec<Value> = data.as_array().cloned()
            .or_else(|| data.get("models").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();
        Ok(build_cline_catalog(&rows))
    }

    /// chat：标准 OpenAI 协议。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value) -> Result<reqwest::Response, String> {
        let mut req = self.http.post(format!("{}{}", API_BASE, CHAT_PATH))
            .header("Authorization", &format!("Bearer {}", cline_bearer(&creds.access_token)))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        for (k, v) in client_headers() {
            req = req.header(k, v);
        }
        req.json(body).send().await.map_err(|e| format!("cline chat: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_gets_workos_prefix() {
        assert_eq!(cline_bearer("abc"), "workos:abc");
        assert_eq!(cline_bearer("workos:abc"), "workos:abc"); // 不重复补
    }

    #[test]
    fn free_model_detected() {
        assert!(is_free_model("kimi-k2:free"));
        assert!(is_free_model("cline-free/nemotron-3.5-super-vl"));
        assert!(!is_free_model("kimi-k2"));
    }

    #[test]
    fn daily_free_exhausted_detected() {
        assert!(is_daily_free_exhausted("Daily free limit reached"));
        assert!(!is_daily_free_exhausted("network error"));
    }

    #[test]
    fn catalog_marks_free() {
        let rows = vec![
            json!({ "id": "kimi-k2:free", "name": "Kimi K2" }),
            json!({ "id": "gpt-oss:20b", "name": "GPT OSS" }),
        ];
        let cat = build_cline_catalog(&rows);
        assert_eq!(cat.len(), 2);
        assert_eq!(cat[0].name, "Kimi K2 · 免费");
        assert_eq!(cat[0].billing_status.as_deref(), Some("免费"));
        assert_eq!(cat[1].name, "GPT OSS");
    }
}