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
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const IDE_VERSION: &str = "0.1.52";
pub const PLUGIN_VERSION: &str = "2.3.62834";
pub const BATCH_MODELS_PATH: &str = "/api/ide/v1/batch_get_detail_param";
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
        ("client_id", APP_ID),
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

pub fn trae_entry_of(row: &Value) -> ModelEntry {
    let id = row.get("id").or_else(|| row.get("model")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let name = row.get("name").or_else(|| row.get("displayName")).and_then(|v| v.as_str())
        .filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
    let context = row.get("contextLength").or_else(|| row.get("context_window")).and_then(|v| v.as_u64()).unwrap_or(200_000);
    let max_output = row.get("maxOutputTokens").or_else(|| row.get("max_tokens")).and_then(|v| v.as_u64()).unwrap_or(32_000);
    ModelEntry {
        upstream_id: id,
        name,
        source: TRAE_SOURCE.to_string(),
        vision: false,
        context,
        max_output,
        multiplier: None,
        billing_status: None,
    }
}

pub fn build_trae_catalog(rows: &[Value]) -> Vec<ModelEntry> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let e = trae_entry_of(r);
        if e.upstream_id.is_empty() || !seen.insert(e.upstream_id.clone()) {
            continue;
        }
        out.push(e);
    }
    out
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
            "ClientID": APP_ID,
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

    /// chat：伪装 IDE 身份头。
    pub async fn chat(&self, creds: &TokenCreds, body: &Value, machine_gen: u64) -> Result<reqwest::Response, String> {
        let uid = &creds.uid;
        let base_machine = creds.extra.get("machineId").and_then(|v| v.as_str()).unwrap_or(uid);
        let device_id = creds.extra.get("deviceId").and_then(|v| v.as_str()).unwrap_or("");
        let url = format!("{}{}", AGENT_HOST, "/mcp/v1/chat");
        let mut req = self.http.post(&url)
            .header("X-Cloudide-Token", &creds.access_token)
            .header("X-Ide-Token", &creds.access_token)
            .header("X-Uid", uid)
            .header("X-App-Id", APP_ID)
            .header("X-App-Version", "default")
            .header("X-Ide-Version", IDE_VERSION)
            .header("X-Ide-Version-Type", "stable")
            .header("X-Device-Type", "macos")
            .header("X-OS-Version", "14.0")
            .header("X-Device-Brand", "Apple")
            .header("X-Machine-Id", derive_rotating_machine_id(base_machine, machine_gen));
        if !device_id.is_empty() {
            req = req.header("X-Device-Id", device_id);
        }
        req.json(body).send().await.map_err(|e| format!("trae chat: {}", e))
    }

    /// 目录：POST batch_get_detail_param。
    pub async fn catalog(&self, creds: &TokenCreds) -> Result<Vec<ModelEntry>, String> {
        let resp = self.listing.post(format!("{}{}", AGENT_HOST, BATCH_MODELS_PATH))
            .header("X-Cloudide-Token", &creds.access_token)
            .header("X-Ide-Token", &creds.access_token)
            .header("X-Uid", &creds.uid)
            .header("X-App-Id", APP_ID)
            .header("Content-Type", "application/json")
            .json(&json!({}))
            .send().await.map_err(|e| format!("trae models: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("trae models body: {}", e))?;
        if !status.is_success() {
            return Err(format!("models HTTP {}", status.as_u16()));
        }
        let data = value.get("data").unwrap_or(&value);
        let rows: Vec<Value> = data.as_array().cloned()
            .or_else(|| data.get("models").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();
        Ok(build_trae_catalog(&rows))
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
    fn catalog_deduped() {
        let rows = vec![
            json!({ "id": "deepseek-v4", "name": "DeepSeek V4", "contextLength": 200_000 }),
            json!({ "id": "deepseek-v4", "name": "dup" }),
            json!({ "model": "glm-5.2", "displayName": "GLM 5.2" }),
        ];
        let cat = build_trae_catalog(&rows);
        assert_eq!(cat.len(), 2);
        assert_eq!(cat[0].upstream_id, "deepseek-v4");
    }
}