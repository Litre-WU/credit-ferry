//! Huawei Maodao (码道 / CodeArts Doer) upstream adapter.
//!
//! Wire facts recovered from the local CodeArts desktop client
//! (`%APPDATA%\codearts\` + `resources/app.asar`) and verified live:
//!
//! - LLM gateway: `https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions`.
//!   The body is OpenAI-shaped (`model`/`messages`/`max_tokens`/`temperature`/
//!   `stream`/`stream_options`/`user_prompt`). **When `stream` is omitted the
//!   upstream still answers with SSE**, so we always pin it explicitly.
//! - Model catalogue: `GET https://opengw.developer.huaweicloud.com/api/v1/gateway/config`
//!   (optional `channel=education`), response
//!   `{error_code, result:{base_url, models:[{model_id, model_name,
//!   context_window, max_tokens, truncate_length, model_desc}]}}`.
//! - Quota: `GET https://opengw.developer.huaweicloud.com/api/v1/user/tokens/balance`
//!   → `result.total_balance` tokens.
//! - Auth: Huawei APIG AKSK signing —
//!   `Authorization: SDK-HMAC-SHA256 Access=<AK>, SignedHeaders=<…>, Signature=<sig>`
//!   where `stringToSign = "SDK-HMAC-SHA256\n" + X-Sdk-Date + "\n" + sha256hex(canonicalRequest)`
//!   and `signature = HMAC-SHA256(secretKey, stringToSign)`. The signed header set
//!   is the outgoing headers plus `host` and `x-sdk-date`.
//! - Rotation: `POST https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens`,
//!   form-encoded, `client_id=codearts`, DPoP-gated. The refresh token is
//!   **jkt-bound** to the DPoP key pair minted at login and rotates on every
//!   use, so both the token and the key must be persisted.
//! - Login: browser OAuth loopback —
//!   `…/portal/authorize?…&code_challenge=…&port=<p>&ticket_id=<hex>` with a local
//!   listener on `127.0.0.1:<p>/oauth/callback`. If the portal answers with
//!   `secret` instead of `code` it is the legacy ticket path:
//!   `GET <snapEngine>/snap-manager/v1/login/ticket?ticket_id=…&secret=…`.
//!
//! The desktop client keeps its session in Electron `safeStorage`, i.e. Chromium
//! OSCrypt v10: an AES-256-GCM key DPAPI-wrapped in `Local State`
//! (`os_crypt.encrypted_key`, base64 with a `DPAPI` prefix), one
//! `secretStorage/<sha256>.enc` blob per secret. We only ever read it, under the
//! same Windows account that created it.
//!
//! Note on ownership: the refresh token is single-use, so CreditFerry and the
//! desktop client cannot both rotate the same session. Prefer logging in from
//! the panel; "从客户端导入" is the convenience path for an already running
//! client (re-import after the client rotates).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex as AsyncMutex};

use crate::store::{MaodaoCreds, ModelEntry};

pub const SNAP_ENGINE: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
pub const IAM_STS_HOST: &str = "https://sts.cn-north-4.myhuaweicloud.com";
pub const PORTAL_HOST: &str = "https://codearts.huaweicloud.com/portal";
pub const BENEFIT_API: &str = "https://opengw.developer.huaweicloud.com";
pub const CLIENT_ID: &str = "codearts";
pub const AUTH_REDIRECT_PATH: &str = "/oauth/callback";
pub const LOGIN_TIMEOUT_MS: u64 = 300_000;

const CHAT_PATH: &str = "/api/v2/chat/completions";
const TOKEN_PATH: &str = "/v1/oauth2/tokens";
const BALANCE_PATH: &str = "/api/v1/user/tokens/balance";
const GATEWAY_CONFIG_PATH: &str = "/api/v1/gateway/config";
const SDK_ALGORITHM: &str = "SDK-HMAC-SHA256";

// Parity with the desktop client's self-identification.
const IDE_NAME: &str = "codearts_space";
const PLUGIN_NAME: &str = "snap_AIIDE";
const PLUGIN_VERSION: &str = "5.1.0";

/// Characters `encodeURIComponent` leaves literal — the client signs with JS
/// semantics, so our percent-encoding has to match byte for byte.
const URI_SAFE: &str = "!'()*-._~";

