//! The local OpenAI-compatible forwarding surface (127.0.0.1 only).
//!
//! Mirrors the fences that made OC Ferry trustworthy:
//! - Bearer key with a constant-time compare; a missing/wrong key is 401.
//! - Any request carrying an `Origin` header is 403 (browsers stay out).
//! - Request bodies capped at 8 MB (413); concurrent forwards are queued.
//! - Model ids: the client-visible `Loomy · <name>` / `Raccoon · <name>`
//!   and the upstream ids (`loomy/<id>` / `raccoon/<id>`) are both accepted.
//! - SSE is passed through byte-for-byte; the stream is teed so the final
//!   `usage` chunk (Loomy reports `points_consumed`) lands in the ledger.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};

use crate::buddy::{self, BuddyClient};
use crate::cline::ClineClient;
use crate::kilo::KiloClient;
use crate::loomy::LoomyClient;
use crate::lobsterai::LobsterClient;
use crate::maodao::{self, MaodaoClient, MaodaoLogin};
use crate::minimax::{self, MinimaxClient};
use crate::opencode::{OpenCodeClient, OpenCodeRuntime};
use crate::qoder::{self, QoderClient};
use crate::freebuff::FreebuffClient;
use crate::raccoon::{ensure_fresh, RaccoonClient};
use crate::store::{LedgerEntry, ModelEntry, Store};
use crate::trae::TraeClient;
use crate::zen::{self, ZenClient};

pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONCURRENCY: usize = 16;
/// Maodao allows three concurrent sessions per account and its own desktop
/// client holds some of them, so ferry traffic is serialised: a queued request
/// waits, a fourth one would come back as `并发会话数已达上限(3个)`.
pub const MAODAO_CONCURRENCY: usize = 1;

pub struct AppState {
    pub store: Mutex<Store>,
    pub loomy: LoomyClient,
    pub raccoon: RaccoonClient,
    pub opencode: OpenCodeClient,
    pub opencode_runtime: Mutex<Option<OpenCodeRuntime>>,
    /// Kilo free pool — keyless, so no credential lives beside the client.
    pub kilo: KiloClient,
    /// Zen free lane — anonymous gateway, per-conversation stable sessions.
    pub zen: ZenClient,
    /// seed (first-message digest) → minted session id; persisted to
    /// `zen-sessions.json` (quota is accounted per session, so a fresh mint
    /// per request burns the pool and surfaces as 429 FreeUsageLimitError).
    pub zen_sessions: Mutex<HashMap<String, String>>,
    /// CodeBuddy / WorkBuddy native OAuth channels (buddy.rs).
    pub buddy: BuddyClient,
    /// LobsterAI (有道) native channel (lobsterai.rs).
    pub lobsterai: LobsterClient,
    /// TRAE (字节) native channel (trae.rs).
    pub trae: TraeClient,
    /// MiniMax Code native channel (minimax.rs).
    pub minimax: MinimaxClient,
    /// Cline native channel (cline.rs) — fully usable.
    pub cline: ClineClient,
    /// Qoder / Qoder CN (qoder.rs) — login + quota only; inference gated.
    pub qoder: QoderClient,
    /// Freebuff / CodeBuff (freebuff.rs) — 登录态导入 + 目录 + 每日 freebucks
    /// 额度展示；免费档推理被上游强门控到官方 CLI，chat 不转发（gated）。
    pub freebuff: FreebuffClient,
    pub maodao: MaodaoClient,
    /// In-flight browser login for the Maodao upstream.
    pub maodao_login: Mutex<Option<MaodaoLogin>>,
    pub key: String,
    /// 额外接入密钥的明文副本（api-keys.json 非 primary 条目）；与主密钥
    /// 同样可过转发口鉴权。增删经 commands 层同步，guard 只读。
    pub extra_keys: std::sync::Mutex<Vec<String>>,
    pub sem: Arc<Semaphore>,
    /// Maodao counts concurrent sessions per account and rejects the fourth
    /// (`并发会话数已达上限(3个)`), so its calls are queued separately from the
    /// global limit — an agent client fires several requests per turn.
    pub maodao_sem: Arc<Semaphore>,
    /// P1-3 keepalive gate. Maodao STS and Raccoon refresh tokens are
    /// **single-use** (each rotation consumes the old RT): two concurrent
    /// rotations guarantee one side burns a dead token and can trip the
    /// upstream's terminal revocation (OFM CodeArts 401 storm). Every path
    /// that rotates a token — the keepalive loop, chat forwarding, and
    /// probes — must hold this gate across ensure_fresh. Lock order:
    /// acquire the gate only with the store lock NOT held; the gate is held
    /// while briefly taking the store lock (never the reverse).
    pub refresh_gate: Mutex<()>,
    /// P1-3: set once the keepalive loop has been spawned (compare_exchange
    /// guard so the loop is never started twice by different triggers).
    pub keepalive_started: std::sync::atomic::AtomicBool,
    /// 统一验证（全渠道串行重探）互斥：重复点击不叠跑。
    pub verify_all_busy: std::sync::atomic::AtomicBool,
    /// 转发口实际在监听时为 true（serve() bind 成功置位、退出/被 abort 清零）。
    /// 快照直接透出，省掉前端每轮单独一次 server_status IPC。
    pub server_running: std::sync::atomic::AtomicBool,
}

/// What the upstreams hand back after a chat call.
enum Upstream {
    /// A raw upstream HTTP response to pass through (Loomy / Raccoon / Maodao / Kilo).
    Http(reqwest::Response),
    /// An SSE response that must be folded back into one JSON completion —
    /// the Zen lane answers 403 FreeTierError for `stream:false` requests.
    HttpFoldSse(reqwest::Response),
    /// An Anthropic-protocol SSE stream (MiniMax) that must be translated to
    /// OpenAI SSE on the fly.
    HttpTranslate(reqwest::Response),
    /// An already-translated OpenAI-shaped completion (OpenCode / MiniMax).
    Json(Value),
}

/// Constant-time byte comparison.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn authorized(headers: &HeaderMap, key: &str) -> bool {
    let expected = format!("Bearer {}", key);
    let actual = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ct_eq(actual.as_bytes(), expected.as_bytes())
}

/// 额外密钥在面板里只回显掩码（保留尾 4 位可辨认）；完整明文仅在「新增」
/// 命令的返回里出现一次，由用户当场复制。
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••••••{}", tail)
}

