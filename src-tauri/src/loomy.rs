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

use base64::Engine as _;
use md5::{Digest, Md5};
use reqwest::header::{HeaderMap, HeaderValue};
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

// ————————————————————————————————————————————————————————————————————
// 微信扫码登录
//
// 协议规格逐条对照 OFM business.mjs：
// - L47204-47205：微信开放平台 appid / redirect_uri
// - L47223-47241：授权页 URL、uuid 提取正则、二维码图片 URL
// - L47291-47317：长轮询 `long.open.weixin.qq.com/connect/l/qrconnect`
//   状态梯：405=已确认（code 就在这一帧）/ 404=已扫码待确认（继续轮询）/
//   403=取消 / 402=二维码过期 / 其他=等待扫码
// - L47137-47156：POST `/login/thirdAccount/bind/auth`（微信 code → rcode）
// - L47185-47197：POST `/login/thirdAccount/bind/skip`（rcode → session）
// - L47157-47183：`bind/sendMsg` + `bind/checkCode`（首次扫码需绑手机号）
// - L47038-47072：账号网关签名头 + {base, param} 请求体
// ————————————————————————————————————————————————————————————————————

/// 讯飞账号网关（business.mjs L42199 `LOOMY_ACCOUNT_BASE`）。
pub const ACCOUNT_BASE: &str = "https://account.xfinfr.com";
/// 取自 `.env.prod`（VITE_XFYUN_ACCESS_KEY_ID / _SECRET / _APP_ID），见 L42286-42288。
pub const ACCESS_KEY_ID: &str = "2thryby66wxi53sk";
pub const ACCESS_KEY_SECRET: &str = "zsak6eadrbawz683wf5r3m2snrwj868r";
pub const APP_ID: &str = "GM3LOOMY";
pub const WECHAT_APP_ID: &str = "wx18d60be432287cf8";
pub const WECHAT_REDIRECT_URI: &str = "https://loomy.xunfei.cn/oauth/wechat/callback";

pub const WECHAT_QR_PATH: &str = "/wechat/qr";
pub const WECHAT_POLL_PATH: &str = "/wechat/poll";
pub const WECHAT_COMPLETE_PATH: &str = "/wechat/complete";

const BIND_AUTH_PATH: &str = "/login/thirdAccount/bind/auth";
const BIND_SKIP_PATH: &str = "/login/thirdAccount/bind/skip";
const BIND_SENDMSG_PATH: &str = "/login/thirdAccount/bind/sendMsg";
const BIND_CHECKCODE_PATH: &str = "/login/thirdAccount/bind/checkCode";

/// 账号网关成功业务码（business.mjs L42200 `LOOMY_OK_CODE`）。
const OK_CODE: &str = "000000";
const WECHAT_POLL_TIMEOUT_MS: u64 = 40_000;
pub const SESSION_TTL_SECONDS: i64 = 1_209_600;
const SMS_CODE_TTL_SECONDS: i64 = 300;
pub const WECHAT_LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// ⚠️ 微信开放平台要求桌面 UA，见 business.mjs L47221。
const WECHAT_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 轮询状态（与 business.mjs L47207 `LOOMY_WECHAT_POLL_STATUS` 对齐）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WechatPollStatus {
    Waiting,
    Scanned,
    Confirmed,
    Cancelled,
    Expired,
    Error,
}

impl WechatPollStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Scanned => "scanned",
            Self::Confirmed => "confirmed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Error => "error",
        }
    }
}

/// 一次长轮询的结果。
pub struct WechatPoll {
    pub status: WechatPollStatus,
    /// 仅在 `Confirmed` 时有值。
    pub code: String,
    pub errcode: String,
}

/// 微信开放平台扫码授权页（business.mjs L47223）。
/// 微信扫码登录用的 HTTP 客户端（直连微信开放平台，不复用转发口）。
pub fn wechat_client() -> Client {
    Client::builder()
        .timeout(Duration::from_millis(WECHAT_POLL_TIMEOUT_MS + 5_000))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// 微信开放平台扫码授权页（business.mjs L47223）。
pub fn wechat_auth_url(state: &str) -> String {
    format!(
        "https://open.weixin.qq.com/connect/qrconnect?appid={}&redirect_uri={}&response_type=code&scope=snsapi_login&state={}#wechat_redirect",
        pct(WECHAT_APP_ID),
        pct(WECHAT_REDIRECT_URI),
        pct(state)
    )
}

/// 二维码图片 URL（business.mjs L47240）。
pub fn wechat_qr_image_url(uuid: &str) -> String {
    format!("https://open.weixin.qq.com/connect/qrcode/{}", pct(uuid))
}

/// 长轮询 URL（business.mjs L47291）。
pub fn wechat_poll_url(uuid: &str, last_errcode: &str) -> String {
    let mut q = format!("uuid={}", pct(uuid));
    if !last_errcode.is_empty() {
        q.push_str(&format!("&last={}", pct(last_errcode)));
    }
    q.push_str(&format!("&_={}", now_ms()));
    format!("https://long.open.weixin.qq.com/connect/l/qrconnect?{}", q)
}

/// uuid 字符集（business.mjs L47222 `UUID_PATTERN`）。
fn is_uuid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/')
}

