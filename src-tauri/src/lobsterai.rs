//! LobsterAI (有道) —— 原生渠道。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 登录（OAuth loopback + code 交换）：打开 `{portalBase}/portal#/login
//!   ?source=electron&redirect_uri=http://127.0.0.1:{port}/auth/callback&state={state}`，
//!   回调拿 code → POST {apiBase}/api/auth/exchange
//!   body {authCode, firstKeyfrom, latestKeyfrom, uuid, version} → 凭据。
//! - 刷新：POST {apiBase}/api/auth/refresh。
//! - chat：POST {apiBase}/api/proxy/v1/chat/completions，标准 OpenAI 协议；
//!   头 Authorization Bearer + X-LobsterAI-Client-Capabilities/Version。
//! - 目录：GET {apiBase}/api/models/available。
//! - 超额判定：402「积分不足」类文案（insufficient credit / 免费额度已用完 / …）。

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::store::{ModelEntry, TokenCreds};

pub const LOBSTER_SOURCE: &str = "lobsterai";
pub const API_BASE: &str = "https://lobsterai-server.youdao.com";
pub const PORTAL_BASE: &str = "https://lobsterai.youdao.com";
pub const EXCHANGE_PATH: &str = "/api/auth/exchange";
pub const REFRESH_PATH: &str = "/api/auth/refresh";
pub const MODELS_PATH: &str = "/api/models/available";
pub const CHAT_PATH: &str = "/api/proxy/v1/chat/completions";
pub const CALLBACK_PATH: &str = "/auth/callback";
/// 登录时 exchange 需要的 firstKeyfrom（客户端渠道标识）。有道文档未公开具体取值，
/// 这里用与 authorize 的 `source=electron` 一致的占位值；若服务端对该字段做白名单校验，
/// 需替换为官方桌面客户端实际使用的值。
pub const FIRST_KEYFROM: &str = "credit-ferry-electron";
pub const CLIENT_CAPABILITIES: &str = "kimi-k3-agentic-v1,thinking-level-control-v1";
pub const FALLBACK_CLIENT_VERSION: &str = "2026.9.4";
pub const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 30_000;

/// 账号额度耗尽的文案（LobsterAI errors.ts 36118-36148 的 hard-credit 集）。
const HARD_CREDIT_MARKERS: &[&str] = &[
    "insufficient credit", "quota exceeded", "freecreditsused", "free quota",
    "upgrade your plan", "积分不足", "额度不足", "余额不足", "积分用完",
    "额度用尽", "没有积分", "积分耗尽", "额度已用完",
];

pub fn is_hard_credit_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    HARD_CREDIT_MARKERS.iter().any(|m| lower.contains(&m.to_lowercase()))
}