/// Shared request guard: bearer key first, then the browser-origin fence.
/// Returns `Some(error response)` when the request must be refused.
/// 主密钥或任一额外密钥（面板「接入密钥」管理新增）都放行。
fn guard(headers: &HeaderMap, state: &AppState) -> Option<Response> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");
    let ok_main = authorized(headers, &state.key);
    let ok_extra = !token.is_empty()
        && state
            .extra_keys
            .lock()
            .map(|k| k.iter().any(|k| k == token))
            .unwrap_or(false);
    if !(ok_main || ok_extra) {
        return Some(json_error(StatusCode::UNAUTHORIZED, "Local proxy API key required"));
    }
    if !origin_allowed(headers) {
        return Some(json_error(StatusCode::FORBIDDEN, "Browser-origin requests are disabled"));
    }
    None
}

/// Outermost diagnostic middleware: log EVERY request that actually reaches
/// the ferry (method, path, status, whether an Authorization header was
/// present, Origin, UA) to `%APPDATA%\CreditFerry\access.log`. If a client
/// reports "cannot connect" but nothing shows up here, the request never
/// reached 127.0.0.1:6666 at all — the usual culprit is a system/HTTP proxy
/// intercepting loopback traffic.
/// Bearer 头里密钥是否存在 + 前 8 位指纹（不可逆，仅用于区分"密钥错/没带/带对"）。
fn auth_hint(headers: &HeaderMap) -> String {
    match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        None => "none".into(),
        Some(v) => {
            let t = v.strip_prefix("Bearer ").unwrap_or(v);
            let head: String = t.chars().take(8).collect();
            format!("{}…(len={})", head, t.chars().count())
        }
    }
}

async fn request_log(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let auth = auth_hint(req.headers());
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let ua = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let res = next.run(req).await;
    let status = res.status().as_u16();
    if let Ok(appdata) = std::env::var("APPDATA") {
        use std::io::Write;
        let p = std::path::Path::new(&appdata)
            .join("CreditFerry")
            .join("access.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(
                f,
                "{} {} {} -> {} auth={} origin={} ua={}",
                chrono::Local::now(),
                method,
                path,
                status,
                auth,
                origin,
                ua
            );
        }
    }
    res
}

/// Origin fence, desktop-aware. Native clients (curl, Node main process)
/// send no `Origin` at all — allowed. Desktop Electron apps (WorkBuddy's
/// "test connection" runs in a renderer) always attach one, but its scheme
/// is a *local* one: `file://`, `app://`, `null`, or a loopback `http(s)`.
/// The server binds 127.0.0.1 only, so any process that can reach it is
/// already on this machine; the fence's real job is to reject a *public web
/// page* (an `http(s)` origin that is not loopback) driving the proxy — the
/// CSRF-style case. So: reject only `http(s)` origins whose host is not
/// loopback; everything else (no origin, null, file/app/data/blob, localhost)
/// passes.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let o = match headers.get("origin").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return true,
    };
    if o == "null" {
        return true;
    }
    // Only an http/https origin can be a remote web page; any other scheme
    // (file://, app://, …) is a local desktop renderer — allow it.
    let rest = match o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
    {
        Some(r) => r,
        None => return true,
    };
    let host = rest.split(|c| c == ':' || c == '/').next().unwrap_or("");
    matches!(host, "127.0.0.1" | "[::1]" | "localhost")
}

/// CORS for the loopback-origin case: a renderer fetch is cross-origin from
/// `http://127.0.0.1:6666`, so preflight (OPTIONS) and response headers are
/// required or Chromium blocks the call before it even reaches the fence.
/// Non-loopback origins get no CORS headers and still hit the 403 in `guard`.
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let local = origin_allowed(req.headers());
    if *req.method() == axum::http::Method::OPTIONS {
        if !local {
            return json_error(StatusCode::FORBIDDEN, "Browser-origin requests are disabled");
        }
        let acao = origin.clone().unwrap_or_else(|| "*".into());
        return (
            [
                ("Access-Control-Allow-Origin", acao),
                ("Access-Control-Allow-Methods", "GET, POST, OPTIONS".into()),
                ("Access-Control-Allow-Headers", "Authorization, Content-Type".into()),
                // Chromium ≥138 PNA preflight: a public https page probing a
                // loopback server must see this or the real request is blocked.
                ("Access-Control-Allow-Private-Network", "true".into()),
                ("Access-Control-Max-Age", "600".into()),
            ],
        )
            .into_response();
    }
    let mut res = next.run(req).await;
    if local {
        if let Some(o) = origin {
            if let Ok(v) = axum::http::HeaderValue::from_str(&o) {
                res.headers_mut().insert("Access-Control-Allow-Origin", v);
            }
        }
    }
    res
}

/// Client-visible model id, e.g. `Loomy · Spark X2.5`.
pub fn client_model_id(entry: &ModelEntry) -> String {
    let prefix = match entry.source.as_str() {
        "raccoon" => "Raccoon",
        "opencode" => "OC",
        "maodao" => "Maodao",
        "kilo" => "Kilo",
        "zen" => "Zen",
        "buddy" => "CodeBuddy",
        "workbuddy" => "WorkBuddy",
        "lobsterai" => "LobsterAI",
        "trae" => "TRAE",
        "minimax" => "MiniMax",
        "cline" => "Cline",
        "freebuff" => "Freebuff",
        _ => "Loomy",
    };
    format!("{} · {}", prefix, entry.name)
}

pub fn upstream_model_id(entry: &ModelEntry) -> String {
    format!("{}/{}", entry.source, entry.upstream_id)
}

/// The largest `max_tokens` the upstreams accept.
///
/// Catalogs report a model's theoretical output size (Maodao says 393216,
/// OpenCode says 524288), but the gateways do not honour it: Maodao answers
/// `max_tokens should be between 1 and 65536` and Loomy's nginx returns a bare
/// 502 past that. A client that trusts the export therefore fails on every
/// turn, so the ferry clamps instead and advertises the clamped number.
pub const MAX_TOKENS_CAP: u64 = 65_536;

/// Fit `max_tokens` to what the upstream will actually accept.
///
/// Two jobs. First, lower an oversized client value to the gateway cap (see
/// [`MAX_TOKENS_CAP`]). Second, when the client omits `max_tokens` entirely,
/// inject the model's own advertised output ceiling so reasoning models and
/// long answers are not silently truncated by whichever small default the
/// upstream gateway happens to pick. A `model_max_output` of 0 means the
/// catalog never told us, and the request is left alone for the upstream's
/// own default — inventing a number there would risk a 400 on strict gates.
pub fn clamp_max_tokens(body: &mut Value, model_max_output: u64) {
    let asked = body.get("max_tokens").and_then(|v| v.as_u64());
    match asked {
        Some(n) if n > MAX_TOKENS_CAP => body["max_tokens"] = Value::from(MAX_TOKENS_CAP),
        Some(_) => {}
        None => {
            if model_max_output > 0 {
                body["max_tokens"] = Value::from(model_max_output.min(MAX_TOKENS_CAP));
            }
        }
    }
}