// ------------------------------------------------------------------ crypto

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| e.to_string())
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> Result<String, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(message);
    let result = mac.finalize().into_bytes();
    let mut out = String::with_capacity(result.len() * 2);
    for byte in result {
        out.push_str(&format!("{:02x}", byte));
    }
    Ok(out)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    crate::store::now_ms()
}

// ------------------------------------------------------------- AKSK signing

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || URI_SAFE.contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// APIG canonical URI: every path segment URI-encoded, always trailing "/".
fn canonical_uri(path: &str) -> String {
    let joined: String = path
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/");
    if joined.ends_with('/') {
        joined
    } else {
        format!("{}/", joined)
    }
}

/// APIG canonical query string: sorted `k=v` pairs joined by "&".
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for chunk in query.split('&') {
        if chunk.is_empty() {
            continue;
        }
        let mut parts = chunk.splitn(2, '=');
        let key = percent_encode(parts.next().unwrap_or(""));
        let value = percent_encode(parts.next().unwrap_or(""));
        pairs.push(format!("{}={}", key, value));
    }
    pairs.sort();
    pairs.join("&")
}

/// `YYYYMMDDTHHMMSSZ` in UTC — the APIG `X-Sdk-Date` format.
fn sdk_date() -> String {
    let secs = now_secs();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Inverse of `days_from_civil` (Howard Hinnant's civil-date algorithms).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Days since 1970-01-01 (Howard Hinnant's civil algorithms).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Build the signed APIG headers for one request. The outgoing headers are
/// signed together with `host` and `x-sdk-date`.
fn signed_headers(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    creds: &MaodaoCreds,
) -> Result<HeaderMap, String> {
    let (host, path, query) = split_url(url)?;
    let date = sdk_date();

    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        map.insert(name.to_lowercase(), value.clone());
    }
    map.insert("host".into(), host);
    map.insert("x-sdk-date".into(), date.clone());

    let sorted: Vec<(&String, &String)> = map.iter().collect();
    let block: String = sorted
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect();
    let names: String = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_uri(&path),
        canonical_query(&query),
        block,
        names,
        sha256_hex(body)
    );
    let string_to_sign = format!(
        "{}\n{}\n{}",
        SDK_ALGORITHM,
        date,
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = hmac_sha256_hex(creds.secret_key.as_bytes(), string_to_sign.as_bytes())?;

    let mut out = HeaderMap::new();
    for (name, value) in sorted.iter() {
        // `Host` is signed but belongs to reqwest, which derives it from the URL.
        if name.as_str() == "host" {
            continue;
        }
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
        let header_value = HeaderValue::from_str(value).map_err(|e| e.to_string())?;
        out.insert(header_name, header_value);
    }
    out.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!(
            "{} Access={}, SignedHeaders={}, Signature={}",
            SDK_ALGORITHM, creds.access_key, names, signature
        ))
        .map_err(|e| e.to_string())?,
    );
    Ok(out)
}

/// Minimal URL splitter (avoids pulling in a full URL parser).
fn split_url(url: &str) -> Result<(String, String, String), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("不支持的 URL: {}", url))?;
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
    let (path, query) = match tail.find('?') {
        Some(pos) => (tail[..pos].to_string(), tail[pos + 1..].to_string()),
        None => (tail.to_string(), String::new()),
    };
    let path = if path.is_empty() { "/".to_string() } else { path };
    Ok((authority.to_string(), path, query))
}

/// The headers the Maodao gateway expects on every call.
fn request_headers(creds: &MaodaoCreds) -> Vec<(String, String)> {
    let token = creds.security_token.clone();
    vec![
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
        ("x-language".into(), "zh-cn".into()),
        ("ide-name".into(), IDE_NAME.into()),
        ("plugin-name".into(), PLUGIN_NAME.into()),
        ("plugin-version".into(), PLUGIN_VERSION.into()),
        ("maas_type".into(), "benefit".into()),
        ("x-security-token".into(), token.clone()),
        ("x-auth-token".into(), token),
    ]
}

/// Signed headers for a request carrying `body` (already serialized).
fn sign_for(method: &str, url: &str, creds: &MaodaoCreds, body: &[u8]) -> Result<HeaderMap, String> {
    let headers = request_headers(creds);
    signed_headers(method, url, &headers, body, creds)
}

// -------------------------------------------------------------------- DPoP

/// A DPoP key pair: the EC P-256 private key plus its public JWK, embedded
/// verbatim in the `dpop+jwt` protected header.
#[derive(Clone)]
pub struct DpopKey {
    signing: SigningKey,
    public_jwk: Value,
}

