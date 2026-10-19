//! CodeBuddy (腾讯 CodeBuddy 国内版) / WorkBuddy (国际版) —— 原生渠道。
//!
//! 协议规格全部来自 Our Free Model 的逆向（`business.mjs`），逐条核对行号：
//! - 登录四步（fetchAuthState → loopGetToken → getAccount）：
//!   POST {endpoint}/v2/plugin/auth/state?platform=ide  → {state, authUrl}
//!   浏览器打开 authUrl → 轮询 GET {endpoint}/v2/plugin/auth/token?state=
//!   → {accessToken, refreshToken, expiresAt/expiresIn, domain}
//!   再轮询 GET /v2/plugin/login/account?state= → {uid, nickname, …}
//!   （未就绪业务码：token=11217，account=12151；轮询 1s，超时 5min）
//! - 刷新：POST {endpoint}/v2/plugin/auth/token/refresh，头带
//!   Authorization: Bearer {access_token} + X-Refresh-Token + X-Auth-Refresh-Source: ide-main。
//!   **refresh_token 单次消费**（每次刷新轮换），故所有续期路径共用 server.rs 的
//!   refresh_gate 串行闸。
//! - chat：POST {endpoint}/v2/chat/completions，**强制流式**（非流式 400
//!   「Non-stream chat request is currently not supported」，实测），归因头
//!   X-Domain / X-Product-Code / X-Agent-Purpose / X-IDE-* / X-Product / UA。
//! - **workbuddy 会话硬校验（2026-10-19 逐头二分实测）**：chat 必须带
//!   X-Conversation-ID / X-Conversation-Request-ID / X-Conversation-Message-ID
//!   三个头（缺任一个 → 400 code=11128 request illegal，伪装成安全拦截），
//!   且 messages 首条必须是 system（否则 11128 first message is not system
//!   prompt）。三头用每请求新 v4 UUID 即可，网关不校验值来源；X-User-Id、
//!   trace 头、UA、X-Product 等其余归因头均可缺。CodeBuddy 网关无此校验
//!   （现状可证），故用 Product.needs_conversation 区分。
//! - 目录：GET {endpoint}/console/enterprises/personal/models（scoped 优先）
//!   + GET {endpoint}/v3/config（parseModelsFromConfig 合并 data.models/agents/promotions）。
//!
//! buddy 与 workbuddy 同一套协议，仅 endpoint / productCode / UA / domain 不同，
//! 用一个 Product 常量表区分（对齐 OFM 的 CODEBUDDY / WORKBUDDY）。

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::store::{BuddyCreds, ModelEntry};

/// 一路渠道的端点/身份常量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Product {
    pub id: &'static str,
    pub display: &'static str,
    pub endpoint: &'static str,
    pub api_domain: &'static str,
    pub product_code: &'static str,
    pub platform: &'static str,
    pub user_agent: &'static str,
    pub client_version: &'static str,
    /// WorkBuddy 网关强校验三会话头 + system 首条（缺 → 11128）；CodeBuddy 不校验。
    pub needs_conversation: bool,
}

pub const CODEBUDDY: Product = Product {
    id: "buddy",
    display: "CodeBuddy",
    endpoint: "https://copilot.tencent.com",
    api_domain: "copilot.tencent.com",
    product_code: "codebuddy",
    platform: "ide",
    user_agent: "CodeBuddyIDE/1.106.1",
    client_version: "1.106.1",
    needs_conversation: false,
};

pub const WORKBUDDY: Product = Product {
    id: "workbuddy",
    display: "WorkBuddy",
    endpoint: "https://www.workbuddy.ai",
    api_domain: "workbuddy.ai",
    product_code: "workbuddy",
    platform: "ide",
    user_agent: "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2",
    client_version: "5.5.2",
    needs_conversation: true,
};

// 路径常量（business.mjs 33742-33766）。
const AUTH_STATE_PATH: &str = "/v2/plugin/auth/state";
const AUTH_TOKEN_PATH: &str = "/v2/plugin/auth/token";
const LOGIN_ACCOUNT_PATH: &str = "/v2/plugin/login/account";
const AUTH_REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";
const CHAT_PATH: &str = "/v2/chat/completions";
const CONFIG_PATH: &str = "/v3/config";
const SCOPED_MODELS_PATH: &str = "/console/enterprises/personal/models";
const CODE_TOKEN_NOT_READY: i64 = 11217;
const CODE_ACCOUNT_NOT_READY: i64 = 12151;
const POLL_INTERVAL_MS: u64 = 1_000;
const LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const AUTH_REFRESH_SOURCE: &str = "ide-main";
const UA: &str = "credit-ferry";