/// Drop `image_url` parts from `messages` for text-only upstreams.
///
/// Loomy (iflytek) and Maodao reject image content with a 400/406 ("model is
/// not multimodal"), so when the resolved entry does not support vision we
/// strip every image segment before forwarding. A content list that becomes
/// empty after stripping is collapsed to a short text note so the request
/// still parses; non-array content (plain strings) is left untouched.
pub fn strip_image_content(body: &mut Value, keep_images: bool) {
    if keep_images {
        return;
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let content = match msg.get_mut("content") {
                Some(c) => c,
                None => continue,
            };
            let Some(items) = content.as_array_mut() else {
                continue;
            };
            items.retain(|item| {
                item.get("type").and_then(|t| t.as_str()) != Some("image_url")
            });
            if items.is_empty() {
                *content = Value::String("(图片已省略：该模型不支持多模态输入)".to_string());
            }
        }
    }
}

/// Resolve a requested model id (client-visible or upstream) to its entry.
pub async fn resolve_model(state: &AppState, requested: &str) -> Option<ModelEntry> {
    let store = state.store.lock().await;
    let candidates = store
        .models
        .loomy
        .iter()
        .chain(store.models.raccoon.iter())
        .chain(store.models.opencode.iter())
        .chain(store.models.maodao.iter())
        .chain(store.models.kilo.iter())
        .chain(store.models.zen.iter())
        .chain(store.models.buddy.iter())
        .chain(store.models.workbuddy.iter())
        .chain(store.models.lobsterai.iter())
        .chain(store.models.trae.iter())
        .chain(store.models.minimax.iter())
        .chain(store.models.cline.iter());
    let mut found = None;
    for entry in candidates {
        if client_model_id(entry) == requested || upstream_model_id(entry) == requested {
            // A model hidden by a fresh failing verdict stays unroutable.
            if !store.models.publishable(&entry.source, &entry.upstream_id) {
                return None;
            }
            found = Some(entry.clone());
            break;
        }
    }
    found
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models).options(preflight))
        .route("/v1/chat/completions", post(chat).options(preflight))
        .layer(axum::middleware::from_fn(cors))
        // Registered last so it wraps everything (cors preflight included) —
        // the outermost witness of "did the request even reach the ferry".
        .layer(axum::middleware::from_fn(request_log))
        .with_state(state)
}

async fn preflight() -> Response {
    // The cors middleware answers OPTIONS before this ever runs; kept as a
    // belt-and-braces 204 for any route added without the middleware.
    StatusCode::NO_CONTENT.into_response()
}