impl DpopKey {
    /// Mint a fresh P-256 key pair from the app's OS-backed CSPRNG.
    pub fn generate() -> Result<(Self, String, String), String> {
        // A uniform 32-byte scalar is a valid P-256 key with overwhelming
        // probability; retry only on the (astronomically unlikely) reject.
        for _ in 0..8 {
            let seed = crate::store::random_bytes(32);
            if let Ok(key) = SigningKey::from_slice(&seed) {
                return Self::from_key(key);
            }
        }
        Err("生成 DPoP 密钥失败".to_string())
    }

    fn from_key(key: SigningKey) -> Result<(Self, String, String), String> {
        let verifying = key.verifying_key();
        let point = verifying.to_encoded_point(false);
        let x = point.x().ok_or("公钥缺少 x 坐标")?;
        let y = point.y().ok_or("公钥缺少 y 坐标")?;
        let public_jwk = json!({
        "kty": "EC",
        "crv": "P-256",
        "x": b64url(x.as_slice()),
        "y": b64url(y.as_slice()),
    });
        let mut private_jwk = public_jwk.clone();
        private_jwk["d"] = Value::String(b64url(&key.to_bytes()));
        Ok((
            Self {
                signing: key,
                public_jwk: public_jwk.clone(),
            },
            private_jwk.to_string(),
            public_jwk.to_string(),
        ))
    }

    pub fn from_private(jwk: &str) -> Result<Self, String> {
        let document: Value = serde_json::from_str(jwk).map_err(|e| e.to_string())?;
        let d = document
            .get("d")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "DPoP 私钥缺少 d".to_string())?;
        let raw = b64url_decode(d)?;
        let signing = SigningKey::from_slice(&raw).map_err(|e| e.to_string())?;
        let public_jwk = json!({
        "kty": "EC",
        "crv": "P-256",
        "x": document.get("x").cloned().unwrap_or(Value::Null),
        "y": document.get("y").cloned().unwrap_or(Value::Null),
    });
        Ok(Self {
            signing,
            public_jwk,
        })
    }

    /// Build an RFC 9449 DPoP proof JWT for one request.
    pub fn proof(&self, htm: &str, htu: &str) -> Result<String, String> {
        let header = json!({
        "alg": "ES256",
        "typ": "dpop+jwt",
        "jwk": self.public_jwk,
    });
        let payload = json!({
        "htm": htm,
        "htu": htu,
        "iat": now_secs(),
        "jti": crate::store::new_hex(16),
    });
        let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(payload.to_string().as_bytes())
    );
        let signature: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        // P-256 ECDSA → raw r||s (64 bytes), which is what JWS ES256 expects.
        let mut raw = Vec::with_capacity(64);
        for byte in signature.to_bytes().iter() {
            raw.push(*byte);
        }
        Ok(format!("{}.{}", signing_input, b64url(&raw)))
    }
}

// ------------------------------------------------------------------ client

pub struct MaodaoClient {
    http: Client,
}