fn header(k: &'static str, v: &str) -> (&'static str, String) { (k, v.to_string()) }

/// 续期请求头（credentialRequestHeaders + refresh 专用头）。
fn refresh_headers(p: &Product, creds: &BuddyCreds) -> Vec<(&'static str, String)> {
    let domain = if creds.domain.is_empty() { p.api_domain } else { creds.domain.as_str() };
    vec![
        header("X-Domain", domain),
        header("User-Agent", p.user_agent),
        header("Authorization", &format!("Bearer {}", creds.access_token)),
        header("X-Refresh-Token", &creds.refresh_token),
        header("X-Auth-Refresh-Source", AUTH_REFRESH_SOURCE),
    ]
}

/// chat 请求头（send() 35500 的归因头全集）。
fn chat_headers(p: &Product, creds: &BuddyCreds) -> Vec<(&'static str, String)> {
    let domain = if creds.domain.is_empty() { p.api_domain } else { creds.domain.as_str() };
    vec![
        header("Authorization", &format!("Bearer {}", creds.access_token)),
        header("X-Domain", domain),
        header("X-Product-Code", p.product_code),
        header("X-Product", "SaaS"),
        header("X-Agent-Purpose", "conversation"),
        header("X-IDE-Name", p.display),
        header("X-IDE-Type", p.display),
        header("X-IDE-Version", p.client_version),
        header("User-Agent", p.user_agent),
    ]
}

/// 目录请求头（fetchModels 的 credentialAuthHeaders + X-Product/X-Product-Code）。
fn catalog_headers(p: &Product, creds: &BuddyCreds) -> Vec<(&'static str, String)> {
    let domain = if creds.domain.is_empty() { p.api_domain } else { creds.domain.as_str() };
    vec![
        header("Authorization", &format!("Bearer {}", creds.access_token)),
        header("X-Domain", domain),
        header("X-Product", "SaaS"),
        header("X-Product-Code", p.product_code),
        header("User-Agent", p.user_agent),
    ]
}

/// 业务码提取（responseCode：body.code 数字优先，否则 HTTP 状态）。
fn response_code(body: &Value, status: u16) -> i64 {
    body.get("code").and_then(|v| v.as_i64()).unwrap_or(status as i64)
}
fn response_data(body: &Value) -> Option<&Value> {
    let d = body.get("data")?;
    if d.is_null() { None } else { Some(d) }
}
fn response_message(body: &Value) -> String {
    for key in ["msg", "message"] {
        if let Some(s) = body.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// 登录：第 1 步，POST auth/state 拿 {state, authUrl}。
pub async fn fetch_auth_state(client: &Client, p: &Product) -> Result<(String, String), String> {
    let url = format!("{}{}?platform={}", p.endpoint, AUTH_STATE_PATH, p.platform);
    let resp = client
        .post(&url)
        .header("X-Domain", p.api_domain)
        .header("X-No-Authorization", "true")
        .header("X-No-User-Id", "true")
        .header("X-No-Enterprise-Id", "true")
        .header("X-No-Department-Info", "true")
        .header("User-Agent", p.user_agent)
        .send()
        .await
        .map_err(|e| format!("{} auth/state 网络错误: {}", p.id, e))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("{} auth/state 解析: {}", p.id, e))?;
    if !status.is_success() {
        return Err(format!("auth/state HTTP {}: {}", status.as_u16(), response_message(&body)));
    }
    let data = response_data(&body).ok_or_else(|| "auth/state 响应缺少 data".to_string())?;
    let state = data.get("state").and_then(|v| v.as_str()).unwrap_or("");
    let auth_url = data.get("authUrl").and_then(|v| v.as_str()).unwrap_or("");
    if state.is_empty() || auth_url.is_empty() {
        return Err("auth/state 响应缺少 state/authUrl".into());
    }
    Ok((state.to_string(), auth_url.to_string()))
}

/// 登录：第 2 步，轮询 auth/token 直到用户完成浏览器登录。
pub async fn poll_token(client: &Client, p: &Product, state: &str) -> Result<BuddyCreds, String> {
    let url = format!("{}{}?state={}", p.endpoint, AUTH_TOKEN_PATH, state);
    let deadline = std::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(format!("{} 获取 token 超时", p.id));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        let resp = client.get(&url).header("X-No-Authorization", "true").header("User-Agent", p.user_agent).send().await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => continue,
        };
        if status.is_success() {
            if let Some(data) = response_data(&body) {
                return Ok(parse_token_data(data));
            }
            continue;
        }
        if response_code(&body, status.as_u16()) == CODE_TOKEN_NOT_READY {
            continue;
        }
        return Err(format!("auth/token HTTP {}: {}", status.as_u16(), response_message(&body)));
    }
}