async fn health(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(err) = guard(&headers, &state) {
        return err;
    }
    let snapshot = build_snapshot(&state).await;
    Json(snapshot).into_response()
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(err) = guard(&headers, &state) {
        return err;
    }
    let store = state.store.lock().await;
    let mut data = Vec::new();
    for entry in store
        .models
        .loomy
        .iter()
        .chain(store.models.raccoon.iter())
        .chain(store.models.opencode.iter())
        .chain(store.models.maodao.iter())
        .chain(store.models.kilo.iter())
        .chain(store.models.zen.iter())
        .chain(store.models.buddy.iter())
        .chain(store.models.workbuddy.iter())
        .chain(store.models.lobsterai.iter())
        .chain(store.models.trae.iter())
        .chain(store.models.minimax.iter())
        .chain(store.models.cline.iter())
    {
        // A fresh failing verdict hides the model from clients.
        if !store.models.publishable(&entry.source, &entry.upstream_id) {
            continue;
        }
        data.push(json!({
            "id": client_model_id(entry),
            "object": "model",
            "owned_by": entry.source,
            "name": client_model_id(entry),
        }));
    }
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn chat(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    if let Some(err) = guard(&headers, &state) {
        return err;
    }
    // Wait for a slot rather than rejecting: an agent client fires a whole
    // batch of requests per turn and a 429 reads to the user as "model broken".
    let _permit = match state.sem.acquire().await {
        Ok(p) => p,
        Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "转发器已关闭"),
    };

    // Read the body with the 8 MB cap.
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Request exceeds 8 MB"),
    };
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };
    let requested = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_owned();
    let entry = match resolve_model(&state, &requested).await {
        Some(e) => e,
        None => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Select an available model from /v1/models",
            )
        }
    };

    let stream = payload.get("stream") == Some(&Value::Bool(true));
    // Rewrite the model id to the upstream id the gateway expects. The body
    // moves into `upstream_body` (no deep clone): agent payloads carry whole
    // conversation histories and used to be copied in full on every turn.
    let mut upstream_body = payload;
    if let Some(obj) = upstream_body.as_object_mut() {
        obj.insert("model".into(), Value::String(entry.upstream_id.clone()));
    }
    // Text-only upstreams (Loomy / Maodao) reject image content; strip it so a
    // multimodal client request still succeeds on a vision-less model.
    strip_image_content(&mut upstream_body, entry.vision);
    clamp_max_tokens(&mut upstream_body, entry.max_output);

    let response = match entry.source.as_str() {
        "opencode" => {
            // The OpenCode adapter answers in full; a streaming client gets
            // the whole envelope as SSE chunks below.
            let creds = {
                let guard = state.opencode_runtime.lock().await;
                guard
                    .as_ref()
                    .map(|r| (r.base.clone(), r.password.clone()))
            };
            let (base, password) = match creds {
                Some(c) => c,
                None => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "OpenCode 运行时未启动（在面板里点「启动运行时」）",
                    )
                }
            };
            match state.opencode.chat(&base, &password, &upstream_body).await {
                Ok(value) => Upstream::Json(value),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "maodao" => {
            // P1-3: rotation is single-use — hold the keepalive gate across
            // load → ensure_fresh → persist so no other path burns the RT.
            let _gate = state.refresh_gate.lock().await;
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.maodao.clone()
            };
            if !creds.ready() {
                return json_error(StatusCode::BAD_REQUEST, "Maodao is not logged in");
            }
            if let Err(e) = maodao::ensure_fresh(&mut creds, 5 * 60 * 1000).await {
                return json_error(StatusCode::UNAUTHORIZED, &format!("Maodao session expired: {}", e));
            }
            {
                let mut store = state.store.lock().await;
                store.credentials.maodao = creds.clone();
                store.save_credentials();
            }
            drop(_gate);
            // The upstream answers with SSE even when `stream` is omitted, so
            // pin it explicitly to whatever the client asked for.
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(stream));
            }
            let _slot = match state.maodao_sem.acquire().await {
                Ok(permit) => permit,
                Err(_) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "码道限流器已关闭"),
            };
            match state.maodao.chat(&creds, &upstream_body).await {
                Ok(r) => Upstream::Http(r),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "raccoon" => {
            // P1-3: single-use refresh token — same gate as Maodao.
            let _gate = state.refresh_gate.lock().await;
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.raccoon.clone()
            };
            if creds.access_token.is_empty() {
                return json_error(StatusCode::BAD_REQUEST, "Raccoon is not logged in");
            }
            if !ensure_fresh(&state.raccoon, &mut creds, 5 * 60 * 1000).await {
                return json_error(StatusCode::UNAUTHORIZED, "Raccoon session expired; please scan again");
            }
            {
                let mut store = state.store.lock().await;
                store.credentials.raccoon = creds.clone();
                store.save_credentials();
            }
            drop(_gate);
            match state.raccoon.chat(&creds, &upstream_body).await {
                Ok(r) => Upstream::Http(r),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "kilo" => {
            // Keyless lane: no credential to load, nothing to keep fresh.
            match state.kilo.chat(&upstream_body).await {
                Ok(r) => Upstream::Http(r),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "buddy" | "workbuddy" => {
            // Native Tencent-family channel (buddy.rs). One OAuth flow, two
            // products. The refresh token is single-use, so renewal runs under
            // the same global gate as every other rotation path.
            let source = entry.source.as_str();
            let product = if source == "workbuddy" { &buddy::WORKBUDDY } else { &buddy::CODEBUDDY };
            let mut creds = {
                let store = state.store.lock().await;
                match source {
                    "workbuddy" => store.credentials.buddy.workbuddy.clone(),
                    _ => store.credentials.buddy.buddy.clone(),
                }
            };
            if !creds.ready() {
                return json_error(StatusCode::UNAUTHORIZED, &format!("{} 未登录", product.display));
            }
            // Renew when the access token is inside the 5-minute lead.
            if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                match state.buddy.refresh(product, &mut creds).await {
                    Ok(()) => {
                        let mut store = state.store.lock().await;
                        match source {
                            "workbuddy" => store.credentials.buddy.workbuddy = creds.clone(),
                            _ => store.credentials.buddy.buddy = creds.clone(),
                        }
                        store.save_credentials();
                    }
                    Err(e) if e.starts_with("REFRESH_TERMINAL") => {
                        drop(_gate);
                        return json_error(StatusCode::UNAUTHORIZED,
                            &format!("{} 会话已失效，请重新登录", product.display));
                    }
                    Err(e) => {
                        drop(_gate);
                        return json_error(StatusCode::BAD_GATEWAY, &e);
                    }
                }
            }
            // The upstream only answers SSE (non-stream 400), so a non-streaming
            // client gets the folded response below.
            let client_stream = stream;
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(true));
            }
            match state.buddy.chat(product, &creds, &upstream_body).await {
                Ok(r) => {
                    if client_stream {
                        Upstream::Http(r)
                    } else {
                        Upstream::HttpFoldSse(r)
                    }
                }
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "trae" => {
            // TRAE SOLO: private event stream upstream; chat() folds it into
            // one translated OpenAI completion (trae.rs translate_trae_sse),
            // so this lane always yields Upstream::Json — no raw SSE passthrough.
            let source = entry.source.as_str();
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.trae.clone()
            };
            if !creds.ready() {
                return json_error(StatusCode::UNAUTHORIZED, &format!("{} 未登录", source));
            }
            if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
                // TRAE 无独立 refresh 路径（TokenCreds 靠登录回调 RT 换取），
                // 到期只能重新登录（与原实现一致）。
                return json_error(StatusCode::UNAUTHORIZED,
                    &format!("{} 会话已过期，请重新登录", source));
            }
            // 通道名必须与目录里的 function 匹配，否则上游在流内回 code=4001。
            if let Some(ch) = &entry.channel {
                if let Some(obj) = upstream_body.as_object_mut() {
                    obj.insert("__channel".into(), Value::String(ch.clone()));
                }
            }
            match state.trae.chat(&creds, &upstream_body, 0).await {
                Ok(v) => Upstream::Json(v),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "lobsterai" => {
            // Native Bearer-token channel (lobsterai.rs). Refreshes its
            // single-use RT under the global gate when the access token is
            // near expiry, then streams; a non-streaming client gets the SSE
            // folded back into one JSON completion.
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.lobsterai.clone()
            };
            if !creds.ready() {
                return json_error(StatusCode::UNAUTHORIZED, "lobsterai 未登录");
            }
            if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                if let Err(e) = state.lobsterai.refresh(&mut creds).await {
                    drop(_gate);
                    if e.starts_with("REFRESH_TERMINAL") {
                        return json_error(StatusCode::UNAUTHORIZED,
                            "lobsterai 会话已失效，请重新登录");
                    }
                    return json_error(StatusCode::BAD_GATEWAY, &e);
                }
                let mut store = state.store.lock().await;
                store.credentials.lobsterai = creds.clone();
                store.save_credentials();
            }
            let client_stream = stream;
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(client_stream));
            }
            match state.lobsterai.chat(&creds, &upstream_body).await {
                Ok(r) => {
                    if client_stream {
                        Upstream::Http(r)
                    } else {
                        Upstream::HttpFoldSse(r)
                    }
                }
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "minimax" => {
            // Anthropic-protocol native channel (minimax.rs). OpenAI-shaped
            // request/response is produced by the translate layer there:
            // stream:true → live Anthropic SSE → OpenAI SSE translation;
            // stream:false → Anthropic message JSON folded to OpenAI here.
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.minimax.clone()
            };
            if !creds.ready() {
                return json_error(StatusCode::UNAUTHORIZED, "minimax 未登录");
            }
            if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                if let Err(e) = state.minimax.refresh(&mut creds).await {
                    drop(_gate);
                    if e.starts_with("REFRESH_TERMINAL") {
                        return json_error(StatusCode::UNAUTHORIZED,
                            "minimax 会话已失效，请重新登录");
                    }
                    return json_error(StatusCode::BAD_GATEWAY, &e);
                }
                let mut store = state.store.lock().await;
                store.credentials.minimax = creds.clone();
                store.save_credentials();
            }
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(stream));
            }
            let resp = match state.minimax.chat(&creds, &upstream_body).await {
                Ok(r) => r,
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            };
            if stream {
                Upstream::HttpTranslate(resp)
            } else {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return json_error(
                        StatusCode::BAD_GATEWAY,
                        &format!("minimax HTTP {}: {}", status.as_u16(),
                            text.replace('"', "'").chars().take(300).collect::<String>()),
                    );
                }
                let value: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        return json_error(StatusCode::BAD_GATEWAY, "minimax 响应不是 JSON");
                    }
                };
                Upstream::Json(crate::minimax::translate_minimax_completion(&value))
            }
        }
        "cline" => {
            // OpenAI-compatible native channel; refresh the single-use RT
            // under the global gate when the access token nears expiry.
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.cline.clone()
            };
            if !creds.ready() {
                return json_error(StatusCode::UNAUTHORIZED, "Cline 未登录");
            }
            if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                if let Err(e) = state.cline.refresh(&mut creds).await {
                    drop(_gate);
                    if e.starts_with("REFRESH_TERMINAL") {
                        return json_error(StatusCode::UNAUTHORIZED, "Cline 会话已失效，请重新登录");
                    }
                    return json_error(StatusCode::BAD_GATEWAY, &e);
                }
                let mut store = state.store.lock().await;
                store.credentials.cline = creds.clone();
                store.save_credentials();
            }
            let client_stream = stream;
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(client_stream));
            }
            match state.cline.chat(&creds, &upstream_body).await {
                Ok(r) => {
                    if client_stream {
                        Upstream::Http(r)
                    } else {
                        Upstream::HttpFoldSse(r)
                    }
                }
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "zen" => {
            // Anonymous lane. Three hard gates, all measured on this machine:
            // (1) the full client fingerprint (minted session id, six x-opencode
            // headers, opencode/ UA); (2) `bash`+`read` declared with non-empty
            // schemas — 403 FreeTierError without them; (3) `stream:true` only —
            // a stream:false request 403s, so a non-streaming client gets the
            // folded SSE below.
            // Quota is accounted per session, so one conversation = one stable
            // minted id (first-message identity is replayed by every client).
            let seed = zen::conversation_seed(&upstream_body);
            let dir = { state.store.lock().await.dir.clone() };
            let (session, changed) = {
                let mut guard = state.zen_sessions.lock().await;
                if let Some(s) = guard.get(&seed) {
                    (s.clone(), false)
                } else {
                    let seq = (guard.len() % 4096) as u32 + 1;
                    let minted = zen::mint_session_id(crate::store::now_ms() as u64, seq);
                    guard.insert(seed, minted.clone());
                    (minted, true)
                }
            };
            if changed {
                let snapshot = state.zen_sessions.lock().await.clone();
                tokio::task::spawn_blocking(move || zen::save_session_map(&dir, &snapshot));
            }
            zen::apply_fingerprint(&mut upstream_body);
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(true));
            }
            match state.zen.chat(&upstream_body, &session).await {
                Ok(r) => {
                    if stream {
                        Upstream::Http(r)
                    } else {
                        Upstream::HttpFoldSse(r)
                    }
                }
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        _ => {
            let session = {
                let store = state.store.lock().await;
                active_loomy_session(&store)
            };
            let session = match session {
                Some(s) => s,
                None => return json_error(StatusCode::BAD_REQUEST, "Loomy has no active session"),
            };
            match state.loomy.chat(&session, &upstream_body).await {
                Ok(r) => Upstream::Http(r),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
    };

    // OpenCode: the completion is already translated.
    if let Upstream::Json(value) = response {
        record_usage(&state, &entry, value.get("usage")).await;
        if stream {
            let sse = opencode_sse(&value);
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(Body::from(sse))
                .unwrap_or_default();
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap_or_default();
    }
    // Zen non-stream: the lane insists on SSE; fold it back into one JSON
    // completion for the client that asked for a plain body.
    // MiniMax stream: translate Anthropic SSE → OpenAI SSE live.
    let response = match response {
        Upstream::HttpFoldSse(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            if !status.is_success() {
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        "{{\"error\":{{\"message\":\"upstream HTTP {}: {}\"}}}}",
                        status.as_u16(),
                        text.replace('"', "'").chars().take(300).collect::<String>()
                    )))
                    .unwrap_or_default();
            }
            let folded = zen::fold_sse(&text);
            record_usage(&state, &entry, folded.get("usage")).await;
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(folded.to_string()))
                .unwrap_or_default();
        }
        Upstream::HttpTranslate(r) => {
            let status = r.status();
            if !status.is_success() {
                let text = r.text().await.unwrap_or_default();
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        "{{\"error\":{{\"message\":\"upstream HTTP {}: {}\"}}}}",
                        status.as_u16(),
                        text.replace('"', "'").chars().take(300).collect::<String>()
                    )))
                    .unwrap_or_default();
            }
            let translated = translate_minimax_stream(r.bytes_stream(), state.clone(), entry.clone());
            let body = Body::from_stream(translated);
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(body)
                .unwrap_or_default();
        }
        Upstream::Http(r) => r,
        Upstream::Json(_) => unreachable!(),
    };

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "application/json")
            .body(Body::from(format!(
                "{{\"error\":{{\"message\":\"upstream HTTP {}: {}\"}}}}",
                status.as_u16(),
                text.replace('"', "'").chars().take(300).collect::<String>()
            )))
            .unwrap_or_default();
    }

    if stream {
        let upstream = response.bytes_stream();
        let teed = tee_points(upstream, state.clone(), entry.clone());
        let body = Body::from_stream(teed);
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(body)
            .unwrap_or_default()
    } else {
        let text = match response.text().await {
            Ok(t) => t,
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("upstream body: {}", e)),
        };
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            record_usage(&state, &entry, value.get("usage")).await;
        }
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(text))
            .unwrap_or_default()
    }
}