impl MaodaoClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    /// Model catalogue from the benefit (and education) channels.
    pub async fn catalog(&self, creds: &MaodaoCreds) -> Result<Vec<ModelEntry>, String> {
        let mut entries: Vec<ModelEntry> = Vec::new();
        let mut seen: BTreeMap<String, bool> = BTreeMap::new();

        for channel in [None, Some("education")] {
            let url = match channel {
                Some(name) => format!("{}{}?channel={}", BENEFIT_API, GATEWAY_CONFIG_PATH, name),
                None => format!("{}{}", BENEFIT_API, GATEWAY_CONFIG_PATH),
            };
            let signed = sign_for("GET", &url, creds, b"")?;
            let response = self
                .http
                .get(&url)
                .headers(signed)
                .send()
                .await
                .map_err(|e| format!("maodao gateway/config: {}", e))?;
            let status = response.status();
            if !status.is_success() {
                // The education channel is optional; the benefit one is not.
                if channel.is_some() {
                    continue;
                }
                let text = response.text().await.unwrap_or_default();
                return Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)));
            }
            let text = response.text().await.unwrap_or_default();
            let document: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            let code = document
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if code != "0000" {
                if channel.is_some() {
                    continue;
                }
                return Err(format!("gateway/config error_code={}", code));
            }
            let models = document
                .pointer("/result/models")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for model in models {
                let id = text_field(&model, &["model_id", "modelId"]);
                if id.is_empty() || seen.contains_key(&id) {
                    continue;
                }
                let name = text_field(&model, &["model_name", "modelName"]);
                seen.insert(id.clone(), true);
                entries.push(ModelEntry {
                    name: if name.is_empty() { id.clone() } else { name },
                    upstream_id: id,
                    source: "maodao".into(),
                    vision: model
                        .get("supports_images")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    context: num_field(&model, &["context_window", "contextWindow"]),
                    max_output: num_field(&model, &["max_tokens", "maxTokens"]),
                    multiplier: None,
                    billing_status: Some("benefit".into()),
        channel: None,
                });
            }
        }
        Ok(entries)
    }

    /// Credit balance reported by the opengw balance endpoint.
    pub async fn quota(&self, creds: &MaodaoCreds) -> Result<Value, String> {
        let url = format!("{}{}", BENEFIT_API, BALANCE_PATH);
        let signed = sign_for("GET", &url, creds, b"")?;
        let response = self
            .http
            .get(&url)
            .headers(signed)
            .send()
            .await
            .map_err(|e| format!("maodao balance: {}", e))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)));
        }
        let body: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        let total = first_num(&body, &[
            "/result/total_balance",
            "/result/total_quota",
            "/result/balance",
        ]);
        let used = first_num(&body, &[
            "/result/used_amount",
            "/result/monthly_tokens_used",
        ]);
        let available = match (total, used) {
            (Some(t), Some(u)) => Some(t - u),
            (Some(t), None) => Some(t),
            _ => None,
        };
        Ok(json!({
            "total": total,
            "used": used,
            "available": available,
            "dailyLimit": first_num(&body, &["/result/daily_token_limit"]),
            "dailyUsed": first_num(&body, &["/result/daily_tokens_used"]),
            "raw": body,
        }))
    }

    /// Forward a chat/completions request; returns the raw upstream response.
    pub async fn chat(&self, creds: &MaodaoCreds, body: &Value) -> Result<reqwest::Response, String> {
        let url = format!("{}{}", SNAP_ENGINE, CHAT_PATH);
        // The signature covers the exact bytes on the wire, so serialize once
        // and send those bytes rather than letting reqwest rebuild the body.
        let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let signed = sign_for("POST", &url, creds, &payload)?;
        self.http
            .post(&url)
            .headers(signed)
            .body(payload)
            .send()
            .await
            .map_err(|e| format!("maodao chat: {}", e))
    }
}

impl Default for MaodaoClient {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------- token rotation

#[derive(Deserialize)]
struct StsCredentials {
    #[serde(alias = "accessKeyId")]
    access_key_id: String,
    #[serde(alias = "secretAccessKey")]
    secret_access_key: String,
    #[serde(alias = "securityToken")]
    security_token: String,
    #[serde(default)]
    expiration: Option<String>,
}

#[derive(Deserialize)]
struct StsTokenResponse {
    credentials: StsCredentials,
    #[serde(alias = "refreshToken", default)]
    refresh_token: Option<String>,
}

/// Exchange an authorization code for a session (login path).
pub async fn exchange_code(
    code: &str,
    code_verifier: &str,
    dpop: &DpopKey,
    redirect_port: u16,
) -> Result<MaodaoCreds, String> {
    let url = format!("{}{}", IAM_STS_HOST, TOKEN_PATH);
    let proof = dpop.proof("POST", &url)?;
    let form = url_encode(&[
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", &redirect_uri(redirect_port)),
    ]);
    let response = Client::new()
        .post(&url)
        .header("DPoP", proof)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"))
        .body(form)
        .send()
        .await
        .map_err(|e| format!("maodao token exchange: {}", e))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)));
    }
    let parsed: StsTokenResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(apply_credentials(parsed))
}