/// 从授权页 HTML 抽取 uuid：先试 `/connect/qrcode/<uuid>`，再试 `l/qrconnect?uuid=<uuid>`。
pub fn extract_wechat_uuid(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    for marker in ["/connect/qrcode/", "l/qrconnect?uuid="] {
        let mut search = html;
        while let Some(idx) = search.find(marker) {
            let rest = &search[idx + marker.len()..];
            let end = rest.find(|c: char| !is_uuid_char(c)).unwrap_or(rest.len());
            let candidate = &rest[..end];
            if candidate.len() >= 6 && candidate.len() <= 64 {
                return candidate.to_string();
            }
            search = &rest[end.min(rest.len())..];
            if search.is_empty() {
                break;
            }
        }
    }
    String::new()
}

/// Percent-encode（RFC 3986 unreserved 保留，其余转义；与 business.mjs
/// `escapeRfc3986` L47003 等价）。
fn pct(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// body 的 MD5（base64）；空 body 返回空串（business.mjs L47000）。
fn content_md5(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let digest = Md5::digest(body.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(&digest[..])
}

/// 待签字符串：method / path / query / md5 / content-type / date / nonce /
/// signedHeaders("") / canonicalizedHeaders("")（business.mjs L47017）。
fn build_signing_string(
    method: &str,
    path: &str,
    query: &str,
    md5: &str,
    content_type: &str,
    date: &str,
    nonce: &str,
) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n\n",
        method.to_uppercase(),
        path,
        query,
        md5,
        content_type,
        date,
        nonce
    )
}

/// 账号网关认证头：`Authorization: account {ak}:{base64(hmac_sha1(secret, s2s))}`，
/// 另带 Date / Nonce / Content-MD5（business.mjs L47038）。
pub fn account_headers(method: &str, path: &str, body: &str) -> Result<HeaderMap, String> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    let content_type = "application/json";
    let md5 = content_md5(body);
    let s2s = build_signing_string(method, path, "", &md5, content_type, &date, &nonce);
    let mut mac = Hmac::<Sha1>::new_from_slice(ACCESS_KEY_SECRET.as_bytes())
        .map_err(|e| format!("loomy hmac init: {}", e))?;
    mac.update(s2s.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let mut h = HeaderMap::new();
    h.insert(
        reqwest::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("account {}:{}", ACCESS_KEY_ID, sig))
            .map_err(|_| "loomy 认证头非法".to_string())?,
    );
    h.insert(
        reqwest::header::DATE,
        HeaderValue::from_str(&date).map_err(|_| "loomy Date 非法".to_string())?,
    );
    h.insert(
        "Nonce",
        HeaderValue::from_str(&nonce).map_err(|_| "loomy Nonce 非法".to_string())?,
    );
    h.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    if !md5.is_empty() {
        h.insert(
            "Content-MD5",
            HeaderValue::from_str(&md5).map_err(|_| "loomy Content-MD5 非法".to_string())?,
        );
    }
    Ok(h)
}

/// 账号网关请求体（business.mjs L47059）：`{ base: {...}, param }`。
fn account_body(param: Value) -> Value {
    json!({
        "base": {
            "appid": APP_ID,
            "modelid": "Web",
            "version": "1.0.0",
            "devid": "web",
            // ⚠️ 硬编码 macOS：客户端在 Windows 上发的也是这个值，照抄不要改。
            "ua": "Loomy|Desktop|Electron|macOS",
            "traceid": crate::store::new_hex(16),
        },
        "param": param,
    })
}