/// Render a completed completion as an SSE sequence for stream:true clients.
fn opencode_sse(value: &Value) -> String {
    let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("chatcmpl");
    let created = value.get("created").and_then(|v| v.as_i64()).unwrap_or(0);
    let model = value.get("model").cloned().unwrap_or(json!(""));
    let choice = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let finish = choice.get("finish_reason").cloned().unwrap_or(json!("stop"));
    let usage = value.get("usage").cloned();
    let chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }]
        })
        .to_string()
    };
    let mut out = String::new();
    out.push_str(&format!("data: {}\n\n", chunk(json!({ "role": "assistant" }), Value::Null)));
    if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
        if !content.is_empty() {
            out.push_str(&format!("data: {}\n\n", chunk(json!({ "content": content }), Value::Null)));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for (i, call) in calls.iter().enumerate() {
            let mut c = call.clone();
            if let Some(obj) = c.as_object_mut() {
                obj.insert("index".into(), json!(i));
            }
            out.push_str(&format!("data: {}\n\n", chunk(json!({ "tool_calls": [c] }), Value::Null)));
        }
    }
    out.push_str(&format!("data: {}\n\n", chunk(json!({}), finish)));
    if let Some(u) = usage {
        let with_usage = json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [], "usage": u
        });
        out.push_str(&format!("data: {}\n\n", with_usage));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