/// Rotate a session using its refresh token. `creds` is updated in place.
pub async fn refresh_session(creds: &mut MaodaoCreds) -> Result<(), String> {
    if creds.refresh_token.is_empty() {
        return Err("码道没有刷新令牌，请重新登录".to_string());
    }
    if creds.dpop_private.is_empty() {
        return Err("码道缺少 DPoP 密钥，请重新登录".to_string());
    }
    let dpop = DpopKey::from_private(&creds.dpop_private)?;
    let url = format!("{}{}", IAM_STS_HOST, TOKEN_PATH);
    let proof = dpop.proof("POST", &url)?;
    let form = url_encode(&[
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", creds.refresh_token.as_str()),
    ]);
    let response = Client::new()
        .post(&url)
        .header("DPoP", proof)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"))
        .body(form)
        .send()
        .await
        .map_err(|e| format!("maodao token refresh: {}", e))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)));
    }
    let parsed: StsTokenResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let rotated = apply_credentials(parsed);
    creds.access_key = rotated.access_key;
    creds.secret_key = rotated.secret_key;
    creds.security_token = rotated.security_token;
    if !rotated.refresh_token.is_empty() {
        // The endpoint rotates the RT; a single-use token must not be kept.
        creds.refresh_token = rotated.refresh_token;
    }
    creds.expires_at = rotated.expires_at;
    Ok(())
}

/// Legacy ticket path: poll SnapEngine with `ticket_id` + `secret`.
pub async fn exchange_ticket(ticket_id: &str, secret: &str, dpop: &DpopKey) -> Result<MaodaoCreds, String> {
    let url = format!(
        "{}/snap-manager/v1/login/ticket?ticket_id={}&secret={}",
        SNAP_ENGINE,
        percent_encode(ticket_id),
        percent_encode(secret)
    );
    let proof = dpop.proof("GET", &url)?;
    let response = Client::new()
        .get(&url)
        .header("DPoP", proof)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .send()
        .await
        .map_err(|e| format!("maodao ticket: {}", e))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)));
    }
    let document: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    let pick = |paths: &[&str]| -> String {
        for path in paths {
            if let Some(value) = document.pointer(path).and_then(|v| v.as_str()) {
                return value.to_string();
            }
        }
        String::new()
    };
    let creds = MaodaoCreds {
        access_key: pick(&["/credentials/access_key_id", "/access_key_id"]),
        secret_key: pick(&["/credentials/secret_access_key", "/secret_access_key"]),
        security_token: pick(&["/credentials/security_token", "/security_token"]),
        refresh_token: pick(&["/refresh_token"]),
        expires_at: document
            .pointer("/credentials/expiration")
            .and_then(|v| v.as_str())
            .and_then(parse_rfc3339_ms)
            .unwrap_or_else(|| now_ms() + 3600 * 1000),
        dpop_private: String::new(),
        dpop_public: String::new(),
        user_name: String::new(),
    };
    if creds.access_key.is_empty() {
        return Err("ticket 响应缺少 credentials".to_string());
    }
    Ok(creds)
}

fn apply_credentials(parsed: StsTokenResponse) -> MaodaoCreds {
    let expires_at = parsed
        .credentials
        .expiration
        .as_deref()
        .and_then(parse_rfc3339_ms)
        .unwrap_or_else(|| now_ms() + 3600 * 1000);
    MaodaoCreds {
        access_key: parsed.credentials.access_key_id,
        secret_key: parsed.credentials.secret_access_key,
        security_token: parsed.credentials.security_token,
        refresh_token: parsed.refresh_token.unwrap_or_default(),
        expires_at,
        dpop_private: String::new(),
        dpop_public: String::new(),
        user_name: String::new(),
    }
}

/// The loopback redirect URI the portal must call back on.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{}{}", port, AUTH_REDIRECT_PATH)
}

/// Build the portal login URL (browser OAuth loopback + PKCE + ticket id).
pub fn login_url(port: u16, code_challenge: &str, ticket_id: &str) -> String {
    format!(
        "{}/authorize?theme=2&locale=zh-cn&uri_scheme={}&client_id={}&port={}&code_challenge={}&code_challenge_method=SHA-256&ticket_id={}&plugin-name={}&plugin-version={}",
        PORTAL_HOST, CLIENT_ID, CLIENT_ID, port, code_challenge, ticket_id,
        PLUGIN_NAME, PLUGIN_VERSION
    )
}

/// `application/x-www-form-urlencoded` serializer for the STS endpoint.
fn url_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse `2026-10-14T17:42:40.403Z` → ms since epoch (UTC assumed).
fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let text = value.trim();
    let parts: Vec<&str> = text.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date: Vec<i64> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    if date.len() != 3 {
        return None;
    }
    let time: Vec<i64> = parts[1]
        .trim_end_matches('Z')
        .split('.')
        .next()
        .unwrap_or("")
        .split(':')
        .filter_map(|p| p.parse().ok())
        .collect();
    if time.len() != 3 {
        return None;
    }
    let days = days_from_civil(date[0], date[1], date[2]);
    Some((days * 86_400 + time[0] * 3600 + time[1] * 60 + time[2]) * 1000)
}