/// POST 账号网关并解包 `{code, desc, data}`；`code !== "000000"` 即报错。
pub async fn post_account(http: &Client, path: &str, param: Value) -> Result<Value, String> {
    let body = account_body(param);
    let payload = serde_json::to_string(&body).map_err(|e| format!("loomy 序列化：{}", e))?;
    let headers = account_headers("POST", path, &payload)?;
    let resp = http
        .post(format!("{}{}", ACCOUNT_BASE, path))
        .headers(headers)
        .body(payload)
        .send()
        .await
        .map_err(|e| format!("loomy {} 请求失败：{}", path, e))?;
    let status = resp.status();
    let value: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({ "code": "HTTP_ERROR", "desc": format!("HTTP {}", status) }));
    let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code != OK_CODE {
        let desc = value
            .get("desc")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("未知错误");
        return Err(format!("{}（{}）", desc, code));
    }
    Ok(value.get("data").cloned().unwrap_or(json!({})))
}

/// 第 1 步：拉授权页并抽 uuid（business.mjs L47243）。
pub async fn fetch_wechat_uuid(http: &Client) -> Result<String, String> {
    let resp = http
        .get(wechat_auth_url(&crate::store::new_hex(16)))
        .header("User-Agent", WECHAT_UA)
        .header("Referer", "https://open.weixin.qq.com/")
        .send()
        .await
        .map_err(|e| format!("微信授权页拉取失败：{}", e))?;
    if !resp.status().is_success() {
        return Err(format!("微信授权页返回 HTTP {}", resp.status()));
    }
    let html = resp.text().await.map_err(|e| format!("微信授权页读取失败：{}", e))?;
    let uuid = extract_wechat_uuid(&html);
    if uuid.is_empty() {
        return Err("微信授权页未包含二维码 uuid（页面结构可能已变化）".into());
    }
    Ok(uuid)
}

