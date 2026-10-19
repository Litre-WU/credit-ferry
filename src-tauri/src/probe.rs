//! Model callability verification — the "体检" pass.
//!
//! A model appearing in a catalogue does not mean it answers: Loomy ships
//! image models in its chat catalogue (they answer 404 "暂未开放"), and any
//! upstream can be temporarily down. After a platform is added or refreshed
//! every never-probed model gets one minimal chat request; models whose fresh
//! verdict is a failure are hidden from /v1/models and from exports, and the
//! reason is shown in the panel instead of surfacing as a mid-conversation
//! upstream error.
//!
//! Cost disclosure: each probe spends a tiny amount of real credit on Loomy
//! and Raccoon (a few points at most); OpenCode's free pool costs nothing.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::server::AppState;
use crate::store::{now_ms, ModelCache, ModelEntry, StoredVerdict};

const PROBE_TIMEOUT_MS: u64 = 60_000;
const CONCURRENCY: usize = 3;

/// 渠道 → 模型目录。probe 与自动体检共用同一映射，避免两处漏渠道。
fn list_for<'s>(cache: &'s ModelCache, source: &str) -> &'s Vec<ModelEntry> {
    match source {
        "raccoon" => &cache.raccoon,
        "opencode" => &cache.opencode,
        "maodao" => &cache.maodao,
        "kilo" => &cache.kilo,
        "zen" => &cache.zen,
        "buddy" => &cache.buddy,
        "workbuddy" => &cache.workbuddy,
        "lobsterai" => &cache.lobsterai,
        "trae" => &cache.trae,
        "minimax" => &cache.minimax,
        "cline" => &cache.cline,
        _ => &cache.loomy,
    }
}

/// One probe request body: the cheapest possible chat.
/// OpenCode and Maodao need a bigger cap — their models are reasoning models
/// that burn the budget on thinking and then return an empty envelope
/// (false negative). Maodao also streams by default unless told otherwise.
fn probe_body(source: &str, model: &str) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": if source == "opencode" || source == "maodao" || source == "kilo" || source == "zen" || source == "buddy" || source == "workbuddy" || source == "lobsterai" || source == "trae" || source == "minimax" || source == "cline" { 200 } else { 8 },
    });
    if source == "maodao" {
        body["stream"] = json!(false);
    }
    body
}

/// Verify every model of one source. Returns the per-model verdicts.
pub async fn probe_source(state: &Arc<AppState>, source: &str) -> Vec<(String, StoredVerdict)> {
    let source = source.to_string();
    let entries: Vec<String> = {
        let store = state.store.lock().await;
        list_for(&store.models, &source)
            .iter()
            .map(|e| e.upstream_id.clone())
            .collect()
    };
    probe_ids(state, &source, entries).await
}