// --------------------------------------------------------------- login flow

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaodaoLoginStatus {
    Pending,
    Success,
    Failed(String),
    TimedOut,
}

/// Shared state handed to the loopback callback handler.
#[derive(Clone)]
pub struct MaodaoLoginContext {
    pub code_verifier: String,
    pub dpop_private: String,
    pub ticket_id: String,
    pub redirect_port: u16,
    pub result: Arc<AsyncMutex<Option<Result<MaodaoCreds, String>>>>,
    pub tx: Arc<watch::Sender<MaodaoLoginStatus>>,
}

/// One in-flight browser login.
pub struct MaodaoLogin {
    pub url: String,
    pub started_at_ms: i64,
    /// The DPoP pair minted for this login — must be stored with the session.
    pub dpop_private: String,
    pub dpop_public: String,
    /// Set by the callback task when the code/ticket exchange finishes.
    pub result: Arc<AsyncMutex<Option<Result<MaodaoCreds, String>>>>,
    pub rx: watch::Receiver<MaodaoLoginStatus>,
}

/// Rotate the token when it is within `margin_ms` of expiring; returns the
/// (possibly refreshed) credentials. Errors mean "re-login required".
pub async fn ensure_fresh(creds: &mut MaodaoCreds, margin_ms: i64) -> Result<(), String> {
    if creds.security_token.is_empty() {
        return Err("码道未登录".to_string());
    }
    if creds.expires_at - now_ms() > margin_ms {
        return Ok(());
    }
    refresh_session(creds).await
}

/// Start the loopback listener and return the login flow (url + poll handle).
/// The caller opens `flow.url` in the user's browser.
pub async fn login_start() -> Result<MaodaoLogin, String> {
    // Port 0 → the OS picks a free ephemeral port; the portal is told which
    // one to redirect to, so nothing has to be hard-coded.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let code_verifier = crate::store::new_hex(64);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let challenge = b64url(&digest);
    let (_key, private_jwk, public_jwk) = DpopKey::generate()?;
    let ticket_id = crate::store::new_hex(32);
    let url = login_url(port, &challenge, &ticket_id);

    let result: Arc<AsyncMutex<Option<Result<MaodaoCreds, String>>>> = Arc::new(AsyncMutex::new(None));
    let (tx, rx) = watch::channel(MaodaoLoginStatus::Pending);
    let context = MaodaoLoginContext {
        code_verifier,
        dpop_private: private_jwk.clone(),
        ticket_id,
        redirect_port: port,
        result: Arc::clone(&result),
        tx: Arc::new(tx),
    };

    tauri::async_runtime::spawn(async move {
        let app = Router::new()
            .route(AUTH_REDIRECT_PATH, get(callback_handler))
            .with_state(context);
        let server = axum::serve(listener, app);
        // The callback handler sends the status directly; the timer covers the
        // case where the browser never calls back (cancel / timeout).
        tokio::select! {
            _ = server => {}
            _ = tokio::time::sleep(Duration::from_millis(LOGIN_TIMEOUT_MS)) => {}
        }
    });

    Ok(MaodaoLogin {
        url,
        started_at_ms: now_ms(),
        dpop_private: private_jwk,
        dpop_public: public_jwk,
        result,
        rx,
    })
}

async fn callback_handler(
    State(context): State<MaodaoLoginContext>,
    Query(params): Query<BTreeMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let outcome: Result<MaodaoCreds, String> = if let Some(error) = params.get("error") {
        Err(params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone()))
    } else if let Some(code) = params.get("code") {
        match DpopKey::from_private(&context.dpop_private) {
            Ok(dpop) => exchange_code(code, &context.code_verifier, &dpop, context.redirect_port).await,
            Err(e) => Err(e),
        }
    } else if let Some(secret) = params.get("secret") {
        match DpopKey::from_private(&context.dpop_private) {
            Ok(dpop) => exchange_ticket(&context.ticket_id, secret, &dpop).await,
            Err(e) => Err(e),
        }
    } else {
        Err("回调缺少授权码".to_string())
    };

    let page = match &outcome {
        Ok(_) => SUCCESS_PAGE.to_string(),
        Err(message) => format!(
            "{}<pre style='color:#b91c1c'>{}</pre>{}</body>",
            ERROR_PAGE_HEAD,
            message.replace('<', "&lt;"),
            ERROR_PAGE_TAIL
        ),
    };
    let status = if outcome.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };

    let mut guard = context.result.lock().await;
    // Only the first callback counts; later reloads cannot re-bind the PKCE
    // verifier (a code is single-use anyway).
    let first = guard.is_none();
    if first {
        *guard = Some(outcome);
    }
    let announced = guard.as_ref().map(|res| match res {
        Ok(_) => MaodaoLoginStatus::Success,
        Err(message) => MaodaoLoginStatus::Failed(message.clone()),
    });
    drop(guard);
    if let Some(status) = announced {
        let _ = context.tx.send(status);
    }

    (status, Html(page))
}