/// Tee an upstream SSE stream: forward bytes untouched, parse complete `data:`
/// lines for the final usage chunk and record it in the ledger.
fn tee_points(
    upstream: impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    state: Arc<AppState>,
    entry: ModelEntry,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send {
    let mut line_buf: Vec<u8> = Vec::new();
    upstream.flat_map(move |chunk| {
        let mut events: Vec<Result<bytes::Bytes, std::io::Error>> = Vec::new();
        match chunk {
            Ok(bytes) => {
                line_buf.extend_from_slice(&bytes);
                // Parse only complete lines; keep the remainder for next round.
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                    if let Ok(text) = std::str::from_utf8(&line) {
                        if let Some(rest) = text.strip_prefix("data:") {
                            let payload = rest.trim();
                            if payload != "[DONE]" && !payload.is_empty() {
                                if let Ok(value) = serde_json::from_str::<Value>(payload) {
                                // Maodao (and OpenAI-style SSE) put `"usage": null`
                                // on every delta chunk — only a real object counts.
                                if value.get("usage").filter(|u| !u.is_null()).is_some() {
                                    let usage = value.get("usage").cloned().unwrap();
                                    let state = state.clone();
                                    let entry = entry.clone();
                                    tokio::spawn(async move {
                                        record_usage(&state, &entry, Some(&usage)).await;
                                    });
                                }
                            }
                            }
                        }
                    }
                }
                events.push(Ok(bytes));
            }
            Err(e) => events.push(Err(std::io::Error::other(e.to_string()))),
        }
        futures_util::stream::iter(events)
    })
}

/// Translate an Anthropic-protocol SSE stream (MiniMax) into OpenAI SSE on
/// the fly. The state machine buffers partial events (multi-byte safe) and
/// emits whatever is complete; when the upstream ends we flush `finish()`
/// so a client always sees a well-formed trailing chunk + `[DONE]`. The
/// usage carried by `message_delta` is folded into the ledger.
struct MiniMaxTranslateStream {
    inner: futures_util::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    sm: minimax::AnthropicToOpenAI,
    state: Arc<AppState>,
    entry: ModelEntry,
    done: bool,
}

