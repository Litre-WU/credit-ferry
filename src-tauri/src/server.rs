//! The local OpenAI-compatible forwarding surface (127.0.0.1 only).
//!
//! Mirrors the fences that made OC Ferry trustworthy:
//! - Bearer key with a constant-time compare; a missing/wrong key is 401.
//! - Any request carrying an `Origin` header is 403 (browsers stay out).
//! - Request bodies capped at 8 MB (413); at most 4 concurrent forwards (429).
//! - Model ids: the client-visible `Loomy · <name>` / `Raccoon · <name>`
//!   and the upstream ids (`loomy/<id>` / `raccoon/<id>`) are both accepted.
//! - SSE is passed through byte-for-byte; the stream is teed so the final
//!   `usage` chunk (Loomy reports `points_consumed`) lands in the ledger.

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

use crate::loomy::LoomyClient;
use crate::maodao::{self, MaodaoClient, MaodaoLogin};
use crate::opencode::{OpenCodeClient, OpenCodeRuntime};
use crate::raccoon::{ensure_fresh, RaccoonClient};
use crate::store::{LedgerEntry, ModelEntry, Store};

pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONCURRENCY: usize = 4;

pub struct AppState {
    pub store: Mutex<Store>,
    pub loomy: LoomyClient,
    pub raccoon: RaccoonClient,
    pub opencode: OpenCodeClient,
    pub opencode_runtime: Mutex<Option<OpenCodeRuntime>>,
    pub maodao: MaodaoClient,
    /// In-flight browser login for the Maodao upstream.
    pub maodao_login: Mutex<Option<MaodaoLogin>>,
    pub key: String,
    pub sem: Arc<Semaphore>,
}

/// What the three upstreams hand back after a chat call.
enum Upstream {
    /// A raw upstream HTTP response to pass through (Loomy / Raccoon).
    Http(reqwest::Response),
    /// An already-translated OpenAI-shaped completion (OpenCode).
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

/// Shared request guard: bearer key first, then the browser-origin fence.
/// Returns `Some(error response)` when the request must be refused.
fn guard(headers: &HeaderMap, key: &str) -> Option<Response> {
    if !authorized(headers, key) {
        return Some(json_error(StatusCode::UNAUTHORIZED, "Local proxy API key required"));
    }
    if headers.contains_key("origin") {
        return Some(json_error(StatusCode::FORBIDDEN, "Browser-origin requests are disabled"));
    }
    None
}

/// Client-visible model id, e.g. `Loomy · Spark X2.5`.
pub fn client_model_id(entry: &ModelEntry) -> String {
    let prefix = match entry.source.as_str() {
        "raccoon" => "Raccoon",
        "opencode" => "OC",
        "maodao" => "Maodao",
        _ => "Loomy",
    };
    format!("{} · {}", prefix, entry.name)
}

pub fn upstream_model_id(entry: &ModelEntry) -> String {
    format!("{}/{}", entry.source, entry.upstream_id)
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
        .chain(store.models.maodao.iter());
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
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(err) = guard(&headers, &state.key) {
        return err;
    }
    let snapshot = build_snapshot(&state).await;
    Json(snapshot).into_response()
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(err) = guard(&headers, &state.key) {
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
    if let Some(err) = guard(&headers, &state.key) {
        return err;
    }
    let _permit = match state.sem.try_acquire() {
        Ok(p) => p,
        Err(_) => return json_error(StatusCode::TOO_MANY_REQUESTS, "At most four requests may run at once"),
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
    let requested = payload.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let entry = match resolve_model(&state, requested).await {
        Some(e) => e,
        None => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "Select an available model from /v1/models",
            )
        }
    };

    let stream = payload.get("stream") == Some(&Value::Bool(true));
    // Rewrite the model id to the upstream id the gateway expects.
    let mut upstream_body = payload.clone();
    if let Some(obj) = upstream_body.as_object_mut() {
        obj.insert("model".into(), Value::String(entry.upstream_id.clone()));
    }
    // Text-only upstreams (Loomy / Maodao) reject image content; strip it so a
    // multimodal client request still succeeds on a vision-less model.
    strip_image_content(&mut upstream_body, entry.vision);

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
            // The upstream answers with SSE even when `stream` is omitted, so
            // pin it explicitly to whatever the client asked for.
            if let Some(obj) = upstream_body.as_object_mut() {
                obj.insert("stream".into(), Value::Bool(stream));
            }
            match state.maodao.chat(&creds, &upstream_body).await {
                Ok(r) => Upstream::Http(r),
                Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
            }
        }
        "raccoon" => {
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
            match state.raccoon.chat(&creds, &upstream_body).await {
                Ok(r) => Upstream::Http(r),
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
    let response = match response {
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
    let active = store.credentials.loomy.active.clone();
    let loomy_session = store.credentials.loomy.sessions.iter().find(|s| Some(&s.name) == active.as_ref());
    let raccoon = &store.credentials.raccoon;
    let opencode_runtime = state.opencode_runtime.lock().await;
    let opencode_running = opencode_runtime.is_some();
    let opencode_version = opencode_runtime.as_ref().map(|r| r.version.clone()).unwrap_or_default();
    drop(opencode_runtime);
    let ledger: Vec<&LedgerEntry> = store.ledger.entries.iter().rev().take(30).collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "endpoint": format!("http://127.0.0.1:{}/v1", store.settings.port),
        "port": store.settings.port,
        "modelsFile": store.settings.models_file,
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
        "modelsFetchedAtMs": store.models.fetched_at_ms,
        "ledger": ledger,
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
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, router(state)).await
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