const SUCCESS_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>登录成功</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>登录成功</h1><p>可以关闭这个页面，回到 CreditFerry 面板。</p>"#;

const ERROR_PAGE_HEAD: &str = r#"<!doctype html><meta charset="utf-8"><title>登录失败</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>登录失败</h1><p>原因：</p>"#;

const ERROR_PAGE_TAIL: &str = "<p>可以关闭这个页面。</p>";

// ------------------------------------------------------- client session import

/// Root of the CodeArts desktop client's user data.
fn client_root() -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let candidate = Path::new(&appdata).join("codearts");
    if candidate.is_dir() {
        return Some(candidate);
    }
    }
    None
}

/// Is the CodeArts desktop client installed (so "从客户端导入" makes sense)?
pub fn client_installed() -> bool {
    client_root().is_some()
}

/// Every encrypted secret blob stored by CodeArts.
fn secret_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(root) = client_root() {
        let dir = root.join("secretStorage");
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "enc").unwrap_or(false) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// Recover the AES-256-GCM key Chromium/Electron keeps DPAPI-wrapped.
fn os_crypt_key(local_state: &Path) -> Result<[u8; 32], String> {
    let raw = fs::read_to_string(local_state).map_err(|e| format!("读取 Local State 失败：{}", e))?;
    let document: Value =
        serde_json::from_str(&raw).map_err(|_| "Local State 不是合法 JSON".to_string())?;
    let encoded = document
        .pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("encrypted_key base64 解码失败：{}", e))?;
    let blob = if bytes.starts_with(b"DPAPI") {
        &bytes[5..]
    } else {
        &bytes[..]
    };
    let plain = dpapi_unprotect(blob)?;
    if plain.len() != 32 {
        return Err(format!("主密钥长度异常：{}", plain.len()));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&plain);
    Ok(key)
}

/// Decrypt one Chromium OSCrypt v10 blob: `v10` + nonce(12) + ciphertext + tag(16).
fn os_crypt_decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};

    if blob.len() < 3 + 12 + 16 {
        return Err("OSCrypt blob 过短".to_string());
    }
    if &blob[..3] != b"v10" {
        return Err("不是 OSCrypt v10 格式".to_string());
    }
    let nonce = &blob[3..15];
    let split = blob.len() - 16;
    let (ct, _tag) = blob[15..].split_at(split - 15);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "AES 密钥无效".to_string())?;
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: b"" })
        .map_err(|_| "OSCrypt 解密失败（主密钥不匹配？）".to_string())
}

/// Unwrap DPAPI (CryptUnprotectData) using the current Windows account.
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();

        // windows 0.58: Result<()> out-param form; free pbData with LocalFree.
        unsafe {
            CryptUnprotectData(
                &mut in_blob,
                None,
                None,
                None,
                None,
                0x01, // CRYPTPROTECT_UI_FORBIDDEN
                &mut out_blob,
            )
        }
        .map_err(|_| "DPAPI 解密失败".to_string())?;

        let out = if out_blob.pbData.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec()
        };
        if !out_blob.pbData.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
                    out_blob.pbData as *mut core::ffi::c_void,
                ));
            }
        }
        Ok(out)
    }

    #[cfg(not(windows))]
    {
        Err("码道凭据解密仅支持 Windows".to_string())
    }
}

/// A raw session as stored by the CodeArts client (camelCase keys).
/// Fields the ferry does not consume are kept so the struct mirrors the
/// client blob and future imports do not silently drop data.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MaodaoSession {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub domain_id: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub security_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub station_type: String,
    /// DPoP private JWK (JSON string) — required, the RT is jkt-bound to it.
    #[serde(default)]
    pub dpop_private_key: String,
    #[serde(default)]
    pub dpop_public_key: String,
}

