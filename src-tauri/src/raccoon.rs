//! Xiaohuanxiong (商汤小浣熊) upstream adapter.
//!
//! Wire facts (from the dsh-connect-sensenova-token-plan reference plugin):
//! - Gateway `https://xiaohuanxiong.com`, three prefixes:
//!   `/api/web/auth/v1` (QR login + refresh), `/api/web/llm/v2`
//!   (model_catalog + OpenAI-compatible chat/completions),
//!   `/api/web/points/v1` (balance).
//! - QR login: the client generates the 32-hex scan code LOCALLY; the phone
//!   opens `https://xiaohuanxiong.com/login/mp?code=<code>&appname=商汤小浣熊官网`;
//!   poll `POST /auth/v1/login_with_qrcode_code {qrcode_code}` every 2 s.
//!   Status ladder: pending → logging → success (access_token + refresh_token,
//!   both JWTs) or canceled.
//! - `POST /auth/v1/refresh {refresh_token}` rotates BOTH tokens (the refresh
//!   token is single-use — always re-store what it returns).
//! - Every request carries `Authorization: Bearer <access JWT>`,
//!   `X-Org-Code` (empty for personal accounts, header always present) and
//!   `X-Raccoon-Language: zh`.
//! - Envelope: `{code, message, data}` where `code === 0` is success.
//! - Catalogue: `categories[type=chat].models[]` — machine id is `model_name`
//!   (`sn-…`), vision lives in `tags`, credits multiplier in
//!   `billing_multiplier` / `billing_effective_multiplier` (promotion-aware).
//! - Balance: `/points/v1/balance` → available_points + daily/reward/monthly/topup.
//! - Chat responses are OpenAI-shaped; usage carries tokens but NOT points —
//!   per-request credit spend is not reported by this gateway.

use std::time::Duration;

use base64::Engine as _;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::store::{ModelEntry, RaccoonCreds};

pub const RACCOON_BASE: &str = "https://xiaohuanxiong.com";
const AUTH_PREFIX: &str = "/api/web/auth/v1";
const LLM_PREFIX: &str = "/api/web/llm/v2";
const POINTS_PREFIX: &str = "/api/web/points/v1";
const QR_POLL_MS: u64 = 2000;
const LOGIN_TIMEOUT_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Debug, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

pub struct RaccoonClient {
    http: Client,
}

/// Headers every gateway request carries.
pub fn raccoon_headers(access_token: &str, org_code: &str, device_id: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", access_token)) {
        headers.insert(AUTHORIZATION, v);
    }
    for (name, value) in [
        ("x-org-code", org_code.to_string()),
        ("x-raccoon-language", "zh".to_string()),
        ("x-client-device-id", device_id.to_string()),
    ] {
        if value.is_empty() && name != "x-org-code" {
            continue;
        }
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
            headers.insert(n, v);
        }
    }
    headers
}

#[derive(Clone, Debug)]
pub enum QrStatus {
    Pending,
    Logging { expired_at: String },
    Success { access: String, refresh: String, expires_at_ms: i64, nickname: String },
    Canceled,
    TimedOut,
}