/// 登录：第 3 步，轮询 login/account 拿账号信息（补进凭据）。
pub async fn poll_account(client: &Client, p: &Product, creds: &mut BuddyCreds, state: &str) -> Result<(), String> {
    let url = format!("{}{}?state={}", p.endpoint, LOGIN_ACCOUNT_PATH, state);
    let deadline = std::time::Instant::now() + Duration::from_millis(LOGIN_TIMEOUT_MS);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(format!("{} 获取账户信息超时", p.id));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        let domain = if creds.domain.is_empty() { p.api_domain } else { creds.domain.as_str() };
        let resp = client
            .get(&url)
            .header("X-Domain", domain)
            .header("Authorization", &format!("Bearer {}", creds.access_token))
            .header("X-No-User-Id", "true")
            .header("X-No-Enterprise-Id", "true")
            .header("User-Agent", p.user_agent)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => continue,
        };
        if status.is_success() {
            if let Some(data) = response_data(&body) {
                if let Some(uid) = data.get("uid").and_then(|v| v.as_str()) {
                    creds.uid = uid.to_string();
                }
                if let Some(nick) = data.get("nickname").and_then(|v| v.as_str()) {
                    creds.nickname = nick.to_string();
                }
                if let Some(phone) = data.get("phone").and_then(|v| v.as_str()) {
                    creds.phone = phone.to_string();
                }
                return Ok(());
            }
            continue;
        }
        if response_code(&body, status.as_u16()) == CODE_ACCOUNT_NOT_READY {
            continue;
        }
        return Err(format!("login/account HTTP {}: {}", status.as_u16(), response_message(&body)));
    }
}

/// parseTokenData：token 响应 → 凭据（expiresAt 绝对/expiresIn 相对都归一为 ms）。
fn parse_token_data(data: &Value) -> BuddyCreds {
    let access_token = data.get("accessToken").and_then(|v| v.as_str()).unwrap_or("").to_string();
    BuddyCreds {
        access_token,
        refresh_token: data.get("refreshToken").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        expires_at: absolute_expiry_ms(data, "expiresAt", "expiresIn", ""),
        refresh_expires_at: absolute_expiry_ms(data, "refreshExpiresAt", "refreshExpiresIn", ""),
        token_type: {
            let t = data.get("tokenType").and_then(|v| v.as_str()).unwrap_or("");
            if t.is_empty() { "Bearer".to_string() } else { t.to_string() }
        },
        scope: data.get("scope").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        domain: data.get("domain").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ..Default::default()
    }
}

/// absoluteExpiryMs：优先绝对（>1e12 当 ms，否则 ×1e3 当秒），否则相对秒 + accessToken iat。
fn absolute_expiry_ms(data: &Value, abs_key: &str, rel_key: &str, access_token: &str) -> i64 {
    if let Some(abs) = data.get(abs_key).and_then(|v| v.as_str()) {
        if !abs.is_empty() {
            if abs.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = abs.parse::<i64>() {
                    return if n > 1_000_000_000_000 { n } else { n * 1000 };
                }
            } else if let Some(ms) = parse_iso_ms(abs) {
                return ms;
            }
        }
    }
    if let Some(rel) = data.get(rel_key).and_then(|v| v.as_f64()) {
        let base = jwt_issued_at_ms(access_token).unwrap_or_else(crate::store::now_ms);
        return base + (rel as i64) * 1000;
    }
    0
}