impl futures_util::Stream for MiniMaxTranslateStream {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.done {
            return std::task::Poll::Ready(None);
        }
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                let out = self.sm.feed(&bytes);
                std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(out))))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                std::task::Poll::Ready(Some(Err(std::io::Error::other(e.to_string()))))
            }
            std::task::Poll::Ready(None) => {
                self.done = true;
                let tail = self.sm.finish();
                if let Some(u) = self.sm.usage().cloned() {
                    let state = self.state.clone();
                    let entry = self.entry.clone();
                    tokio::spawn(async move {
                        record_usage(&state, &entry, Some(&u)).await;
                    });
                }
                std::task::Poll::Ready(Some(Ok(bytes::Bytes::from(tail))))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

fn translate_minimax_stream(
    upstream: impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    state: Arc<AppState>,
    entry: ModelEntry,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send {
    MiniMaxTranslateStream {
        inner: upstream.boxed(),
        sm: minimax::AnthropicToOpenAI::new(),
        state,
        entry,
        done: false,
    }
}

/// Fold one usage object into the ledger.
pub async fn record_usage(state: &AppState, entry: &ModelEntry, usage: Option<&Value>) {
    let usage = match usage {
        Some(u) => u,
        None => return,
    };
    let prompt = usage.get("prompt_tokens").and_then(|v| v.as_u64());
    let completion = usage.get("completion_tokens").and_then(|v| v.as_u64());
    let points = usage.get("points_consumed").and_then(|v| v.as_f64());
    let kind = if points.is_some() { "reported" } else { "tokens-only" };
    if prompt.is_none() && completion.is_none() && points.is_none() {
        return;
    }
    // Maodao-style SSE puts an all-zero usage object on every intermediate
    // delta; only the final chunk carries the real numbers. Zero rows would
    // flood the ledger, so skip them.
    if points.is_none()
        && prompt.unwrap_or(0) == 0
        && completion.unwrap_or(0) == 0
    {
        return;
    }
    let mut store = state.store.lock().await;
    store.ledger.record(LedgerEntry {
        ts_ms: crate::store::now_ms(),
        source: entry.source.clone(),
        model: entry.upstream_id.clone(),
        points,
        prompt_tokens: prompt,
        completion_tokens: completion,
        kind: kind.into(),
    });
    store.save_ledger();
}

pub fn active_loomy_session(store: &Store) -> Option<String> {
    let active = store.credentials.loomy.active.clone()?;
    let session = store
        .credentials
        .loomy
        .sessions
        .iter()
        .find(|s| s.name == active)?;
    if session.session.is_empty() {
        return None;
    }
    Some(session.session.clone())
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": { "message": message } }))).into_response()
}

/// Full snapshot for the panel (also served on /health).
pub async fn build_snapshot(state: &AppState) -> Value {
    let store = state.store.lock().await;
    let mut loomy_models = Vec::new();
    let mut raccoon_models = Vec::new();
    let mut opencode_models = Vec::new();
    let mut maodao_models = Vec::new();
    let mut kilo_models = Vec::new();
    let mut zen_models = Vec::new();
    let mut buddy_models = Vec::new();
    let mut workbuddy_models = Vec::new();
    let mut lobsterai_models = Vec::new();
    let mut trae_models = Vec::new();
    let mut minimax_models = Vec::new();
    let mut cline_models = Vec::new();
    let mut qoder_models = Vec::new();
    let mut qodercn_models = Vec::new();
    let mut freebuff_models = Vec::new();
    for entry in store.models.loomy.iter() {
        loomy_models.push(model_json(&store, entry));
    }
    for entry in store.models.raccoon.iter() {
        raccoon_models.push(model_json(&store, entry));
    }
    for entry in store.models.opencode.iter() {
        opencode_models.push(model_json(&store, entry));
    }
    for entry in store.models.maodao.iter() {
        maodao_models.push(model_json(&store, entry));
    }
    for entry in store.models.kilo.iter() {
        kilo_models.push(model_json(&store, entry));
    }
    for entry in store.models.zen.iter() {
        zen_models.push(model_json(&store, entry));
    }
    for entry in store.models.buddy.iter() {
        buddy_models.push(model_json(&store, entry));
    }
    for entry in store.models.workbuddy.iter() {
        workbuddy_models.push(model_json(&store, entry));
    }
    for entry in store.models.lobsterai.iter() {
        lobsterai_models.push(model_json(&store, entry));
    }
    for entry in store.models.trae.iter() {
        trae_models.push(model_json(&store, entry));
    }
    for entry in store.models.minimax.iter() {
        minimax_models.push(model_json(&store, entry));
    }
    for entry in store.models.cline.iter() {
        cline_models.push(model_json(&store, entry));
    }
    for entry in store.models.freebuff.iter() {
        freebuff_models.push(model_json(&store, entry));
    }
    // Qoder 目录仅展示（不发布到 /v1/models）：推理端点需 WASM 签名。
    qoder_models = qoder::fallback_models(qoder::QODER_SOURCE);
    qodercn_models = qoder::fallback_models(qoder::QODERCN_SOURCE);
    let active = store.credentials.loomy.active.clone();
    let loomy_session = store.credentials.loomy.sessions.iter().find(|s| Some(&s.name) == active.as_ref());
    let raccoon = &store.credentials.raccoon;
    let opencode_runtime = state.opencode_runtime.lock().await;
    let opencode_running = opencode_runtime.is_some();
    let opencode_version = opencode_runtime.as_ref().map(|r| r.version.clone()).unwrap_or_default();
    drop(opencode_runtime);
    // 面板账本取最近 200 条（前端翻页消费）；全量仍持久化在 ledger.json。
    let ledger_total = store.ledger.entries.len() as u64;
    let ledger: Vec<&LedgerEntry> = store.ledger.entries.iter().rev().take(200).collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "repo": env!("CARGO_PKG_REPOSITORY"),
        "endpoint": format!("http://127.0.0.1:{}/v1", store.settings.port),
        "port": store.settings.port,
        "modelsFile": store.settings.models_file,
        "apiKeys": store.api_keys.keys.iter().map(|k| json!({
            "id": k.id,
            "label": k.label,
            // 主密钥给全文（导出依赖），额外密钥只给掩码尾 4 位，降低截屏泄露面。
            "key": if k.primary { k.key.clone() } else { mask_key(&k.key) },
            "revealed": k.primary,
            "createdAtMs": k.created_at_ms,
            "primary": k.primary,
        })).collect::<Vec<_>>(),
        "loomy": {
            "sessions": store.credentials.loomy.sessions.iter().map(|s| json!({
                "name": s.name, "phone": s.phone, "expiresAtMs": s.expires_at_ms,
                "source": s.source, "active": Some(&s.name) == active.as_ref(),
                "expired": s.expires_at_ms > 0 && s.expires_at_ms < crate::store::now_ms(),
            })).collect::<Vec<_>>(),
            "active": active,
            "sessionExpiresAtMs": loomy_session.map(|s| s.expires_at_ms).unwrap_or(0),
            "balance": store.ledger.loomy_balance,
            "models": loomy_models,
        },
        "raccoon": {
            "loggedIn": !raccoon.access_token.is_empty(),
            "nickname": raccoon.nickname,
            "expiresAtMs": raccoon.expires_at_ms,
            "balance": store.ledger.raccoon_balance,
            "models": raccoon_models,
        },
        "opencode": {
            "running": opencode_running,
            "version": opencode_version,
            "models": opencode_models,
        },
        "maodao": {
            "loggedIn": store.credentials.maodao.ready(),
            "userName": store.credentials.maodao.user_name,
            "expiresAtMs": store.credentials.maodao.expires_at,
            "clientInstalled": maodao::client_installed(),
            "balance": store.ledger.maodao_balance,
            "models": maodao_models,
        },
        "kilo": {
            // Keyless lane: always available, no login state to report —
            // but the free pool logs prompts upstream, so the panel shows
            // the privacy warning beside the roster.
            "loggedIn": true,
            "privacy": crate::kilo::privacy_note(),
            "models": kilo_models,
        },
        "zen": {
            // Anonymous public-pool lane (Bearer public): no login state,
            // and the panel surfaces the per-session quota note instead.
            "loggedIn": true,
            "sessions": state.zen_sessions.lock().await.len(),
            "models": zen_models,
        },
        "buddy": {
            // Native Tencent-family channel: OAuth login + single-use RT renewal.
            "loggedIn": store.credentials.buddy.buddy.ready(),
            "nickname": store.credentials.buddy.buddy.nickname,
            "expiresAtMs": store.credentials.buddy.buddy.expires_at,
            "models": buddy_models,
            "balance": Value::Null,
        },
        "workbuddy": {
            "loggedIn": store.credentials.buddy.workbuddy.ready(),
            "nickname": store.credentials.buddy.workbuddy.nickname,
            "expiresAtMs": store.credentials.buddy.workbuddy.expires_at,
            "models": workbuddy_models,
            "balance": Value::Null,
        },
        "lobsterai": {
            "loggedIn": store.credentials.lobsterai.ready(),
            "nickname": store.credentials.lobsterai.nickname,
            "expiresAtMs": store.credentials.lobsterai.expires_at,
            "models": lobsterai_models,
        },
        "trae": {
            "loggedIn": store.credentials.trae.ready(),
            "nickname": store.credentials.trae.nickname,
            "expiresAtMs": store.credentials.trae.expires_at,
            "models": trae_models,
        },
        "minimax": {
            "loggedIn": store.credentials.minimax.ready(),
            "expiresAtMs": store.credentials.minimax.expires_at,
            "models": minimax_models,
            "balance": Value::Null,
        },
        "cline": {
            "loggedIn": store.credentials.cline.ready(),
            "nickname": store.credentials.cline.nickname,
            "expiresAtMs": store.credentials.cline.expires_at,
            "models": cline_models,
            "balance": Value::Null,
        },
        "qoder": {
            "loggedIn": store.credentials.qoder.ready(),
            "expiresAtMs": store.credentials.qoder.expires_at,
            "models": qoder_models,
            "gated": true,
            "gateNote": "Qoder 推理端点需 qoder-auth-wasm.wasm 生成 Bearer COSY.<载荷>.<签名> 与加密请求体，普通 Bearer 会被判 403 Signature invalid——本工具原生实现其登录/目录/每日签到额度，但暂不可调用其模型（目录仅供查看，不发布到 /v1/models）。",
        },
        "qodercn": {
            "loggedIn": store.credentials.qodercn.ready(),
            "expiresAtMs": store.credentials.qodercn.expires_at,
            "models": qodercn_models,
            "gated": true,
            "gateNote": "同上（WASM 签名限制）；仅登录与每日额度可用。",
        },
        "freebuff": {
            "loggedIn": store.credentials.freebuff.ready(),
            "nickname": store.credentials.freebuff.nickname,
            "expiresAtMs": store.credentials.freebuff.expires_at,
            "models": freebuff_models,
            "quota": store.models.freebuff_quota.clone().unwrap_or(Value::Null),
            "gated": true,
            "gateNote": "Freebuff 免费档推理被服务端强制要求官方 CLI（cost_mode:\"free\" 直连 API 返回 403 free_mode_cli_required，并附「可能封号」警告），本工具不伪装 CLI 身份转发；已接入登录态导入、模型目录与每日 freebucks 额度（25 分/日，0 点 Asia/Shanghai 刷新，price=0 为纯免费）。目录仅供查看，不发布到 /v1/models。",
        },
        // P1-3 keepalive loop state (rounds, notes, terminal blocks).
        "keepalive": crate::keepalive::keepalive_json(&store),
        "verifyAllRunning": state.verify_all_busy.load(std::sync::atomic::Ordering::SeqCst),
        // 转发口监听状态：axum serve 的 bind 是否成功（面板 pill 直接读这里，
        // 省掉每轮一次 server_status IPC）。
        "serverRunning": state.server_running.load(std::sync::atomic::Ordering::SeqCst),
        "modelsFetchedAtMs": store.models.fetched_at_ms,
        "ledger": ledger,
        "ledgerTotal": ledger_total,
    })
}