impl RaccoonClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// The URL the phone opens after scanning.
    pub fn qr_login_url(code: &str) -> String {
        format!(
            "{}/login/mp?code={}&appname={}",
            RACCOON_BASE,
            urlencode(code),
            urlencode("商汤小浣熊官网")
        )
    }

    /// One poll of the QR login. Any anomaly degrades to Pending: a transient
    /// network error must never read as success (empty token) or canceled.
    pub async fn qr_poll(&self, code: &str) -> QrStatus {
        let resp = match self
            .http
            .post(format!("{}{}/login_with_qrcode_code", RACCOON_BASE, AUTH_PREFIX))
            .json(&json!({ "qrcode_code": code }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return QrStatus::Pending,
        };
        let envelope: Envelope = match resp.json().await {
            Ok(e) => e,
            Err(_) => return QrStatus::Pending,
        };
        let data = match envelope.data {
            Some(d) if envelope.code == 0 => d,
            _ => return QrStatus::Pending,
        };
        let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
        match status {
            "canceled" => QrStatus::Canceled,
            "logging" => QrStatus::Logging {
                expired_at: data.get("expired_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            },
            "success" => {
                let access = data.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if access.is_empty() {
                    return QrStatus::Pending;
                }
                let refresh = data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let expires = crate::store::jwt_exp_ms(&access).unwrap_or(0);
                let nickname = jwt_claim(&access, &["nickname", "nick_name", "display_name", "preferred_username", "name"]);
                QrStatus::Success { access, refresh, expires_at_ms: expires, nickname }
            }
            _ => QrStatus::Pending,
        }
    }

    /// Rotate the credential pair; the refresh token is single-use.
    pub async fn refresh(&self, refresh_token: &str) -> Result<(String, String, i64), String> {
        let resp = self
            .http
            .post(format!("{}{}/refresh", RACCOON_BASE, AUTH_PREFIX))
            .json(&json!({ "refresh_token": refresh_token }))
            .send()
            .await
            .map_err(|e| format!("raccoon refresh: {}", e))?;
        let status = resp.status();
        let envelope: Envelope = resp.json().await.map_err(|e| format!("raccoon refresh body: {}", e))?;
        if envelope.code != 0 || envelope.data.is_none() {
            return Err(format!(
                "raccoon refresh refused (HTTP {} code {} {})",
                status.as_u16(),
                envelope.code,
                envelope.message
            ));
        }
        let data = envelope.data.unwrap();
        let access = data.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let refresh = data.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if access.is_empty() {
            return Err("raccoon refresh carried no access token".into());
        }
        let exp = crate::store::jwt_exp_ms(&access).unwrap_or(0);
        Ok((access, refresh, exp))
    }

    /// GET /llm/v2/model_catalog → normalized chat models.
    pub async fn catalog(&self, creds: &RaccoonCreds) -> Result<Vec<ModelEntry>, String> {
        let resp = self
            .http
            .get(format!("{}{}/model_catalog", RACCOON_BASE, LLM_PREFIX))
            .headers(raccoon_headers(&creds.access_token, &creds.org_code, &creds.device_id))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("raccoon catalog: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("raccoon catalog HTTP {}", resp.status()));
        }
        let envelope: Envelope = resp.json().await.map_err(|e| format!("raccoon catalog body: {}", e))?;
        if envelope.code != 0 || envelope.data.is_none() {
            return Err(format!("raccoon catalog code={} {}", envelope.code, envelope.message));
        }
        let mut out = Vec::new();
        let categories = envelope
            .data
            .as_ref()
            .and_then(|d| d.get("categories"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        for category in categories {
            if category.get("type").and_then(|v| v.as_str()) != Some("chat") {
                continue;
            }
            let models = category.get("models").and_then(|m| m.as_array()).cloned().unwrap_or_default();
            for raw in models {
                if raw.get("visible") == Some(&Value::Bool(false)) {
                    continue;
                }
                let id = first_str(&raw, &["model_name", "id", "name"]);
                if id.is_empty() {
                    continue;
                }
                let name = first_str(&raw, &["name"]);
                let name = if name.is_empty() { id.clone() } else { name };
                let tags = raw.get("tags").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                let vision = tags
                    .iter()
                    .any(|t| matches!(t.as_str(), Some("vision") | Some("image") | Some("image-understanding")));
                let params = raw.get("params").cloned().unwrap_or(json!({}));
                let context = params
                    .get("context_window")
                    .or_else(|| raw.get("context_window"))
                    .or_else(|| raw.get("context_length"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let max_output = params
                    .get("max_tokens")
                    .or_else(|| params.get("max_output_tokens"))
                    .or_else(|| raw.get("max_output_tokens"))
                    .or_else(|| raw.get("max_output_length"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let base_multiplier = pick_f64(&raw, &["billing_multiplier", "multiplier"]);
                let status = raw.get("billing_status").and_then(|v| v.as_str()).map(|s| s.to_string());
                let effective = raw.get("billing_effective_multiplier").and_then(|v| v.as_f64());
                let promoted = matches!(status.as_deref(), Some("discount") | Some("limited_free")) && effective.is_some();
                out.push(ModelEntry {
                    upstream_id: id,
                    name,
                    source: "raccoon".into(),
                    vision,
                    context,
                    max_output,
                    multiplier: if promoted { effective } else { base_multiplier },
                    billing_status: status,
        channel: None,
                });
            }
            if !out.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// GET /points/v1/balance → {total, daily?, reward?, monthly?, topup?}.
    pub async fn balance(&self, creds: &RaccoonCreds) -> Result<Value, String> {
        let resp = self
            .http
            .get(format!("{}{}/balance", RACCOON_BASE, POINTS_PREFIX))
            .headers(raccoon_headers(&creds.access_token, &creds.org_code, &creds.device_id))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("raccoon balance: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("raccoon balance HTTP {}", resp.status()));
        }
        let envelope: Envelope = resp.json().await.map_err(|e| format!("raccoon balance body: {}", e))?;
        if envelope.code != 0 || envelope.data.is_none() {
            return Err(format!("raccoon balance code={} {}", envelope.code, envelope.message));
        }
        let data = envelope.data.unwrap();
        let total = first_num(&data, &["available_points", "balance", "available", "amount"]);
        Ok(json!({
            "total": total,
            "daily": first_num(&data, &["daily_points"]),
            "reward": first_num(&data, &["reward_points"]),
            "monthly": first_num(&data, &["monthly_points"]),
            "topup": first_num(&data, &["topup_points"]),
        }))
    }

    /// Forward a chat/completions request (OpenAI-compatible).
    pub async fn chat(&self, creds: &RaccoonCreds, body: &Value) -> Result<reqwest::Response, String> {
        self.http
            .post(format!("{}{}/chat/completions", RACCOON_BASE, LLM_PREFIX))
            .headers(raccoon_headers(&creds.access_token, &creds.org_code, &creds.device_id))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("raccoon chat: {}", e))
    }
}

/// Keep a live token fresh: refresh when inside the expiry window.
pub async fn ensure_fresh(client: &RaccoonClient, creds: &mut RaccoonCreds, window_ms: i64) -> bool {
    let now = crate::store::now_ms();
    if creds.access_token.is_empty() {
        return false;
    }
    let expires = if creds.expires_at_ms > 0 {
        creds.expires_at_ms
    } else {
        crate::store::jwt_exp_ms(&creds.access_token).unwrap_or(0)
    };
    if expires == 0 || now + window_ms < expires {
        return true;
    }
    match client.refresh(&creds.refresh_token).await {
        Ok((access, refresh, exp)) => {
            creds.access_token = access;
            creds.refresh_token = refresh;
            creds.expires_at_ms = exp;
            true
        }
        Err(_) => false,
    }
}

fn first_str(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn pick_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(n) = value.get(key).and_then(|v| v.as_f64()) {
            return Some(n);
        }
    }
    None
}

fn first_num(value: &Value, keys: &[&str]) -> Option<f64> {
    pick_f64(value, keys)
}

fn jwt_claim(token: &str, keys: &[&str]) -> String {
    let payload = match token.split('.').nth(1) {
        Some(p) => p,
        None => return String::new(),
    };
    let decoded = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let value: Value = match serde_json::from_slice(&decoded) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    for key in keys {
        if let Some(s) = value.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Percent-encode for the QR URL query.
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

pub const QR_POLL_INTERVAL_MS: u64 = QR_POLL_MS;
pub const QR_LOGIN_TIMEOUT_MS: i64 = LOGIN_TIMEOUT_MS;
