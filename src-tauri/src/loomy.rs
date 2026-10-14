//! Loomy (科大讯飞) upstream adapter.
//!
//! Wire facts (measured by the loomy2api reference implementation):
//! - Inference base is OpenAI-compatible: `https://loomyad.xunfei.cn/api/v1`
//!   (`POST /chat/completions`, `GET /models`, `GET /points/records`).
//! - The gateway needs `Authorization: Bearer <session>` AND `token: <session>`
//!   (the client uses session auth; either is accepted, both are sent).
//! - `traceparent` is REQUIRED — without it the upstream hangs until timeout.
//! - `loomy-version` is presence-checked only.
//! - Responses are OpenAI-shaped; `usage.points_consumed` reports the credits
//!   spent by the request (also `completion_tokens_details.reasoning_tokens`).
//! - Sessions last 14 days; the desktop client keeps them in
//!   `C:\Users\Public\Loomy\<sha256(user)[:12]>\userData\auth-session.json`
//!   (fallback root `%APPDATA%\Loomy`), which this adapter imports read-only.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::{LoomySession, ModelEntry};

pub const LOOMY_BASE: &str = "https://loomyad.xunfei.cn/api/v1";
pub const LOOMY_CLIENT_ROOT: &str = r"C:\Users\Public\Loomy";
pub const LOOMY_VERSION: &str = "0.9.38";
const SESSION_TTL_MS: i64 = 14 * 24 * 3600 * 1000;

pub struct LoomyClient {
    pub base: String,
    http: Client,
}

/// Headers the model gateway requires.
pub fn loomy_headers(session: &str, version: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, ACCEPT, CONTENT_TYPE};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", session)) {
        headers.insert(AUTHORIZATION, v);
    }
    if let Ok(name) = HeaderName::from_bytes(b"token") {
        if let Ok(v) = HeaderValue::from_str(session) {
            headers.insert(name, v);
        }
    }
    // traceparent: 00-<32 hex trace>-<16 hex span>-01 — absence hangs the request.
    let traceparent = format!("00-{}-{}-01", crate::store::new_hex(16), &crate::store::new_hex(8)[..16]);
    if let Ok(name) = HeaderName::from_bytes(b"traceparent") {
        if let Ok(v) = HeaderValue::from_str(&traceparent) {
            headers.insert(name, v);
        }
    }
    if !version.is_empty() {
        if let Ok(name) = HeaderName::from_bytes(b"loomy-version") {
            if let Ok(v) = HeaderValue::from_str(version) {
                headers.insert(name, v);
            }
        }
    }
    headers
}

impl LoomyClient {
    pub fn new(base: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self { base: base.trim_end_matches('/').to_string(), http }
    }

    /// GET /models → normalized entries. Empty vec on failure.
    pub async fn models(&self, session: &str) -> Result<Vec<ModelEntry>, String> {
        let resp = self
            .http
            .get(format!("{}/models", self.base))
            .headers(loomy_headers(session, LOOMY_VERSION))
            .send()
            .await
            .map_err(|e| format!("loomy /models: {}", e))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| format!("loomy /models body: {}", e))?;
        if !status.is_success() {
            return Err(format!("loomy /models HTTP {}", status));
        }
        let mut out = Vec::new();
        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
            for m in data {
                let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&id)
                    .to_string();
                let vision = m
                    .get("modalities")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().any(|x| x.as_str() == Some("image")))
                    .unwrap_or(false);
                out.push(ModelEntry {
                    upstream_id: id,
                    name,
                    source: "loomy".into(),
                    vision,
                    context: m.get("context_length").and_then(|v| v.as_u64()).unwrap_or(0),
                    max_output: 0,
                    multiplier: m.get("multiplier").and_then(|v| v.as_f64()),
                    billing_status: None,
                });
            }
        }
        Ok(out)
    }

    /// GET /points/records → {balance, daily_balance, available}.
    pub async fn quota(&self, session: &str) -> Result<Value, String> {
        let url = format!(
            "{}/points/records?record_type=all&page_no=1&page_size=1",
            self.base
        );
        let resp = self
            .http
            .get(&url)
            .headers(loomy_headers(session, LOOMY_VERSION))
            .send()
            .await
            .map_err(|e| format!("loomy /points/records: {}", e))?;
        let body: Value = resp.json().await.map_err(|e| format!("loomy points body: {}", e))?;
        let data = body.get("data").cloned().unwrap_or(json!({}));
        Ok(json!({
            "balance": data.get("balance"),
            "daily_balance": data.get("dailyBalance"),
            "available": data.get("availableBalance"),
        }))
    }

    /// Forward a chat/completions request; returns the raw upstream response.
    pub async fn chat(
        &self,
        session: &str,
        body: &Value,
    ) -> Result<reqwest::Response, String> {
        self.http
            .post(format!("{}/chat/completions", self.base))
            .headers(loomy_headers(session, LOOMY_VERSION))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("loomy chat: {}", e))
    }
}

/// Import sessions from an installed Loomy desktop client (read-only scan).
pub fn import_client_sessions() -> Vec<LoomySession> {
    let mut out = Vec::new();
    let mut roots: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(LOOMY_CLIENT_ROOT)];
    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(std::path::Path::new(&appdata).join("Loomy"));
    }
    for root in roots {
        let pattern = root.join("*").join("userData").join("auth-session.json");
        let entries = match glob_simple(&pattern) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for path in entries {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let session = value.get("session").and_then(|v| v.as_str()).unwrap_or("");
            if session.is_empty() {
                continue;
            }
            let updated = value.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            let phone = value.get("phone").and_then(|v| v.as_str()).unwrap_or("").to_string();
            out.push(LoomySession {
                name: format!("desktop-{}", phone.chars().rev().take(4).collect::<String>()),
                session: session.to_string(),
                userid: value.get("userid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                phone,
                expires_at_ms: if updated > 0 { updated + SESSION_TTL_MS } else { 0 },
                source: "client".into(),
            });
        }
    }
    out
}

/// Minimal single-level glob for `<root>/*/userData/auth-session.json`.
fn glob_simple(pattern: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let comps: Vec<String> = pattern
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let star = comps.iter().position(|c| c == "*");
    let (prefix, star, suffix) = match star {
        Some(i) => (
            std::path::PathBuf::from(comps[..i].join(std::path::MAIN_SEPARATOR_STR)),
            i,
            comps[i + 1..].to_vec(),
        ),
        None => return Ok(out),
    };
    let _ = star;
    if let Ok(read) = std::fs::read_dir(&prefix) {
        for entry in read.flatten() {
            let mut candidate = entry.path();
            for part in &suffix {
                candidate = candidate.join(part);
            }
            if candidate.is_file() {
                out.push(candidate);
            }
        }
    }
    Ok(out)
}