fn parse_iso_ms(text: &str) -> Option<i64> {
    // 只处理 `YYYY-MM-DDTHH:MM:SS(.sss)?(Z|±hh:mm)` 形态，够用且不引 chrono。
    let t = text.trim();
    if t.len() < 19 {
        return None;
    }
    // 日期固定前 10 字符 `YYYY-MM-DD`，其余交给时间解析。
    let mut dit = t[..10].split('-');
    let y: i64 = dit.next()?.parse().ok()?;
    let mo: i64 = dit.next()?.parse().ok()?;
    let d: i64 = dit.next()?.parse().ok()?;
    let time_part = t[10..].trim_start_matches('T');
    let mut rit = time_part.split(':');
    let h: i64 = rit.next()?.parse().ok()?;
    let mi: i64 = rit.next()?.parse().ok()?;
    // 秒可能带小数 `.sss` 或时区尾巴 `Z`/`+08:00`，只取整数部分。
    let sec_str: String = rit
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let sec: i64 = sec_str.parse().ok()?;
    // 天数（UTC，Howard Hinnant 算法），够算保活窗口。
    let days = days_from_civil(y, mo, d);
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    Some(secs * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant 算法。
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn jwt_issued_at_ms(token: &str) -> Option<i64> {
    crate::store::jwt_iat_ms(token)
}

/// 刷新：POST auth/token/refresh。RT 单次消费，调用方须持串行闸。
pub async fn refresh_token(client: &Client, p: &Product, creds: &mut BuddyCreds) -> Result<(), String> {
    if !creds.refreshable() {
        return Err(format!("{} 无 refresh_token，请重新登录", p.id));
    }
    let url = format!("{}{}", p.endpoint, AUTH_REFRESH_PATH);
    let mut req = client.post(&url);
    for (k, v) in refresh_headers(p, creds) {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("{} 刷新网络错误: {}", p.id, e))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("{} 刷新解析: {}", p.id, e))?;
    if !status.is_success() {
        let code = response_code(&body, status.as_u16());
        let msg = response_message(&body);
        let expired = status.as_u16() == 401 || status.as_u16() == 403
            || code == 401 || code == 403
            || msg.contains("expired") || msg.contains("invalid");
        if expired {
            return Err(format!("REFRESH_TERMINAL:{}", msg));
        }
        return Err(format!("{} 刷新 HTTP {}: {}", p.id, status.as_u16(), msg));
    }
    let data = response_data(&body).ok_or_else(|| "刷新响应缺少 data".to_string())?;
    let fresh = parse_token_data(data);
    // 只在刷新成功时轮换（单次消费的关键：失败不动 RT）。
    creds.access_token = fresh.access_token;
    if !fresh.refresh_token.is_empty() {
        creds.refresh_token = fresh.refresh_token;
    }
    creds.expires_at = fresh.expires_at;
    creds.refresh_expires_at = fresh.refresh_expires_at;
    if !fresh.token_type.is_empty() { creds.token_type = fresh.token_type; }
    if !fresh.scope.is_empty() { creds.scope = fresh.scope; }
    if !fresh.domain.is_empty() { creds.domain = fresh.domain; }
    Ok(())
}

pub struct BuddyClient {
    http: Client,
    listing: Client,
}

impl BuddyClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .user_agent(UA)
            .build()
            .unwrap_or_default();
        let listing = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(UA)
            .build()
            .unwrap_or_default();
        Self { http, listing }
    }

    /// 目录：scoped（personal）优先，回退 /v3/config，合并去重。
    pub async fn catalog(&self, p: &Product, creds: &BuddyCreds) -> Result<Vec<ModelEntry>, String> {
        if !creds.ready() {
            return Err(format!("{} 未登录", p.id));
        }
        let mut rows: Vec<Value> = Vec::new();
        // 1) scoped models
        let scoped_url = format!("{}{}", p.endpoint, SCOPED_MODELS_PATH);
        let mut req = self.listing.get(&scoped_url);
        for (k, v) in catalog_headers(p, creds) {
            req = req.header(k, v);
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    rows = parse_models_from_config(&body);
                }
            }
        }
        // 2) /v3/config 补充（scoped 为空或作为并集补充）
        let cfg_url = format!("{}{}", p.endpoint, CONFIG_PATH);
        let mut req = self.listing.get(&cfg_url);
        for (k, v) in catalog_headers(p, creds) {
            req = req.header(k, v);
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.json::<Value>().await {
                    let extra = parse_models_from_config(&body);
                    let known: std::collections::HashSet<String> = rows.iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect();
                    for e in extra {
                        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if !id.is_empty() && !known.contains(&id) {
                            rows.push(e);
                        }
                    }
                }
            }
        }
        if rows.is_empty() {
            return Err(format!("{} 目录为空（未登录或账号无模型权限）", p.id));
        }
        Ok(build_buddy_catalog(p.id, &rows))
    }

    /// 登录第 1 步：POST auth/state 拿 {state, authUrl}。
    pub async fn auth_state(&self, p: &Product) -> Result<(String, String), String> {
        fetch_auth_state(&self.http, p).await
    }

    /// 登录第 2 步：轮询 auth/token 直到用户在浏览器完成登录。
    pub async fn poll_login_token(&self, p: &Product, state: &str) -> Result<BuddyCreds, String> {
        poll_token(&self.http, p, state).await
    }

    /// 登录第 3 步：轮询 login/account 补齐账号信息。
    pub async fn poll_login_account(&self, p: &Product, creds: &mut BuddyCreds, state: &str) -> Result<(), String> {
        poll_account(&self.http, p, creds, state).await
    }

    /// 刷新凭据（单次消费 RT，调用方须持全局 refresh_gate）。
    pub async fn refresh(&self, p: &Product, creds: &mut BuddyCreds) -> Result<(), String> {
        refresh_token(&self.http, p, creds).await
    }

    /// chat：强制流式转发（SSE 直传）。调用方负责剥上游非 SSE 错误。
    pub async fn chat(&self, p: &Product, creds: &BuddyCreds, body: &Value) -> Result<reqwest::Response, String> {
        let url = format!("{}{}", p.endpoint, CHAT_PATH);
        // 强制 stream:true（实测非流式 400）。
        let mut body = body.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        let mut req = self.http.post(&url).header("Accept", "text/event-stream");
        for (k, v) in chat_headers(p, creds) {
            req = req.header(k, v);
        }
        if p.needs_conversation {
            // WorkBuddy 网关强校验（2026-10-19 二分实测）：三会话头缺任一 →
            // 11128 request illegal；首条不是 system → 11128 first message is
            // not system prompt。三头值只要形态是 UUID 即可，每请求新建。
            let conv = uuid::Uuid::new_v4().to_string();
            let creq = uuid::Uuid::new_v4().to_string();
            let cmsg = uuid::Uuid::new_v4().to_string();
            req = req
                .header("X-Conversation-ID", conv)
                .header("X-Conversation-Request-ID", creq)
                .header("X-Conversation-Message-ID", cmsg);
            // 首条补空 system（实测空 content 的 system 也放行，最小侵入用户提示）。
            if let Some(arr) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
                let first_is_system = arr
                    .first()
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    .map(|r| r == "system")
                    .unwrap_or(false);
                if !first_is_system {
                    arr.insert(0, serde_json::json!({ "role": "system", "content": "" }));
                }
            }
        }
        req.json(&body)
            .send()
            .await
            .map_err(|e| format!("{} chat: {}", p.id, e))
    }
}