fn model_json(store: &crate::store::Store, entry: &ModelEntry) -> Value {
    let verdict = store.models.verdict(&entry.source, &entry.upstream_id);
    let (probe_ok, probe_note, probed_at) = match verdict {
        Some(v) => (
            Some(v.ok),
            Some(v.note.clone()),
            Some(v.at_ms),
        ),
        None => (None, None, None),
    };
    json!({
        "clientId": client_model_id(entry),
        "upstreamId": upstream_model_id(entry),
        "source": entry.source,
        "name": entry.name,
        "vision": entry.vision,
        "context": entry.context,
        "maxOutput": entry.max_output,
        "multiplier": entry.multiplier,
        "billingStatus": entry.billing_status,
        "probeOk": probe_ok,
        "probeNote": probe_note,
        "probedAtMs": probed_at,
        "published": store.models.publishable(&entry.source, &entry.upstream_id),
    })
}

/// Start the axum server on the loopback interface.
pub async fn serve(state: Arc<AppState>) -> std::io::Result<()> {
    let port = { state.store.lock().await.settings.port };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            state.server_running.store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(e);
        }
    };
    state.server_running.store(true, std::sync::atomic::Ordering::SeqCst);
    let result = axum::serve(listener, router(state.clone())).await;
    // Normal exit (never reached when the task is aborted — server_stop
    // clears the flag itself in that path).
    state.server_running.store(false, std::sync::atomic::Ordering::SeqCst);
    result
}

/// Idle timeout guard: nothing to do, the server runs until the app quits.
pub async fn idle() {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::header::HeaderName::from_lowercase(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn origin_fence_blocks_only_remote_web_pages() {
        // No Origin (native client) and local desktop renderers pass.
        assert!(origin_allowed(&hdrs(&[])));
        assert!(origin_allowed(&hdrs(&[("origin", "file://")])));
        assert!(origin_allowed(&hdrs(&[("origin", "app://.")])));
        assert!(origin_allowed(&hdrs(&[("origin", "null")])));
        assert!(origin_allowed(&hdrs(&[("origin", "http://localhost:3000")])));
        assert!(origin_allowed(&hdrs(&[("origin", "http://127.0.0.1:5173")])));
        // A public web page is the CSRF case and stays blocked.
        assert!(!origin_allowed(&hdrs(&[("origin", "https://evil.example.com")])));
        assert!(!origin_allowed(&hdrs(&[("origin", "http://10.0.0.5:8080")])));
    }

    #[test]
    fn clamp_lowers_only_what_the_upstream_would_reject() {
        let mut over = json!({ "max_tokens": 393216 });
        clamp_max_tokens(&mut over, 8192);
        assert_eq!(over["max_tokens"], Value::from(MAX_TOKENS_CAP));

        let mut fits = json!({ "max_tokens": 512 });
        clamp_max_tokens(&mut fits, 8192);
        assert_eq!(fits["max_tokens"], Value::from(512));

        // No client value and no catalog value: leave it for the upstream.
        let mut absent = json!({ "model": "m" });
        clamp_max_tokens(&mut absent, 0);
        assert!(absent.get("max_tokens").is_none());
    }

    #[test]
    fn absent_max_tokens_inherits_the_model_advertised_ceiling() {
        // The client did not ask; the ferry gives it the model's own max.
        let mut body = json!({ "model": "m" });
        clamp_max_tokens(&mut body, 32_768);
        assert_eq!(body["max_tokens"], Value::from(32_768));

        // The advertised ceiling past the gateway cap clamps down.
        let mut huge = json!({ "model": "m" });
        clamp_max_tokens(&mut huge, 393_216);
        assert_eq!(huge["max_tokens"], Value::from(MAX_TOKENS_CAP));

        // A client value always wins over the catalog default.
        let mut asked = json!({ "model": "m", "max_tokens": 256 });
        clamp_max_tokens(&mut asked, 32_768);
        assert_eq!(asked["max_tokens"], Value::from(256));
    }

    fn multimodal() -> Value {
        json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": [
                    {"type":"text","text":"look at this"},
                    {"type":"image_url","image_url":{"url":"https://x/a.png"}},
                    {"type":"image_url","image_url":{"url":"https://x/b.png"}}
                ]},
                { "role": "user", "content": "plain string" }
            ]
        })
    }

    #[test]
    fn strip_removes_image_parts_only_when_vision_off() {
        let mut body = multimodal();
        strip_image_content(&mut body, false);
        let first = &body["messages"][0]["content"];
        let items = first.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "text");
        // the non-array message is left untouched
        assert_eq!(body["messages"][1]["content"], "plain string");
    }

    #[test]
    fn strip_keeps_everything_when_vision_on() {
        let mut body = multimodal();
        strip_image_content(&mut body, true);
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn strip_collapses_empty_content_list() {
        let mut body = json!({
            "model": "m",
            "messages": [ {"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]} ]
        });
        strip_image_content(&mut body, false);
        assert!(body["messages"][0]["content"].as_str().is_some());
    }
}