impl MaodaoSession {
    /// Convert to the credentials shape we persist.
    pub fn to_creds(&self) -> MaodaoCreds {
        MaodaoCreds {
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            security_token: self.security_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: if self.expires_at > 0 {
                self.expires_at
            } else {
                now_ms() + 3600 * 1000
            },
            dpop_private: self.dpop_private_key.clone(),
            dpop_public: self.dpop_public_key.clone(),
            user_name: self.user_name.clone(),
        }
    }
}

/// Scan the installed CodeArts client for a usable session (read-only).
///
/// A blob only counts when it carries BOTH a refresh token and the DPoP private
/// key: the refresh token is jkt-bound, so without the key it can never be
/// rotated and the session would be dead on arrival.
pub fn import_client_sessions() -> Vec<MaodaoSession> {
    let root = match client_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let key = match os_crypt_key(&root.join("Local State")) {
        Ok(k) => k,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for path in secret_files() {
        let raw = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let blob = match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let plain = match os_crypt_decrypt(&key, &blob) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let text = String::from_utf8_lossy(&plain).to_string();
        let document: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session_value = match document.get("session") {
            Some(v) => v.clone(),
            None => continue,
        };
        let mut parsed: MaodaoSession = match serde_json::from_value(session_value) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let context = document.get("loginContext");
        let private = context
            .and_then(|c| c.pointer("/dpopKeyPair/privateKey"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let public = context
            .and_then(|c| c.pointer("/dpopKeyPair/publicKey"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if parsed.refresh_token.is_empty() || private.is_empty() {
    continue;
        }
        parsed.dpop_private_key = private;
        parsed.dpop_public_key = public;
        out.push(parsed);
    }
    out
}

// ------------------------------------------------------------------ helpers

fn text_field(value: &Value, keys: &[&str]) -> String {
    for key in keys {
    if let Some(text) = value.get(*key).and_then(|v| v.as_str()) {
        if !text.is_empty() {
    return text.to_string();
        }
    }
    }
    String::new()
}

fn num_field(value: &Value, keys: &[&str]) -> u64 {
    for key in keys {
    if let Some(number) = value.get(*key) {
        if let Some(number) = number.as_u64() {
    return number;
        }
        if let Some(number) = number.as_f64() {
    if number > 0.0 {
    return number as u64;
    }
        }
    }
    }
    0
}

fn first_num(value: &Value, paths: &[&str]) -> Option<f64> {
    for path in paths {
    if let Some(found) = value.pointer(path) {
        if let Some(number) = found.as_f64() {
    return Some(number);
        }
        if let Some(number) = found.as_u64() {
    return Some(number as f64);
        }
    }
    }
    None
}

/// Keep an upstream error note short and readable.
pub fn short(text: &str) -> String {
    for marker in ["\"message\":\"", "\"error_msg\":\""] {
    if let Some(start) = text.find(marker) {
        let rest = &text[start + marker.len()..];
        if let Some(end) = rest.find('"') {
    return rest[..end].chars().take(120).collect();
        }
    }
    }
    let cleaned = text.replace('\n', " ");
    cleaned.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_uri_always_trails() {
        assert_eq!(
            canonical_uri("/api/v1/gateway/config"),
            "/api/v1/gateway/config/"
        );
        assert_eq!(canonical_uri("/"), "/");
        assert_eq!(canonical_uri("/a b/c"), "/a%20b/c/");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query("channel=edu&x=a b"), "channel=edu&x=a%20b");
    }

    #[test]
    fn split_url_handles_query() {
        let (host, path, query) = split_url("https://a.b.com/x/y?channel=edu").expect("parse");
        assert_eq!(host, "a.b.com");
        assert_eq!(path, "/x/y");
        assert_eq!(query, "channel=edu");
    }

    #[test]
    fn dpop_proof_is_three_parts() {
        let (key, private, public) = DpopKey::generate().expect("keypair");
        assert!(private.contains("\"d\""));
        assert!(public.contains("\"x\""));
        let proof = key.proof("POST", "https://example.com/token").expect("proof");
        assert_eq!(proof.split('.').count(), 3);
    }

    #[test]
    fn parses_rfc3339_expiration() {
        assert!(parse_rfc3339_ms("2026-10-14T17:57:34.195Z").expect("date") > 0);
    }
}