/// exchange 响应 → 凭据。`expiresAt` 优先 expiresIn×1000，回退 JWT exp。
pub fn parse_token_payload(data: &Value) -> TokenCreds {
    let user = data.get("user").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let get_str = |src: &serde_json::Map<String, Value>, k: &str| {
        src.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let access_token = get_str(data.as_object().unwrap(), "accessToken");
    let expires_in = data.get("expiresIn").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let expires_at = if expires_in > 0.0 {
        crate::store::now_ms() + (expires_in as i64) * 1000
    } else {
        crate::store::jwt_exp_ms(&access_token).unwrap_or(0)
    };
    let uid = {
        let u = get_str(&user, "id");
        if u.is_empty() { get_str(&user, "yid") } else { u }
    };
    TokenCreds {
        access_token,
        refresh_token: get_str(data.as_object().unwrap(), "refreshToken"),
        expires_at,
        uid,
        nickname: get_str(&user, "nickname"),
        ..Default::default()
    }
}

/// Percent-encode a string for use inside a query parameter value.
pub fn pct(input: &str) -> String {
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

/// Build the browser login URL (OAuth loopback). The provider redirects back to
/// `http://127.0.0.1:{port}/auth/callback?code=…` after the user authorises.
pub fn login_url(port: u16, state: &str) -> String {
    let redirect = pct(&format!("http://127.0.0.1:{}{}", port, CALLBACK_PATH));
    format!(
        "{}#/login?source=electron&redirect_uri={}&state={}",
        PORTAL_BASE, redirect, state
    )
}

/// 目录行 → ModelEntry。
pub fn lobster_entry_of(row: &Value) -> ModelEntry {
    let id = row.get("id").or_else(|| row.get("model")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let name = row.get("name").or_else(|| row.get("displayName")).and_then(|v| v.as_str())
        .filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
    let context = row.get("contextLength").or_else(|| row.get("context_window")).and_then(|v| v.as_u64()).unwrap_or(131_072);
    let max_output = row.get("maxOutputTokens").or_else(|| row.get("max_tokens")).and_then(|v| v.as_u64()).unwrap_or(32_768);
    ModelEntry {
        upstream_id: id,
        name,
        source: LOBSTER_SOURCE.to_string(),
        vision: false,
        context,
        max_output,
        multiplier: None,
        billing_status: None,
    }
}

pub fn build_lobster_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let e = lobster_entry_of(r);
        if e.upstream_id.is_empty() || !seen.insert(e.upstream_id.clone()) {
            continue;
        }
        out.push(e);
    }
    out
}

pub struct LobsterClient {
    http: Client,
    listing: Client,
}

impl LobsterClient {
    pub fn new() -> Self {
        let http = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        let listing = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        Self { http, listing }
    }

    /// 登录：把回调拿到的 code 换成凭据。
    pub async fn exchange(&self, code: &str, uuid: &str, first_keyfrom: &str) -> Result<TokenCreds, String> {
        let body = json!({
            "authCode": code, "firstKeyfrom": first_keyfrom,
            "latestKeyfrom": crate::store::now_ms().to_string(),
            "uuid": uuid, "version": FALLBACK_CLIENT_VERSION,
        });
        let resp = self.http.post(format!("{}{}", API_BASE, EXCHANGE_PATH))
            .header("Accept", "application/json").header("Content-Type", "application/json")
            .json(&body).send().await.map_err(|e| format!("lobsterai exchange: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("lobsterai exchange body: {}", e))?;
        if !status.is_success() {
            let m = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("exchange HTTP {}: {}", status.as_u16(), m));
        }
        let data = value.get("data").unwrap_or(&value);
        let creds = parse_token_payload(data);
        if creds.access_token.is_empty() {
            return Err("exchange 响应缺少 accessToken".into());
        }
        Ok(creds)
    }

    /// 刷新（RT 单次消费，调用方须持全局 refresh_gate）。
    pub async fn refresh(&self, creds: &mut TokenCreds) -> Result<(), String> {
        if creds.refresh_token.is_empty() {
            return Err("lobsterai 无 refresh_token，请重新登录".into());
        }
        let resp = self.http.post(format!("{}{}", API_BASE, REFRESH_PATH))
            .header("Accept", "application/json").header("Content-Type", "application/json")
            .json(&json!({ "refreshToken": creds.refresh_token }))
            .send().await.map_err(|e| format!("lobsterai refresh: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("lobsterai refresh body: {}", e))?;
        if !status.is_success() {
            let m = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if status.as_u16() == 401 || is_hard_credit_error(m) {
                return Err(format!("REFRESH_TERMINAL:{}", m));
            }
            return Err(format!("refresh HTTP {}: {}", status.as_u16(), m));
        }
        let data = value.get("data").unwrap_or(&value);
        let fresh = parse_token_payload(data);
        if !fresh.access_token.is_empty() {
            creds.access_token = fresh.access_token;
            creds.expires_at = fresh.expires_at;
        }
        if !fresh.refresh_token.is_empty() {
            creds.refresh_token = fresh.refresh_token;
        }
        Ok(())
    }

    /// 目录：GET /api/models/available。
    pub async fn catalog(&self, creds: &TokenCreds) -> Result<Vec<ModelEntry>, String> {
        let resp = self.listing.get(format!("{}{}", API_BASE, MODELS_PATH))
            .header("Authorization", &format!("Bearer {}", creds.access_token))
            .header("Accept", "application/json")
            .send().await.map_err(|e| format!("lobsterai models: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("lobsterai models body: {}", e))?;
        if !status.is_success() {
            let m = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("models HTTP {}: {}", status.as_u16(), m));
        }
        let data = value.get("data").unwrap_or(&value);
        let rows: Vec<Value> = data.as_array().cloned()
            .or_else(|| data.get("models").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();
        Ok(build_lobster_catalog(&rows))
    }

    /// chat：标准 OpenAI 协议透传（流式由客户端决定）。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value) -> Result<reqwest::Response, String> {
        self.http.post(format!("{}{}", API_BASE, CHAT_PATH))
            .header("Authorization", &format!("Bearer {}", creds.access_token))
            .header("X-LobsterAI-Client-Capabilities", CLIENT_CAPABILITIES)
            .header("X-LobsterAI-Client-Version", FALLBACK_CLIENT_VERSION)
            .json(body)
            .send().await.map_err(|e| format!("lobsterai chat: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exchange_payload() {
        let creds = parse_token_payload(&json!({
            "accessToken": "at", "refreshToken": "rt", "expiresIn": 3600,
            "user": { "id": "u1", "nickname": "小明", "userId": "au1" }
        }));
        assert_eq!(creds.access_token, "at");
        assert_eq!(creds.refresh_token, "rt");
        assert_eq!(creds.uid, "u1");
        assert_eq!(creds.nickname, "小明");
        assert!(creds.expires_at > crate::store::now_ms());
    }

    #[test]
    fn hard_credit_detected() {
        assert!(is_hard_credit_error("免费额度已用完，请升级套餐"));
        assert!(is_hard_credit_error("Insufficient credit"));
        assert!(!is_hard_credit_error("临时网络错误"));
    }

    #[test]
    fn catalog_deduped() {
        let rows = vec![
            json!({ "id": "kimi-k3", "name": "Kimi K3", "contextLength": 200_000 }),
            json!({ "id": "kimi-k3", "name": "dup" }),
            json!({ "model": "deepseek-v4", "displayName": "DeepSeek V4" }),
        ];
        let cat = build_lobster_catalog(&rows);
        assert_eq!(cat.len(), 2);
        assert_eq!(cat[0].upstream_id, "kimi-k3");
        assert_eq!(cat[0].context, 200_000);
        assert_eq!(cat[1].name, "DeepSeek V4");
    }
}