/// Verify a given set of upstream ids of one source (shared by the full
/// probe and the stale-recheck path).
async fn probe_ids(
    state: &Arc<AppState>,
    source: &str,
    entries: Vec<String>,
) -> Vec<(String, StoredVerdict)> {
    // Maodao counts concurrent sessions per account, so probing it at the
    // default width would trip its own limit.
    let width = if source == "maodao" {
        crate::server::MAODAO_CONCURRENCY
    } else {
        CONCURRENCY
    };
    let sem = Arc::new(Semaphore::new(width));
    let mut handles = Vec::new();
    for upstream_id in entries {
        let state = Arc::clone(state);
        let sem = Arc::clone(&sem);
        let source = source.to_string();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let mut verdict = probe_one(&state, &source, &upstream_id).await;
            if !verdict.ok && verdict.category == "rate_limited" {
                // 上游瞬时限流：退避后重试一次；再次被限流则保留
                // rate_limited 判定（不会把可用模型下架）。
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let retry = probe_one(&state, &source, &upstream_id).await;
                if retry.ok || retry.category != "rate_limited" {
                    verdict = retry;
                }
            }
            (upstream_id, verdict)
        }));
    }
    let mut out = Vec::new();
    for handle in handles {
        if let Ok((id, verdict)) = handle.await {
            out.push((id, verdict));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Probe one model.
async fn probe_one(state: &Arc<AppState>, source: &str, upstream_id: &str) -> StoredVerdict {
    let started = std::time::Instant::now();
    let body = probe_body(source, upstream_id);
    let outcome: Result<(), String> = match source {
        "opencode" => {
            let creds = {
                let guard = state.opencode_runtime.lock().await;
                guard.as_ref().map(|r| (r.base.clone(), r.password.clone()))
            };
            match creds {
                Some((base, password)) => {
                    state.opencode.chat(&base, &password, &body).await.map(|_| ())
                }
                None => Err("OpenCode 运行时未启动".to_string()),
            }
        }
        "maodao" => {
            // P1-3: probes rotate the single-use RT too — same gate.
            let _gate = state.refresh_gate.lock().await;
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.maodao.clone()
            };
            if !creds.ready() {
                return StoredVerdict {
                    ok: false,
                    note: "未登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            if let Err(e) = crate::maodao::ensure_fresh(&mut creds, 5 * 60 * 1000).await {
                return StoredVerdict {
                    ok: false,
                    note: short(&e),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            // Persist the rotated token (it is single-use).
            {
                let mut store = state.store.lock().await;
                store.credentials.maodao = creds.clone();
                store.save_credentials();
            }
            drop(_gate);
            match state.maodao.chat(&creds, &body).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        "raccoon" => {
            // P1-3: single-use refresh token — same gate.
            let _gate = state.refresh_gate.lock().await;
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.raccoon.clone()
            };
            if creds.access_token.is_empty() {
                return StoredVerdict {
                    ok: false,
                    note: "未登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            crate::raccoon::ensure_fresh(&state.raccoon, &mut creds, 5 * 60 * 1000).await;
            {
                let mut store = state.store.lock().await;
                store.credentials.raccoon = creds.clone();
                store.save_credentials();
            }
            drop(_gate);
            match state.raccoon.chat(&creds, &body).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        "kilo" => {
            // Keyless: the probe just needs the gateway to answer 2xx.
            // Free-pool reasoning models can spend the whole budget on
            // thinking and return an empty content — the HTTP status is
            // still the honest verdict (same discipline as OpenCode).
            match state.kilo.chat(&body).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        "zen" => {
            // The lane's three gates apply to probes too: fingerprint headers,
            // the bash+read tool declaration, and stream:true (a stream:false
            // request 403s). Probes share ONE stable session (seed "probe") —
            // minting fresh per request trips the per-session quota and reads
            // as a wave of 429s instead of real verdicts.
            // Lock order follows the chat path: store.dir first, then the
            // session map, never the other way around.
            let dir = { state.store.lock().await.dir.clone() };
            let (session, changed) = {
                let mut guard = state.zen_sessions.lock().await;
                if let Some(s) = guard.get("probe") {
                    (s.clone(), false)
                } else {
                    let minted = crate::zen::mint_session_id(now_ms() as u64, 1);
                    guard.insert("probe".to_string(), minted.clone());
                    (minted, true)
                }
            };
            if changed {
                let snapshot = state.zen_sessions.lock().await.clone();
                tokio::task::spawn_blocking(move || crate::zen::save_session_map(&dir, &snapshot));
            }
            let mut body = body;
            body["stream"] = json!(true);
            crate::zen::apply_fingerprint(&mut body);
            match state.zen.chat(&body, &session).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        "buddy" | "workbuddy" => {
            // Native Tencent-family probe. Stream-only upstream (non-stream
            // 400), and the RT is single-use — renew under the same gate as
            // the chat path when the access token is near expiry.
            let source = source.to_string();
            let product = if source == "workbuddy" {
                &crate::buddy::WORKBUDDY
            } else {
                &crate::buddy::CODEBUDDY
            };
            let mut creds = {
                let store = state.store.lock().await;
                if source == "workbuddy" {
                    store.credentials.buddy.workbuddy.clone()
                } else {
                    store.credentials.buddy.buddy.clone()
                }
            };
            if !creds.ready() {
                return StoredVerdict {
                    ok: false,
                    note: "未登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            if creds.expired() || creds.expires_at - now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                if let Err(e) = state.buddy.refresh(product, &mut creds).await {
                    return StoredVerdict {
                        ok: false,
                        note: short(&e),
                        at_ms: now_ms(),
                        category: "unavailable".into(),
                    };
                }
                let mut store = state.store.lock().await;
                if source == "workbuddy" {
                    store.credentials.buddy.workbuddy = creds.clone();
                } else {
                    store.credentials.buddy.buddy = creds.clone();
                }
                store.save_credentials();
            }
            let mut body = body;
            body["stream"] = json!(true);
            match state.buddy.chat(product, &creds, &body).await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = resp.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        "trae" => {
            // TRAE SOLO: chat() 内部折叠私有事件流，Ok(value) 即表示拿到了
            // 真实正文/工具调用（event:error 已映射为 Err）。
            let mut creds = {
                let store = state.store.lock().await;
                store.credentials.trae.clone()
            };
            if !creds.ready() {
                return StoredVerdict {
                    ok: false,
                    note: "未登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            if creds.expired() || creds.expires_at - now_ms() < 5 * 60 * 1000 {
                return StoredVerdict {
                    ok: false,
                    note: "会话已过期，请重新登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            // 通道名必须与目录 function 匹配，否则上游流内回 code=4001。
            let channel = {
                let store = state.store.lock().await;
                store.models.trae.iter()
                    .find(|e| e.upstream_id == upstream_id)
                    .and_then(|e| e.channel.clone())
            };
            let mut body = body;
            body["stream"] = json!(true);
            if let Some(ch) = channel {
                body["__channel"] = json!(ch);
            }
            match state.trae.chat(&creds, &body, 0).await {
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            }
        }
        "lobsterai" | "minimax" | "cline" => {
            // Native Bearer-token channels: refresh single-use RT under the
            // global gate when near expiry, then one streamed probe request.
            let source = source.to_string();
            let mut creds = {
                let store = state.store.lock().await;
                match source.as_str() {
                    "lobsterai" => store.credentials.lobsterai.clone(),
                    "cline" => store.credentials.cline.clone(),
                    _ => store.credentials.minimax.clone(),
                }
            };
            if !creds.ready() {
                return StoredVerdict {
                    ok: false,
                    note: "未登录".into(),
                    at_ms: now_ms(),
                    category: "unavailable".into(),
                };
            }
            if creds.expired() || creds.expires_at - now_ms() < 5 * 60 * 1000 {
                let _gate = state.refresh_gate.lock().await;
                let refreshed = match source.as_str() {
                    "lobsterai" => state.lobsterai.refresh(&mut creds).await,
                    "cline" => state.cline.refresh(&mut creds).await,
                    _ => state.minimax.refresh(&mut creds).await,
                };
                if let Err(e) = refreshed {
                    return StoredVerdict {
                        ok: false,
                        note: short(&e),
                        at_ms: now_ms(),
                        category: "unavailable".into(),
                    };
                }
                let mut store = state.store.lock().await;
                match source.as_str() {
                    "lobsterai" => store.credentials.lobsterai = creds.clone(),
                    "cline" => store.credentials.cline = creds.clone(),
                    _ => store.credentials.minimax = creds.clone(),
                }
                store.save_credentials();
            }
            let mut body = body;
            body["stream"] = json!(true);
            let resp = match source.as_str() {
                "lobsterai" => state.lobsterai.chat(&creds, &body).await,
                "cline" => state.cline.chat(&creds, &body).await,
                _ => state.minimax.chat(&creds, &body).await,
            };
            match resp {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        Ok(())
                    } else {
                        let text = r.text().await.unwrap_or_default();
                        Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                    }
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            let session = {
                let store = state.store.lock().await;
                crate::server::active_loomy_session(&store)
            };
            match session {
                Some(s) => match state.loomy.chat(&s, &body).await {
                    Ok(resp) => {
                        // The send can succeed on a 404/400 — the status still
                        // has to be checked or broken models pass the probe.
                        let status = resp.status();
                        if status.is_success() {
                            Ok(())
                        } else {
                            let text = resp.text().await.unwrap_or_default();
                            Err(format!("upstream HTTP {}: {}", status.as_u16(), short(&text)))
                        }
                    }
                    Err(e) => Err(e),
                },
                None => Err("Loomy 没有可用会话".to_string()),
            }
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(()) => StoredVerdict {
            ok: true,
            note: format!("可用（{} ms）", duration_ms),
            at_ms: now_ms(),
            category: "available".into(),
        },
        Err(message) => {
            let category = if message.contains("timed out") || message.contains("timeout") {
                "timeout"
            } else if is_rate_limited(&message) {
                "rate_limited"
            } else {
                "unavailable"
            };
            StoredVerdict {
                ok: false,
                note: short(&message),
                at_ms: now_ms(),
                category: category.into(),
            }
        }
    }
}

/// 上游瞬时限流（HTTP 429 / 14003 too many requests）：模型本身可用，
/// 批量探针踩到账号级 QPS 时不应把模型下架。
fn is_rate_limited(msg: &str) -> bool {
    msg.contains("HTTP 429") || msg.contains("too many requests") || msg.contains("14003")
}

/// Keep an upstream error note short and readable.
fn short(text: &str) -> String {
    let cleaned = text.replace('\n', " ");
    if let Some(start) = cleaned.find("\"message\":\"") {
        let rest = &cleaned[start + 10..];
        if let Some(end) = rest.find('"') {
            return rest[..end].chars().take(100).collect();
        }
    }
    cleaned.chars().take(100).collect()
}

/// Probe with a hard deadline so a hanging upstream cannot stall the panel.
pub async fn probe_source_bounded(state: &Arc<AppState>, source: &str) -> Vec<(String, StoredVerdict)> {
    match tokio::time::timeout(
        Duration::from_millis(PROBE_TIMEOUT_MS * 12),
        probe_source(state, source),
    )
    .await
    {
        Ok(verdicts) => verdicts,
        Err(_) => Vec::new(),
    }
}

/// `probe_pending` with the same hard deadline — for the auto-probe path.
pub async fn probe_pending_bounded(state: &Arc<AppState>, source: &str) -> Vec<(String, StoredVerdict)> {
    match tokio::time::timeout(
        Duration::from_millis(PROBE_TIMEOUT_MS * 12),
        probe_pending(state, source),
    )
    .await
    {
        Ok(verdicts) => verdicts,
        Err(_) => Vec::new(),
    }
}

/// Store verdicts back into the model cache.
pub async fn save_verdicts(state: &Arc<AppState>, verdicts: &[(String, StoredVerdict)], source: &str) {
    let mut store = state.store.lock().await;
    for (id, verdict) in verdicts {
        store
            .models
            .probes
            .insert(format!("{}/{}", source, id), verdict.clone());
    }
    store.save_models();
}

/// Models of this source that still deserve a probe: no verdict at all, or
/// a failed verdict whose TTL lapsed. Successful verdicts that merely went
/// stale stay published without re-probing — that would burn Loomy/Raccoon
/// credits for no user-visible gain.
fn select_pending(cache: &ModelCache, source: &str, stale_failures_only: bool) -> Vec<String> {
    list_for(cache, source)
        .iter()
        .filter(|e| match cache.verdict(source, &e.upstream_id) {
            None => !stale_failures_only,
            Some(v) => !v.ok && !v.fresh(),
        })
        .map(|e| e.upstream_id.clone())
        .collect()
}

/// Does any model of this source lack a verdict (or hold a lapsed failing
/// one)? Drives the auto-probe after a catalogue refresh.
pub async fn needs_probe(state: &Arc<AppState>, source: &str) -> bool {
    let store = state.store.lock().await;
    !select_pending(&store.models, source, false).is_empty()
}

/// Probe only the models that still deserve it (missing verdict or lapsed
/// failure). Returns empty when everything is fresh.
pub async fn probe_pending(state: &Arc<AppState>, source: &str) -> Vec<(String, StoredVerdict)> {
    let ids = {
        let store = state.store.lock().await;
        select_pending(&store.models, source, false)
    };
    if ids.is_empty() {
        return Vec::new();
    }
    probe_ids(state, source, ids).await
}

/// Periodic recheck for the keepalive loop: only lapsed FAILING verdicts —
/// so a transiently broken model gets republished once upstream recovers
/// without a manual refresh.
pub async fn reprobe_stale_failures(state: &Arc<AppState>, source: &str) {
    let ids = {
        let store = state.store.lock().await;
        select_pending(&store.models, source, true)
    };
    if ids.is_empty() {
        return;
    }
    let verdicts = probe_ids(state, source, ids).await;
    save_verdicts(state, &verdicts, source).await;
}
