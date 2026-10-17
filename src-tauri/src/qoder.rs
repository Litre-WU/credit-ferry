//! Qoder / Qoder CN —— 原生渠道。
//!
//! 协议规格来自 Our Free Model 逆向（`business.mjs`），逐条核对行号：
//! - 登录（device + PKCE）：打开 `{authBase}/device/selectAccounts?challenge=
//!   S256(pkce_verifier)&challenge_method=S256&nonce&machine_id&client_id`，
//!   浏览器选号授权 → 轮询 `{openApiBase}/api/v1/deviceToken/poll?
//!   nonce&verifier&challenge_method=S256` 换 token。原生可做。
//! - 刷新：POST /api/v1/deviceToken/refresh。原生可做。
//! - 每日额度（原生可做）：GET /sash/api/v2/me/usage + 签到
//!   POST /sash/api/v1/me/campaigns/{id}/claim（每日 10:00 UTC+8 刷新）。
//! - 目录：远端无端点，OFM 用一份**硬编码兜底表**（catalog-v6 实测校准），
//!   本模块内置同一张表；原生可做。
//! - ⚠️ **chat 不可原生化**：推理请求体与 `Authorization: Bearer COSY.<载荷>.<签名>`
//!   全部由 `qoder-auth-wasm.wasm` 的 `qodercontext_prepareInferRequest` 生成
//!   （签名算法编译在 WASM 内），普通 Bearer 会被判 `403 Signature invalid`。
//!   因此本模块的 `chat` 诚实返回「需 WASM 签名器」，不做伪装请求。
//!
//! 国际版 api2.qoder.sh 在本机网络下 HTTP/2 NGHTTP2_INTERNAL_ERROR（OFM
//! 注释亦记录），故国际版 chat 另受网络可达性限制。

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::{ModelEntry, TokenCreds};

pub const QODER_SOURCE: &str = "qoder";
pub const QODERCN_SOURCE: &str = "qodercn";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QoderProduct {
    pub id: &'static str,
    pub source: &'static str,
    pub auth_base: &'static str,
    pub open_api_base: &'static str,
    pub client_id: &'static str,
}

pub const QODER: QoderProduct = QoderProduct {
    id: "qoder",
    source: QODER_SOURCE,
    auth_base: "https://qoder.com",
    open_api_base: "https://openapi.qoder.sh",
    client_id: "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb",
};

pub const QODERCN: QoderProduct = QoderProduct {
    id: "qodercn",
    source: QODERCN_SOURCE,
    auth_base: "https://qoder.cn",
    open_api_base: "https://openapi.qoder.com.cn",
    client_id: "f24c2b03-2f37-4f4a-9a52-2b0aa2f27d0f",
};

const DEVICE_SELECT_PATH: &str = "/device/selectAccounts";
const POLL_PATH: &str = "/api/v1/deviceToken/poll";
const REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";
const USAGE_PATH: &str = "/sash/api/v2/me/usage";
const CAMPAIGNS_PATH: &str = "/sash/api/v1/me/campaigns";
const CAMPAIGN_REFRESH_HOUR_UTC8: u8 = 10;
const UA: &str = "credit-ferry";
const TIMEOUT_MS: u64 = 60_000;

/// PKCE S256：base64url(sha256(verifier))。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64_url(&digest)
}