/// 二维码图片的 data URL，供面板直接内嵌显示。
pub async fn fetch_wechat_qr_data_url(http: &Client, uuid: &str) -> Result<String, String> {
    let resp = http
        .get(wechat_qr_image_url(uuid))
        .header("User-Agent", WECHAT_UA)
        .header("Referer", "https://open.weixin.qq.com/")
        .send()
        .await
        .map_err(|e| format!("微信二维码下载失败：{}", e))?;
    if !resp.status().is_success() {
        return Err(format!("微信二维码返回 HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("微信二维码读取失败：{}", e))?;
    if bytes.len() < 200 {
        return Err("微信二维码响应不是图片（可能是错误页）".into());
    }
    let mime = detect_image_mime(&bytes);
    if mime.is_empty() {
        return Err("微信二维码图片格式无法识别".into());
    }
    use base64::Engine as _;
    Ok(format!("data:{};base64,{}", mime, base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return "image/jpeg";
    }
    if bytes.len() >= 4 && bytes[0] == 0x89 && bytes[1] == b'P' && bytes[2] == b'N' && bytes[3] == b'G' {
        return "image/png";
    }
    if bytes.len() >= 3 && bytes[0] == b'G' && bytes[1] == b'I' && bytes[2] == b'F' {
        return "image/gif";
    }
    ""
}

/// 长轮询一次（business.mjs L47291）。
pub async fn poll_wechat_once(
    http: &Client,
    uuid: &str,
    last_errcode: &str,
) -> WechatPoll {
    let url = wechat_poll_url(uuid, last_errcode);
    let resp = match http
        .get(&url)
        .header("User-Agent", WECHAT_UA)
        .header("Referer", wechat_auth_url(""))
        .timeout(Duration::from_millis(WECHAT_POLL_TIMEOUT_MS))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return WechatPoll {
                status: WechatPollStatus::Error,
                code: String::new(),
                errcode: e.to_string(),
            }
        }
    };
    let body = resp.text().await.unwrap_or_default();
    let errcode = find_group(&body, r"wx_errcode\s*=\s*(\d+)");
    let code = find_group_single(&body, r"wx_code\s*=\s*'([^']*)'");
    match errcode.as_str() {
        "405" => WechatPoll {
            status: if code.is_empty() {
                WechatPollStatus::Scanned
            } else {
                WechatPollStatus::Confirmed
            },
            code,
            errcode,
        },
        "404" => WechatPoll { status: WechatPollStatus::Scanned, code: String::new(), errcode },
        "403" => WechatPoll { status: WechatPollStatus::Cancelled, code: String::new(), errcode },
        "402" => WechatPoll { status: WechatPollStatus::Expired, code: String::new(), errcode },
        _ => WechatPoll { status: WechatPollStatus::Waiting, code: String::new(), errcode },
    }
}

/// 极简正则：取第一个捕获组（仅支持 `\d+` / `[^']*` / 字符集形式）。
fn find_group(text: &str, pattern: &str) -> String {
    // 形如 `wx_errcode\s*=\s*(\d+)`，这里按「前缀 + 捕获字符集」处理。
    let Some(prefix_end) = pattern.find('(') else {
        return String::new();
    };
    let prefix = &pattern[..prefix_end];
    let class = pattern[prefix_end..]
        .trim_start_matches('(')
        .trim_end_matches("+)")
        .trim_end_matches("*]")
        .trim_end_matches(')');
    let prefix_re = prefix.replace("\\s*", "").replace("\\s+", "");
    if let Some(idx) = text.find(&prefix_re) {
        let rest = &text[idx + prefix_re.len()..];
        let end = rest
            .find(|c: char| !matches_class(c, class))
            .unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    String::new()
}

fn matches_class(c: char, class: &str) -> bool {
    match class {
        "\\d" => c.is_ascii_digit(),
        _ => c != '\'',
    }
}

fn find_group_single(text: &str, pattern: &str) -> String {
    if let Some(idx) = text.find("wx_code") {
        let rest = &text[idx..];
        if let Some(start) = rest.find('\'') {
            let inner = &rest[start + 1..];
            if let Some(end) = inner.find('\'') {
                return inner[..end].to_string();
            }
        }
    }
    let _ = pattern;
    String::new()
}

/// 扫码状态机（对应 business.mjs `startLoomyWechatLoginFlow` L47324）。
pub enum WechatLoginState {
    /// 展示二维码，等待扫码。
    Waiting,
    /// 已扫码，等待手机端确认。
    Scanned,
    /// 登录成功，返回会话凭证。
    Done(LoomyLogin),
    /// 需要绑定手机号（首次扫码的新微信）。
    NeedPhone { rcode: String },
    /// 用户取消。
    Cancelled,
    /// 二维码失效，需重新获取。
    Expired,
    /// 失败原因。
    Failed(String),
}

/// 登录成功的会话凭证。
#[derive(Clone, Debug)]
pub struct LoomyLogin {
    pub session: String,
    pub userid: String,
    pub phone: String,
    pub nickname: String,
}

/// 用微信 code 换 rcode（business.mjs L47137）。
pub async fn bind_third_account(http: &Client, code: &str) -> Result<Value, String> {
    post_account(
        http,
        BIND_AUTH_PATH,
        json!({ "tcode": { "code": code }, "type": "wx" }),
    )
    .await
}

/// rcode → session（business.mjs L47185）。
pub async fn bind_skip(http: &Client, rcode: &str) -> Result<LoomyLogin, String> {
    let data = post_account(
        http,
        BIND_SKIP_PATH,
        json!({ "rcode": rcode, "expire": SESSION_TTL_SECONDS }),
    )
    .await?;
    let session = data.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if session.is_empty() {
        return Err("Loomy 绑定响应缺少 session".into());
    }
    Ok(LoomyLogin {
        session,
        userid: data.get("userid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        phone: data.get("phone").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        nickname: data
            .get("nickname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 首次扫码的新微信需要绑手机号：下发短信（business.mjs L47157）。
pub async fn bind_send_msg(
    http: &Client,
    rcode: &str,
    phone: &str,
) -> Result<String, String> {
    let data = post_account(
        http,
        BIND_SENDMSG_PATH,
        json!({ "rcode": rcode, "phone": phone, "ccode": "86", "expire": SMS_CODE_TTL_SECONDS }),
    )
    .await?;
    let msgid = data.get("msgid").map(|v| v.to_string()).unwrap_or_default();
    if msgid.trim_matches('"').is_empty() {
        return Err("绑定手机号响应缺少 msgid".into());
    }
    Ok(msgid.trim_matches('"').to_string())
}

/// 校验短信码并完成绑定（business.mjs L47169）。
pub async fn bind_check_code(
    http: &Client,
    rcode: &str,
    mcode: &str,
    msgid: &str,
) -> Result<LoomyLogin, String> {
    let data = post_account(
        http,
        BIND_CHECKCODE_PATH,
        json!({ "rcode": rcode, "mcode": mcode, "msgid": msgid, "expire": SESSION_TTL_SECONDS }),
    )
    .await?;
    let session = data.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if session.is_empty() {
        return Err("绑定登录响应缺少 session".into());
    }
    Ok(LoomyLogin {
        session,
        userid: data.get("userid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        phone: data.get("phone").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        nickname: data
            .get("nickname")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}