/// parseModelsFromConfig：data.models[].id/name + agents 引用的 id，映射成行。
fn parse_models_from_config(body: &Value) -> Vec<Value> {
    let data = match body.get("data") {
        Some(d) if d.is_object() => d,
        _ => return Vec::new(),
    };
    let mut by_id = std::collections::HashMap::new();
    if let Some(models) = data.get("models").and_then(|v| v.as_array()) {
        for m in models {
            if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                by_id.insert(id.to_string(), m.clone());
            }
        }
    }
    let mut ids: Vec<String> = by_id.keys().cloned().collect();
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let meta = &by_id[&id];
        // 过滤非 chat / 自动选择别名。
        if id.contains("auto") && !meta.get("name").is_some() {
            continue;
        }
        let mut row = meta.clone();
        if let Some(obj) = row.as_object_mut() {
            obj.insert("id".into(), Value::String(id));
        }
        out.push(row);
    }
    out
}

/// 目录行 → ModelEntry（source=buddy/workbuddy）。
fn buddy_entry_of(source: &str, row: &Value) -> ModelEntry {
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let name = row.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        .unwrap_or(&id).to_string();
    let ctx = row.get("contextWindow").and_then(|v| v.as_u64()).unwrap_or(131_072);
    let max_out = row.get("maxOutputTokens").and_then(|v| v.as_u64()).unwrap_or(32_768);
    let vision = row.get("vision").and_then(|v| v.as_bool()).unwrap_or(false)
        || row.get("input").and_then(|v| v.as_array())
            .map(|a| a.iter().any(|m| m.as_str() == Some("image")))
            .unwrap_or(false);
    ModelEntry {
        upstream_id: id,
        name,
        source: source.to_string(),
        vision,
        context: ctx,
        max_output: max_out,
        multiplier: None,
        billing_status: None,
        channel: None,
    }
}