fn base64_url(bytes: &[u8]) -> String {
    // 不引 base64 crate 依赖：手写 base64url 编码（URL-safe, 无 padding）。
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

/// 兜底模型表（catalog-v6 实测校准，节选核心几条；contextWindow 取官方档位表最大档）。
pub fn fallback_models(source: &str) -> Vec<ModelEntry> {
    let rows: &[(&str, &str, u64, f64, bool)] = &[
        ("auto", "Auto", 200_000, 0.5, true),
        ("ultimate", "Ultimate", 1_000_000, 2.0, true),
        ("performance", "Performance", 1_000_000, 1.1, true),
        ("efficient", "Efficient", 1_000_000, 0.3, true),
        ("smodel", "Sonus", 1_000_000, 8.0, true),
        ("cmodel", "Cantus", 1_000_000, 4.0, true),
        ("qmodel_38max", "Max", 1_000_000, 0.2, true),
        ("qfmodel", "Flash", 1_000_000, 0.0, true),
    ];
    rows.iter().map(|(id, name, ctx, price, vision)| ModelEntry {
        upstream_id: id.to_string(),
        name: name.to_string(),
        source: source.to_string(),
        vision: *vision,
        context: *ctx,
        max_output: 32_768,
        multiplier: Some(*price),
        billing_status: if *price == 0.0 { Some("免费".to_string()) } else { None },
    }).collect()
}

/// 每日次数制超额判定（business code 110 / 文案 daily count exceeded）。
pub fn is_daily_exhausted(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    msg.contains("\"code\":\"110\"")
        || msg.contains("\"code\": 110")
        || lower.contains("daily count exceeded")
        || lower.contains("billing_error")
        || lower.contains("quota exceeded")
}

pub struct QoderClient {
    http: Client,
    listing: Client,
}

impl QoderClient {
    pub fn new() -> Self {
        let http = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        let listing = Client::builder().timeout(Duration::from_millis(TIMEOUT_MS)).connect_timeout(Duration::from_secs(15)).user_agent(UA).build().unwrap_or_default();
        Self { http, listing }
    }

    /// 登录第 1 步：生成 PKCE verifier，构造浏览器授权 URL。
    pub fn build_auth_url(&self, p: &QoderProduct, verifier: &str, nonce: &str, machine_id: &str) -> String {
        let challenge = pkce_challenge(verifier);
        format!(
            "{}{}?challenge={}&challenge_method=S256&nonce={}&machine_id={}&client_id={}",
            p.auth_base, DEVICE_SELECT_PATH, challenge, nonce, machine_id, p.client_id
        )
    }

    /// 登录第 2 步：轮询 poll 换 token。
    pub async fn poll_token(&self, p: &QoderProduct, verifier: &str, nonce: &str) -> Result<TokenCreds, String> {
        let url = format!("{}{}?nonce={}&verifier={}&challenge_method=S256", p.open_api_base, POLL_PATH, nonce, verifier);
        let resp = self.http.get(&url).header("User-Agent", "Qoder")
            .send().await.map_err(|e| format!("qoder poll: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("qoder poll body: {}", e))?;
        if !status.is_success() {
            return Err("等待用户完成授权".to_string());
        }
        let access_token = value.get("access_token").and_then(|v| v.as_str())
            .or_else(|| value.get("token").and_then(|v| v.as_str()))
            .unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err("等待用户完成授权（尚无 token）".into());
        }
        let expire_time = value.get("expire_time").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let expires_at = if expire_time > 0.0 {
            if expire_time > 1e12 { expire_time as i64 } else { (expire_time as i64) * 1000 }
        } else {
            crate::store::jwt_exp_ms(&access_token).unwrap_or(0)
        };
        let security_oauth = value.get("security_oauth_token").and_then(|v| v.as_str()).unwrap_or("");
        let machine_id = value.get("machine_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(TokenCreds {
            access_token,
            refresh_token: value.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            expires_at,
            nickname: value.get("email").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            extra: json!({ "security_oauth_token": security_oauth, "machine_id": machine_id }),
            ..Default::default()
        })
    }

    /// 刷新（RT 单次消费，调用方持 refresh_gate）。
    pub async fn refresh(&self, p: &QoderProduct, creds: &mut TokenCreds) -> Result<(), String> {
        if creds.refresh_token.is_empty() {
            return Err("qoder 无 refresh_token，请重新登录".into());
        }
        let machine_id = creds.extra.get("machine_id").and_then(|v| v.as_str()).unwrap_or("");
        let resp = self.http.post(format!("{}{}", p.open_api_base, REFRESH_PATH))
            .header("Content-Type", "application/json")
            .json(&json!({ "refresh_token": creds.refresh_token, "machine_id": machine_id, "client_id": p.client_id }))
            .send().await.map_err(|e| format!("qoder refresh: {}", e))?;
        let status = resp.status();
        let value: Value = resp.json().await.map_err(|e| format!("qoder refresh body: {}", e))?;
        if !status.is_success() {
            return Err(format!("REFRESH_TERMINAL:refresh HTTP {}", status.as_u16()));
        }
        let data = value.get("data").unwrap_or(&value);
        if let Some(t) = data.get("access_token").and_then(|v| v.as_str()) {
            creds.access_token = t.to_string();
        }
        if let Some(t) = data.get("security_oauth_token").and_then(|v| v.as_str()) {
            creds.extra["security_oauth_token"] = json!(t);
        }
        let expire_time = data.get("expire_time").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if expire_time > 0.0 {
            creds.expires_at = if expire_time > 1e12 { expire_time as i64 } else { (expire_time as i64) * 1000 };
        }
        Ok(())
    }

    /// 每日额度 + 签到（原生可做，护住每日免费额度）。
    pub async fn claim_daily(&self, p: &QoderProduct, creds: &TokenCreds) -> Result<Value, String> {
        let mut req = self.http.post(format!("{}{}", p.open_api_base, CAMPAIGNS_PATH))
            .header("Cosy-ClientType", "10")
            .header("User-Agent", "Qoder")
            .header("Content-Type", "application/json");
        if !creds.access_token.is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", creds.access_token));
        }
        let resp = req.json(&json!({})).send().await.map_err(|e| format!("qoder campaigns: {}", e))?;
        let value: Value = resp.json().await.unwrap_or(json!({}));
        let campaigns = value.get("campaigns").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let mut claimed = 0.0;
        for c in &campaigns {
            let claimable = c.get("claimStatus").and_then(|v| v.as_str()) == Some("CLAIMABLE")
                && c.get("actionType").and_then(|v| v.as_str()) == Some("CLAIM_BENEFIT");
            let Some(cid) = c.get("campaignId").and_then(|v| v.as_str()) else { continue };
            if !claimable {
                continue;
            }
            let resp2 = self.http.post(format!("{}{}/{}/claim", p.open_api_base, CAMPAIGNS_PATH, cid))
                .header("Cosy-ClientType", "10")
                .header("User-Agent", "Qoder")
                .bearer_auth(&creds.access_token)
                .json(&json!({})).send().await;
            if let Ok(r) = resp2 {
                if r.status().is_success() {
                    if let Ok(v) = r.json::<Value>().await {
                        if let Some(a) = v.pointer("/benefit/amount").and_then(|x| x.as_f64()) {
                            claimed += a;
                        }
                    }
                }
            }
        }
        Ok(json!({ "claimed": claimed, "refreshHourUtc8": CAMPAIGN_REFRESH_HOUR_UTC8 }))
    }

    /// 目录：远端无端点，用内置兜底表。
    pub fn catalog(&self, p: &QoderProduct) -> Vec<ModelEntry> {
        fallback_models(p.source)
    }

    /// chat：**不可原生化**——请求体与 Bearer COSY.<载荷>.<签名> 由
    /// qoder-auth-wasm.wasm 生成，普通 Bearer 会被判 403 Signature invalid。
    pub fn chat(&self, _p: &QoderProduct, _creds: &TokenCreds, _body: &Value) -> Result<reqwest::Response, String> {
        Err("Qoder 推理端点需 WASM 签名（qoder-auth-wasm.wasm 生成 Bearer COSY.<载荷>.<签名> 与加密请求体），Rust 端暂不可原生调用；登录/目录/每日签到额度已原生支持".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        // RFC 7636 官方测试向量：verifier "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // → challenge "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        let got = pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(got, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn auth_url_contains_pkce_params() {
        let c = QoderClient::new();
        let url = c.build_auth_url(&QODER, "verifier123", "nonce1", "machine1");
        assert!(url.starts_with("https://qoder.com/device/selectAccounts?"));
        assert!(url.contains("challenge_method=S256"));
        assert!(url.contains("nonce=nonce1"));
        assert!(url.contains("machine_id=machine1"));
        assert!(url.contains("client_id=e883ade2"));
    }

    #[test]
    fn daily_exhausted_detected() {
        assert!(is_daily_exhausted("{\"code\":110,\"msg\":\"billing daily count exceeded\"}"));
        assert!(!is_daily_exhausted("normal response"));
    }

    #[test]
    fn fallback_catalog_marks_free() {
        let cat = fallback_models(QODER_SOURCE);
        assert!(cat.iter().any(|m| m.upstream_id == "qfmodel" && m.multiplier == Some(0.0)));
        assert!(cat.iter().all(|m| m.source == "qoder"));
    }
}