fn build_buddy_catalog(source: &str, rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let e = buddy_entry_of(source, r);
        if e.upstream_id.is_empty() || !seen.insert(e.upstream_id.clone()) {
            continue;
        }
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, name: &str) -> Value {
        json!({ "id": id, "name": name, "contextWindow": 200_000, "maxOutputTokens": 8192,
                "input": ["text", "image"] })
    }

    #[test]
    fn parses_iso_expiry_to_ms() {
        // 2026-01-01T00:00:00Z → 1767225600000
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00Z"), Some(1767225600000));
    }

    #[test]
    fn absolute_expiry_prefers_absolute_then_relative() {
        // 绝对（秒 → ×1e3）
        let v = json!({ "expiresAt": "1767225600" });
        assert_eq!(absolute_expiry_ms(&v, "expiresAt", "expiresIn", ""), 1767225600000);
        // 相对（expiresIn 秒 + now 基准）
        let v2 = json!({ "expiresIn": 3600 });
        let got = absolute_expiry_ms(&v2, "expiresAt", "expiresIn", "");
        assert!(got > crate::store::now_ms() + 3_500_000 && got < crate::store::now_ms() + 3_700_000);
    }

    #[test]
    fn parse_token_maps_all_fields() {
        let creds = parse_token_data(&json!({
            "accessToken": "at", "refreshToken": "rt", "tokenType": "Bearer",
            "expiresAt": "1767225600", "domain": "copilot.tencent.com", "scope": "all"
        }));
        assert_eq!(creds.access_token, "at");
        assert_eq!(creds.refresh_token, "rt");
        assert_eq!(creds.expires_at, 1767225600000);
        assert_eq!(creds.domain, "copilot.tencent.com");
        assert!(creds.refreshable());
    }

    #[test]
    fn config_models_parsed_and_deduped() {
        let body = json!({ "data": { "models": [row("hy3","Hy3"), row("glm","GLM"), row("hy3","dup")] } });
        let rows = parse_models_from_config(&body);
        assert_eq!(rows.len(), 2); // 去重
        let cat = build_buddy_catalog("buddy", &rows);
        assert_eq!(cat.len(), 2);
        assert!(cat.iter().all(|e| e.source == "buddy"));
        assert!(cat[0].vision);
    }

    #[test]
    fn chat_headers_use_product_identity() {
        let creds = BuddyCreds { access_token: "at".into(), domain: "d.custom".into(), ..Default::default() };
        let hs = chat_headers(&CODEBUDDY, &creds);
        let get = |k: &str| hs.iter().find(|(n,_)| *n==k).map(|(_,v)| v.clone()).unwrap_or_default();
        assert_eq!(get("X-Product-Code"), "codebuddy");
        assert_eq!(get("X-Domain"), "d.custom");
        assert_eq!(get("Authorization"), "Bearer at");
        // workbuddy 用自己的身份
        let hs2 = chat_headers(&WORKBUDDY, &BuddyCreds{ access_token: "at2".into(), ..Default::default() });
        let get2 = |k: &str| hs2.iter().find(|(n,_)| *n==k).map(|(_,v)| v.clone()).unwrap_or_default();
        assert_eq!(get2("X-Product-Code"), "workbuddy");
        assert_eq!(get2("X-Domain"), "workbuddy.ai");
    }

    #[test]
    fn domain_falls_back_when_empty() {
        let creds = BuddyCreds { access_token: "at".into(), domain: "".into(), ..Default::default() };
        let hs = chat_headers(&CODEBUDDY, &creds);
        let d = hs.iter().find(|(n,_)| *n=="X-Domain").map(|(_,v)| v.clone()).unwrap();
        assert_eq!(d, "copilot.tencent.com");
    }
}