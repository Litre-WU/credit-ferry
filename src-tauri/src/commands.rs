//! Tauri command layer: the panel talks to Rust through these, never HTTP.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query, State as AxumState};
use axum::response::Html;
use axum::routing::{get as axum_get, post as axum_post};
use axum::{http::StatusCode, Router};
use qrcode::render::svg;
use qrcode::QrCode;
use serde_json::{json, Value};
use tauri::State;
use tokio::sync::Mutex;

use crate::loomy::import_client_sessions;
use crate::maodao::{self, MaodaoLoginStatus};
use crate::freebuff;
use crate::qoder;
use crate::raccoon::{QrStatus, RaccoonClient};
use crate::server::{self, AppState};
use crate::store::{now_ms, OWNER, TokenCreds};

pub struct QrFlow {
    pub code: String,
    pub started_at_ms: i64,
    pub consumed: bool,
}

pub struct Ferry {
    pub state: Arc<AppState>,
    pub qr: Mutex<Option<QrFlow>>,
    pub server_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

pub type FerryState<'a> = State<'a, Arc<Ferry>>;

/// Full snapshot for the panel.
#[tauri::command]
pub async fn get_snapshot(ferry: FerryState<'_>) -> Result<Value, String> {
    Ok(server::build_snapshot(&ferry.state).await)
}

#[tauri::command]
pub fn get_api_key(ferry: FerryState<'_>) -> String {
    ferry.state.key.clone()
}

// ---------------------------------------------------------------- server

#[tauri::command]
pub async fn server_start(ferry: FerryState<'_>) -> Result<Value, String> {
    let mut guard = ferry.server_task.lock().await;
    if guard.is_some() {
        return Ok(json!({ "running": true, "alreadyRunning": true }));
    }
    let state = Arc::clone(&ferry.state);
    let handle = tauri::async_runtime::spawn(async move {
        if let Err(e) = server::serve(state).await {
            eprintln!("ferry server exited: {}", e);
        }
    });
    *guard = Some(handle);
    drop(guard);
    let refresh_state = Arc::clone(&ferry.state);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        refresh_all(&refresh_state).await;
    });
    Ok(json!({ "running": true, "alreadyRunning": false }))
}

#[tauri::command]
pub async fn server_stop(ferry: FerryState<'_>) -> Result<Value, String> {
    let mut guard = ferry.server_task.lock().await;
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    Ok(json!({ "running": false }))
}

#[tauri::command]
pub async fn server_status(ferry: FerryState<'_>) -> Result<Value, String> {
    let running = ferry.server_task.lock().await.is_some();
    let port = { ferry.state.store.lock().await.settings.port };
    Ok(json!({ "running": running, "port": port }))
}

// ---------------------------------------------------------------- keepalive

/// Start the background keepalive loop if it is not running yet.
/// Independent of the forwarding port — credential renewal must continue
/// even while the server is stopped (panel toggle / server_start idempotent).
pub(crate) fn start_keepalive(state: &Arc<AppState>) {
    if state
        .keepalive_started
        .compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .is_ok()
    {
        crate::keepalive::spawn(Arc::clone(state));
    }
}

/// Force one keepalive round right now (panel button).
#[tauri::command]
pub async fn keepalive_run_once(ferry: FerryState<'_>) -> Result<Value, String> {
    start_keepalive(&ferry.state);
    let (_, snapshot) = crate::keepalive::run_round(&ferry.state).await;
    Ok(snapshot)
}

/// Current keepalive state (panel polling).
#[tauri::command]
pub async fn keepalive_status(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    Ok(crate::keepalive::keepalive_json(&store))
}

/// Refresh every logged-in upstream (used on server start).
pub async fn refresh_all(state: &Arc<AppState>) {
    let _ = loomy_refresh_inner(state).await;
    let _ = raccoon_refresh_inner(state).await;
    let _ = opencode_refresh_inner(state).await;
    let _ = maodao_refresh_inner(state).await;
    let _ = kilo_refresh_inner(state).await;
    let _ = zen_refresh_inner(state).await;
    // Buddy family refreshes only when logged in; errors are ignored here.
    let _ = buddy_refresh_inner(state, "buddy").await;
    let _ = buddy_refresh_inner(state, "workbuddy").await;
    let _ = generic_refresh_inner(state, "lobsterai").await;
    let _ = generic_refresh_inner(state, "trae").await;
    let _ = generic_refresh_inner(state, "minimax").await;
    let _ = freebuff_refresh_inner(state).await;
    // P1-3: keep the renewal loop alive regardless of the server toggle.
    start_keepalive(state);
}

/// After a catalogue refresh, verify any model that has never been probed
/// ("追加平台后验证模型是否可调用"). Runs in the background so the panel
/// never blocks; costs a few credits per probed model on Loomy/Raccoon.
pub async fn auto_probe_if_needed(state: &Arc<AppState>, source: &str) {
    if !crate::probe::needs_probe(state, source).await {
        return;
    }
    let verdicts = crate::probe::probe_pending_bounded(state, source).await;
    crate::probe::save_verdicts(state, &verdicts, source).await;
}

/// Verify every model of one source right now (panel button).
#[tauri::command]
pub async fn probe_models(ferry: FerryState<'_>, source: String) -> Result<Value, String> {
    let source = if source == "raccoon" || source == "opencode" || source == "maodao" || source == "kilo" || source == "zen" || source == "buddy" || source == "workbuddy" || source == "lobsterai" || source == "trae" || source == "minimax" || source == "cline" { source } else { "loomy".to_string() };
    let verdicts = crate::probe::probe_source_bounded(&ferry.state, &source).await;
    crate::probe::save_verdicts(&ferry.state, &verdicts, &source).await;
    let ok = verdicts.iter().filter(|(_, v)| v.ok).count();
    Ok(json!({
        "probed": verdicts.len(),
        "ok": ok,
        "failed": verdicts.len() - ok,
        "verdicts": verdicts.iter().map(|(id, v)| json!({
            "id": id, "ok": v.ok, "note": v.note, "category": v.category
        })).collect::<Vec<_>>(),
    }))
}

/// 统一验证：全部可转发渠道串行重探（渠道内并发 3，渠道间串行以压低
/// 同时打多家上游的限流概率）。后台跑，前端经 snapshot.verifyAllRunning
/// 与每渠道 probeOk 看进度。Freebuff/Qoder 是 gated 目录（不转发推理），
/// 不参与探针。
pub const VERIFY_ALL_SOURCES: [&str; 12] = [
    "loomy", "raccoon", "opencode", "maodao", "kilo", "zen",
    "buddy", "workbuddy", "lobsterai", "trae", "minimax", "cline",
];

#[tauri::command]
pub async fn verify_all_models(ferry: FerryState<'_>) -> Result<Value, String> {
    let state = Arc::clone(&ferry.state);
    if state
        .verify_all_busy
        .compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .is_err()
    {
        return Err("统一验证正在进行中，请等待本轮完成".into());
    }
    tauri::async_runtime::spawn(async move {
        for source in VERIFY_ALL_SOURCES {
            // 每渠道重探整目录：结果即时入库，面板轮询 snapshot 看进度。
            let verdicts = crate::probe::probe_source(&state, source).await;
            crate::probe::save_verdicts(&state, &verdicts, source).await;
        }
        // 全部渠道重探完成后补一轮 catalog 刷新，把新增/下架模型同步进目录。
        refresh_all(&state).await;
        state.verify_all_busy.store(false, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(json!({ "started": true, "sources": VERIFY_ALL_SOURCES }))
}

// ---------------------------------------------------------------- opencode

/// Bring up the isolated OpenCode runtime exactly once. The lock is held for
/// the WHOLE start so a concurrent caller (panel button vs auto-start) can
/// never slip through the check-then-start window and leak a second server.
/// Returns true when THIS call started it, false when it was already running.
pub async fn ensure_opencode_runtime(state: &Arc<AppState>) -> Result<bool, String> {
    let mut guard = state.opencode_runtime.lock().await;
    if guard.is_some() {
        return Ok(false);
    }
    let data_dir = { state.store.lock().await.dir.clone() };
    let status = |_: &str| {};
    let runtime = crate::opencode::start(&data_dir, &status).await?;
    *guard = Some(runtime);
    Ok(true)
}

/// Start the isolated OpenCode runtime (downloads the binary on first run).
#[tauri::command]
pub async fn opencode_start(ferry: FerryState<'_>) -> Result<Value, String> {
    let started = ensure_opencode_runtime(&ferry.state)
        .await
        .map_err(|e| format!("OpenCode 启动失败：{}", e))?;
    let version = { ferry.state.opencode_runtime.lock().await.as_ref().map(|r| r.version.clone()) };
    if started {
        let _ = opencode_refresh_inner(&ferry.state).await;
    }
    Ok(json!({ "running": true, "alreadyRunning": !started, "version": version }))
}

/// Stop the isolated OpenCode runtime.
#[tauri::command]
pub async fn opencode_stop(ferry: FerryState<'_>) -> Result<Value, String> {
    let mut guard = ferry.state.opencode_runtime.lock().await;
    if let Some(mut runtime) = guard.take() {
        runtime.stop();
    }
    let mut store = ferry.state.store.lock().await;
    store.models.opencode.clear();
    store.save_models();
    Ok(json!({ "running": false }))
}

pub async fn opencode_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let creds = {
        let guard = state.opencode_runtime.lock().await;
        guard.as_ref().map(|r| (r.base.clone(), r.password.clone()))
    };
    let (base, password) = match creds {
        Some(c) => c,
        None => return Err("OpenCode 运行时未启动".into()),
    };
    let models = state
        .opencode
        .models(&base, &password)
        .await
        .map_err(|e| format!("读取 OpenCode 模型失败：{}", e))?;
    let mut store = state.store.lock().await;
    store.models.opencode = models;
    store.models.fetched_at_ms = now_ms();
    store.save_models();
    drop(store);
    auto_probe_if_needed(state, "opencode").await;
    Ok(json!({ "models": state.store.lock().await.models.opencode.len() }))
}

#[tauri::command]
pub async fn opencode_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    opencode_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- loomy

/// 微信扫码登录上下文（一次性 loopback 登录窗口共享状态）。
struct LoomyWechatCtx {
    state: Arc<AppState>,
    uuid: Mutex<Option<String>>,
    last_errcode: Mutex<String>,
    rcode: Mutex<Option<String>>,
    msgid: Mutex<Option<String>>,
    done: Mutex<bool>,
}

/// Loomy 的 loopback 登录：`GET /wechat/qr` 展示二维码，前端 `/wechat/poll`
/// 轮询扫码状态，确认后自动换会话并落盘；首次扫码的新微信走
/// `/wechat/complete` 短信绑定。
async fn loomy_wechat_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let ctx = Arc::new(LoomyWechatCtx {
        state: Arc::clone(state),
        uuid: Mutex::new(None),
        last_errcode: Mutex::new(String::new()),
        rcode: Mutex::new(None),
        msgid: Mutex::new(None),
        done: Mutex::new(false),
    });
    let url = format!("http://127.0.0.1:{}{}", port, crate::loomy::WECHAT_QR_PATH);
    open_in_browser(&url);
    tauri::async_runtime::spawn(async move {
        let app = Router::new()
            .route(crate::loomy::WECHAT_QR_PATH, axum_get(loomy_qr_page))
            .route(crate::loomy::WECHAT_POLL_PATH, axum_get(loomy_poll))
            .route(crate::loomy::WECHAT_COMPLETE_PATH, axum_post(loomy_complete))
            .with_state(ctx);
        let server = axum::serve(listener, app);
        tokio::select! {
            _ = server => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(crate::loomy::WECHAT_LOGIN_TIMEOUT_MS)) => {}
        }
    });
    Ok(json!({ "authUrl": url, "mode": "wechat_qr" }))
}

#[tauri::command]
pub async fn loomy_wechat_login(ferry: FerryState<'_>) -> Result<Value, String> {
    loomy_wechat_login_inner(&ferry.state).await
}

/// 确保已拿到二维码 uuid（首次调用时才去微信开放平台拉授权页）。
async fn ensure_loomy_uuid(ctx: &LoomyWechatCtx) -> Result<String, String> {
    let mut guard = ctx.uuid.lock().await;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let http = crate::loomy::wechat_client();
    let uuid = crate::loomy::fetch_wechat_uuid(&http).await?;
    *guard = Some(uuid.clone());
    Ok(uuid)
}

/// 把登录成功的会话写进凭据文件并设为当前会话。
async fn persist_loomy_login(ctx: &LoomyWechatCtx, login: crate::loomy::LoomyLogin) {
    let expires_at_ms = crate::store::now_ms() + crate::loomy::SESSION_TTL_SECONDS * 1000;
    let name = if login.phone.is_empty() {
        format!("wx-{}", &login.userid.chars().rev().take(4).collect::<String>())
    } else {
        format!("wx-{}", login.phone.chars().rev().take(4).collect::<String>())
    };
    let mut store = ctx.state.store.lock().await;
    let sessions = &mut store.credentials.loomy.sessions;
    if let Some(existing) = sessions.iter_mut().find(|s| s.session == login.session) {
        existing.userid = login.userid.clone();
        existing.phone = login.phone.clone();
        existing.expires_at_ms = expires_at_ms;
    } else {
        sessions.push(crate::store::LoomySession {
            name: name.clone(),
            session: login.session.clone(),
            userid: login.userid.clone(),
            phone: login.phone.clone(),
            expires_at_ms,
            source: "wechat".into(),
        });
    }
    store.credentials.loomy.active = Some(name);
    store.save_credentials();
}

/// 二维码展示页：内嵌微信二维码图片 + 前端轮询脚本。
async fn loomy_qr_page(
    AxumState(ctx): AxumState<Arc<LoomyWechatCtx>>,
) -> (StatusCode, Html<String>) {
    if *ctx.done.lock().await {
        return (StatusCode::OK, Html(login_done_page()));
    }
    let uuid = match ensure_loomy_uuid(&ctx).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Html(format!(
                    r#"<!doctype html><meta charset="utf-8"><title>二维码获取失败</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>二维码获取失败</h1><p>{}</p><p><a href="/wechat/qr">重试</a></p>"#,
                    e.replace('<', "&lt;")
                )),
            )
        }
    };
    let http = crate::loomy::wechat_client();
    let data_url = match crate::loomy::fetch_wechat_qr_data_url(&http, &uuid).await {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Html(format!(
                    r#"<!doctype html><meta charset="utf-8"><title>二维码下载失败</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>二维码下载失败</h1><p>{}</p><p><a href="/wechat/qr">重试</a></p>"#,
                    e.replace('<', "&lt;")
                )),
            )
        }
    };
    (StatusCode::OK, Html(qr_page(&data_url)))
}

/// 轮询扫码状态；确认后自动换会话 + 落盘。
async fn loomy_poll(AxumState(ctx): AxumState<Arc<LoomyWechatCtx>>) -> axum::Json<Value> {
    if *ctx.done.lock().await {
        return axum::Json(json!({ "status": "confirmed" }));
    }
    let uuid = match ensure_loomy_uuid(&ctx).await {
        Ok(u) => u,
        Err(e) => return axum::Json(json!({ "status": "error", "message": e })),
    };
    let last = ctx.last_errcode.lock().await.clone();
    let http = crate::loomy::wechat_client();
    let polled = crate::loomy::poll_wechat_once(&http, &uuid, &last).await;
    if !polled.errcode.is_empty() {
        *ctx.last_errcode.lock().await = polled.errcode.clone();
    }
    match polled.status {
        crate::loomy::WechatPollStatus::Confirmed => {
            match crate::loomy::bind_third_account(&http, &polled.code).await {
                Ok(record) => {
                    let rcode = record.get("rcode").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let bind = record.get("bind").and_then(|v| v.as_i64()).unwrap_or(0);
                    if rcode.is_empty() {
                        return axum::Json(json!({ "status": "error", "message": "Loomy 绑定响应缺少 rcode" }));
                    }
                    if bind == 1 {
                        match crate::loomy::bind_skip(&http, &rcode).await {
                            Ok(login) => {
                                persist_loomy_login(&ctx, login).await;
                                *ctx.done.lock().await = true;
                                axum::Json(json!({ "status": "confirmed" }))
                            }
                            Err(e) => axum::Json(json!({ "status": "error", "message": e })),
                        }
                    } else {
                        // 首次扫码的新微信：需要绑定手机号才能拿到会话。
                        *ctx.rcode.lock().await = Some(rcode);
                        axum::Json(json!({ "status": "need_phone" }))
                    }
                }
                Err(e) => axum::Json(json!({ "status": "error", "message": e })),
            }
        }
        crate::loomy::WechatPollStatus::Expired => {
            // 清空 uuid，下次进入 /wechat/qr 会重新拉一张。
            *ctx.uuid.lock().await = None;
            *ctx.last_errcode.lock().await = String::new();
            axum::Json(json!({ "status": "expired" }))
        }
        other => axum::Json(json!({ "status": other.as_str() })),
    }
}

/// 短信绑定手机号（send_sms / verify_sms）。
async fn loomy_complete(
    AxumState(ctx): AxumState<Arc<LoomyWechatCtx>>,
    axum::Json(payload): axum::Json<Value>,
) -> axum::Json<Value> {
    let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let phone = payload
        .get("phone")
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_default();
    let code = payload
        .get("code")
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_default();
    let http = crate::loomy::wechat_client();
    match action {
        "send_sms" => {
            let Some(rcode) = ctx.rcode.lock().await.clone() else {
                return axum::Json(json!({ "ok": false, "message": "当前没有待绑定的扫码会话" }));
            };
            match crate::loomy::bind_send_msg(&http, &rcode, &phone).await {
                Ok(msgid) => {
                    *ctx.msgid.lock().await = Some(msgid);
                    axum::Json(json!({ "ok": true }))
                }
                Err(e) => axum::Json(json!({ "ok": false, "message": e })),
            }
        }
        "verify_sms" => {
            let (rcode, msgid) = {
                let g1 = ctx.rcode.lock().await;
                let g2 = ctx.msgid.lock().await;
                (g1.clone(), g2.clone())
            };
            let (Some(rcode), Some(msgid)) = (rcode, msgid) else {
                return axum::Json(json!({ "ok": false, "message": "请先获取短信验证码" }));
            };
            match crate::loomy::bind_check_code(&http, &rcode, &code, &msgid).await {
                Ok(login) => {
                    persist_loomy_login(&ctx, login).await;
                    *ctx.done.lock().await = true;
                    axum::Json(json!({ "ok": true, "done": true }))
                }
                Err(e) => axum::Json(json!({ "ok": false, "message": e })),
            }
        }
        _ => axum::Json(json!({ "ok": false, "message": format!("未知 action: {}", action) })),
    }
}

fn qr_page(data_url: &str) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>Loomy 微信扫码登录</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","Microsoft YaHei",sans-serif;
         background:#f5f6f8; color:#1f2329; }}
  .card {{ background:#fff; border-radius:12px; padding:28px 32px; box-shadow:0 4px 24px rgba(0,0,0,.08);
          text-align:center; width:320px; }}
  h1 {{ font-size:17px; margin:0 0 4px; font-weight:600; }}
  .sub {{ font-size:13px; color:#8a9099; margin-bottom:18px; }}
  .qr {{ width:220px; height:220px; display:block; margin:0 auto; border:1px solid #eceef1; border-radius:8px; }}
  .status {{ margin-top:16px; font-size:13px; color:#4e5969; min-height:20px; }}
  .status[data-tone="ok"] {{ color:#0f9d58; font-weight:600; }}
  .status[data-tone="err"] {{ color:#d93026; }}
  .form {{ display:none; margin-top:18px; text-align:left; }}
  .form[data-show="1"] {{ display:block; }}
  label {{ display:block; font-size:12px; color:#8a9099; margin-bottom:4px; }}
  input {{ width:100%; box-sizing:border-box; padding:8px 10px; font-size:14px; margin-bottom:10px;
          border:1px solid #d9dde3; border-radius:6px; }}
  button {{ width:100%; padding:9px; font-size:14px; border:0; border-radius:6px;
           background:#07c160; color:#fff; cursor:pointer; }}
  button:disabled {{ background:#c9cdd4; cursor:not-allowed; }}
  button.ghost {{ background:#f2f3f5; color:#1f2329; margin-top:8px; }}
  .row {{ display:flex; gap:8px; }}
</style>
<body>
  <div class="card">
    <h1>Loomy 微信扫码登录</h1>
    <div class="sub">请使用微信扫描下方二维码</div>
    <img class="qr" src="{data_url}" alt="微信二维码"/>
    <div class="status" id="status">等待扫码…</div>
    <div class="form" id="form" data-show="0">
      <label for="phone">手机号（中国大陆 +86）</label>
      <div class="row">
        <input id="phone" placeholder="请输入手机号"/>
        <button id="send" type="button">发送验证码</button>
      </div>
      <label for="code">短信验证码</label>
      <div class="row">
        <input id="code" placeholder="请输入验证码"/>
        <button id="verify" type="button">完成绑定</button>
      </div>
    </div>
  </div>
<script>
  const statusEl = document.getElementById('status');
  const formEl = document.getElementById('form');
  const phoneEl = document.getElementById('phone');
  const codeEl = document.getElementById('code');
  const sendEl = document.getElementById('send');
  const verifyEl = document.getElementById('verify');
  let stopped = false;
  function setStatus(text, tone) {{
    statusEl.textContent = text;
    if (tone) statusEl.dataset.tone = tone; else delete statusEl.dataset.tone;
  }}
  sendEl.addEventListener('click', async () => {{
    const phone = phoneEl.value.trim();
    if (phone.length === 0) return;
    sendEl.disabled = true;
    try {{
      const r = await fetch('/wechat/complete', {{
        method: 'POST', headers: {{ 'Content-Type': 'application/json' }},
        body: JSON.stringify({{ action: 'send_sms', phone }})
      }});
      const j = await r.json();
      if (j.ok) setStatus('验证码已发送，请输入短信验证码', 'ok');
      else {{ setStatus(j.message || '发送失败', 'err'); sendEl.disabled = false; }}
    }} catch (e) {{ setStatus(String(e), 'err'); sendEl.disabled = false; }}
  }});
  verifyEl.addEventListener('click', async () => {{
    const code = codeEl.value.trim();
    if (code.length === 0) return;
    verifyEl.disabled = true;
    try {{
      const r = await fetch('/wechat/complete', {{
        method: 'POST', headers: {{ 'Content-Type': 'application/json' }},
      body: JSON.stringify({{ action: 'verify_sms', phone: phoneEl.value.trim(), code }})
      }});
      const j = await r.json();
      if (j.ok && j.done) {{
        setStatus('登录成功，可以关闭这个页面了', 'ok');
        stopped = true;
      }} else {{ setStatus(j.message || '验证失败', 'err'); verifyEl.disabled = false; }}
    }} catch (e) {{ setStatus(String(e), 'err'); verifyEl.disabled = false; }}
  }});
  async function tick() {{
    if (stopped) return;
    try {{
      const r = await fetch('/wechat/poll');
      const j = await r.json();
      if (j.status === 'waiting') setStatus('等待扫码…');
      else if (j.status === 'scanned') setStatus('已扫码，请在微信中确认');
      else if (j.status === 'confirmed') {{ setStatus('登录成功，可以关闭这个页面了', 'ok'); stopped = true; }}
      else if (j.status === 'need_phone') {{
        setStatus('该微信尚未绑定手机号，请完成绑定', 'err');
        formEl.dataset.show = '1';
      }}
      else if (j.status === 'expired') setStatus('二维码已失效，正在刷新…', 'err');
      else if (j.status === 'cancelled') {{ setStatus('已取消扫码', 'err'); stopped = true; }}
      else setStatus(j.message || '未知状态', 'err');
    }} catch (e) {{
      setStatus('轮询异常：' + String(e), 'err');
    }}
    if (!stopped) setTimeout(tick, 1500);
  }}
  tick();
</script>"#
    )
}

fn login_done_page() -> String {
    r#"<!doctype html><meta charset="utf-8"><title>登录成功</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>登录成功</h1><p>可以关闭这个页面，回到 CreditFerry 面板。</p>"#
        .to_string()
}

/// Scan the installed Loomy desktop client for sessions (read-only).
#[tauri::command]
pub async fn loomy_import_client(ferry: FerryState<'_>) -> Result<Value, String> {
    let found = import_client_sessions();
    let mut store = ferry.state.store.lock().await;
    let mut added = 0;
    for session in found {
        if store
            .credentials
            .loomy
            .sessions
            .iter()
            .any(|s| s.session == session.session)
        {
            continue;
        }
        store.credentials.loomy.sessions.push(session);
        added += 1;
    }
    store.save_credentials();
    if added > 0 && store.credentials.loomy.active.is_none() {
        store.credentials.loomy.active = store
            .credentials
            .loomy
            .sessions
            .first()
            .map(|s| s.name.clone());
        store.save_credentials();
    }
    Ok(json!({ "added": added, "sessions": store.credentials.loomy.sessions.len() }))
}

/// Validate and store a manually pasted Loomy session token.
#[tauri::command]
pub async fn loomy_set_session(ferry: FerryState<'_>, session: String, name: Option<String>) -> Result<Value, String> {
    let session = session.trim().to_string();
    if session.is_empty() {
        return Err("会话不能为空".into());
    }
    // Validate against the gateway before storing.
    ferry
        .state
        .loomy
        .models(&session)
        .await
        .map_err(|e| format!("该会话无法通过 Loomy 网关校验：{}", e))?;
    let name = name.unwrap_or_else(|| format!("manual-{}", &session.chars().rev().take(4).collect::<String>()));
    let mut store = ferry.state.store.lock().await;
    if !store
        .credentials
        .loomy
        .sessions
        .iter()
        .any(|s| s.session == session)
    {
        store.credentials.loomy.sessions.push(crate::store::LoomySession {
            name: name.clone(),
            session: session.clone(),
            userid: String::new(),
            phone: String::new(),
            expires_at_ms: 0,
            source: "manual".into(),
        });
    }
    store.credentials.loomy.active = Some(name);
    store.save_credentials();
    Ok(json!({ "ok": true }))
}

/// Pick the active Loomy session by name.
#[tauri::command]
pub async fn loomy_select(ferry: FerryState<'_>, name: String) -> Result<Value, String> {
    let mut store = ferry.state.store.lock().await;
    if !store.credentials.loomy.sessions.iter().any(|s| s.name == name) {
        return Err("会话不存在".into());
    }
    store.credentials.loomy.active = Some(name);
    store.save_credentials();
    Ok(json!({ "ok": true }))
}

/// Drop a stored Loomy session.
#[tauri::command]
pub async fn loomy_forget(ferry: FerryState<'_>, name: String) -> Result<Value, String> {
    let mut store = ferry.state.store.lock().await;
    store.credentials.loomy.sessions.retain(|s| s.name != name);
    if store.credentials.loomy.active.as_deref() == Some(name.as_str()) {
        store.credentials.loomy.active = store
            .credentials
            .loomy
            .sessions
            .first()
            .map(|s| s.name.clone());
    }
    store.save_credentials();
    Ok(json!({ "ok": true }))
}

/// Refresh Loomy model catalogue + credit balance.
#[tauri::command]
/// Refresh Loomy model catalogue + credit balance.
pub async fn loomy_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let session = {
        let store = state.store.lock().await;
        server::active_loomy_session(&store)
    };
    let session = match session {
        Some(s) => s,
        None => return Err("Loomy 没有可用会话".into()),
    };
    let models = state
        .loomy
        .models(&session)
        .await
        .map_err(|e| format!("读取 Loomy 模型失败：{}", e))?;
    let quota = state.loomy.quota(&session).await.ok();
    let mut store = state.store.lock().await;
    store.models.loomy = models;
    store.models.fetched_at_ms = now_ms();
    store.save_models();
    if let Some(q) = quota {
        if let Some(balance) = q.get("balance").and_then(|v| v.as_f64()) {
            store.ledger.loomy_balance = Some(balance);
        }
        store.save_ledger();
    }
    drop(store);
    auto_probe_if_needed(state, "loomy").await;
    Ok(json!({
        "models": state.store.lock().await.models.loomy.len(),
        "balance": state.store.lock().await.ledger.loomy_balance,
    }))
}

#[tauri::command]
pub async fn loomy_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    loomy_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- raccoon

/// Start a WeChat QR login: generate the code locally, render the SVG.
#[tauri::command]
pub async fn raccoon_qr_start(ferry: FerryState<'_>) -> Result<Value, String> {
    let code = crate::store::new_hex(16);
    let url = RaccoonClient::qr_login_url(&code);
    let qr = QrCode::new(url.as_bytes()).map_err(|e| format!("二维码生成失败：{}", e))?;
    let image = qr
        .render()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#0f172a"))
        .light_color(svg::Color("#ffffff"))
        .build();
    let mut guard = ferry.qr.lock().await;
    *guard = Some(QrFlow {
        code: code.clone(),
        started_at_ms: now_ms(),
        consumed: false,
    });
    Ok(json!({
        "code": code,
        "scanUrl": url,
        "svg": image,
        "timeoutMs": crate::raccoon::QR_LOGIN_TIMEOUT_MS,
    }))
}

/// Poll the QR login once (the panel calls this every 2 s).
#[tauri::command]
pub async fn raccoon_qr_status(ferry: FerryState<'_>) -> Result<Value, String> {
    let flow = {
        let guard = ferry.qr.lock().await;
        match &*guard {
            Some(f) => f.clone_code(),
            None => return Ok(json!({ "status": "idle" })),
        }
    };
    if now_ms() - flow.started_at_ms > crate::raccoon::QR_LOGIN_TIMEOUT_MS {
        *ferry.qr.lock().await = None;
        return Ok(json!({ "status": "timed_out" }));
    }
    let status = ferry.state.raccoon.qr_poll(&flow.code).await;
    match status {
        QrStatus::Pending => Ok(json!({ "status": "pending" })),
        QrStatus::Logging { expired_at } => Ok(json!({ "status": "logging", "expiredAt": expired_at })),
        QrStatus::Canceled => {
            *ferry.qr.lock().await = None;
            Ok(json!({ "status": "canceled" }))
        }
        QrStatus::TimedOut => {
            *ferry.qr.lock().await = None;
            Ok(json!({ "status": "timed_out" }))
        }
        QrStatus::Success {
            access,
            refresh,
            expires_at_ms,
            nickname,
        } => {
            {
                let mut store = ferry.state.store.lock().await;
                store.credentials.raccoon.access_token = access;
                store.credentials.raccoon.refresh_token = refresh;
                store.credentials.raccoon.expires_at_ms = expires_at_ms;
                store.credentials.raccoon.nickname = nickname;
                // Fresh credential: any stale terminal block is void now.
                store.credentials.keepalive.blocks.remove("raccoon");
                store.save_credentials();
            }
            *ferry.qr.lock().await = None;
            Ok(json!({ "status": "success" }))
        }
    }
}

impl QrFlow {
    fn clone_code(&self) -> QrFlow {
        QrFlow {
            code: self.code.clone(),
            started_at_ms: self.started_at_ms,
            consumed: self.consumed,
        }
    }
}

/// Forget the Raccoon credential (does not touch Loomy).
#[tauri::command]
pub async fn raccoon_logout(ferry: FerryState<'_>) -> Result<Value, String> {
    let mut store = ferry.state.store.lock().await;
    store.credentials.raccoon = Default::default();
    store.credentials.keepalive.blocks.remove("raccoon");
    store.models.raccoon.clear();
    store.save_credentials();
    store.save_models();
    Ok(json!({ "ok": true }))
}

/// Refresh the Raccoon catalogue + credit balance (auto-rotates the token).
pub async fn raccoon_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    // P1-3: this path rotates too — same gate; clear any terminal block on success.
    let _gate = state.refresh_gate.lock().await;
    let mut creds = {
        let store = state.store.lock().await;
        store.credentials.raccoon.clone()
    };
    if creds.access_token.is_empty() {
        return Err("小浣熊未登录".into());
    }
    crate::raccoon::ensure_fresh(&state.raccoon, &mut creds, 5 * 60 * 1000).await;
    {
        let mut store = state.store.lock().await;
        store.credentials.raccoon = creds.clone();
        store.credentials.keepalive.blocks.remove("raccoon");
        store.save_credentials();
    }
    drop(_gate);
    let models = state
        .raccoon
        .catalog(&creds)
        .await
        .map_err(|e| format!("读取小浣熊模型失败：{}", e))?;
    let balance = state.raccoon.balance(&creds).await.ok();
    let mut store = state.store.lock().await;
    store.models.raccoon = models;
    store.models.fetched_at_ms = now_ms();
    store.save_models();
    if let Some(b) = balance {
        if let Some(total) = b.get("total").and_then(|v| v.as_f64()) {
            store.ledger.raccoon_balance = Some(total);
        }
        store.save_ledger();
    }
    drop(store);
    auto_probe_if_needed(state, "raccoon").await;
    Ok(json!({
        "models": state.store.lock().await.models.raccoon.len(),
        "balance": state.store.lock().await.ledger.raccoon_balance,
    }))
}

#[tauri::command]
pub async fn raccoon_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    raccoon_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- maodao

/// Import the session stored by the installed CodeArts desktop client
/// (read-only; the refresh token is single-use so the client and the ferry
/// cannot both rotate the same session).
#[tauri::command]
pub async fn maodao_import_client(ferry: FerryState<'_>) -> Result<Value, String> {
    if !maodao::client_installed() {
        return Err("未检测到码道桌面客户端（%APPDATA%\\codearts）".into());
    }
    let found = maodao::import_client_sessions();
    let session = match found.into_iter().next() {
        Some(s) => s,
        None => return Err("客户端里没有可导入的完整会话（需要刷新令牌＋DPoP 密钥）".into()),
    };
    let creds = session.to_creds();
    {
        let mut store = ferry.state.store.lock().await;
        store.credentials.maodao = creds;
        // Fresh credential: any stale terminal block is void now.
        store.credentials.keepalive.blocks.remove("maodao");
        store.save_credentials();
    }
    maodao_refresh_inner(&ferry.state).await
}

/// Start a browser login: returns the portal URL the panel should open.
#[tauri::command]
pub async fn maodao_login_start(ferry: FerryState<'_>) -> Result<Value, String> {
    let flow = maodao::login_start().await?;
    let url = flow.url.clone();
    *ferry.state.maodao_login.lock().await = Some(flow);
    open_in_browser(&url);
    Ok(json!({ "url": url, "timeoutMs": maodao::LOGIN_TIMEOUT_MS }))
}

/// Hand a URL to the system's default browser (the login URL is entirely
/// generated by us, so there is no injection surface).
///
/// Windows: `ShellExecuteW` — the same API Explorer uses when you click a
/// link. The old `explorer.exe <url>` spawn is unreliable here: explorer
/// parses its argument as a *file path*, so login URLs containing `&`, `#`
/// or a long PKCE challenge get silently dropped and no browser opens.
/// The other platforms already shell out to their documented opener.
fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        // verb="open" + lpFile=url + 其余空 ⇒ 交给系统默认浏览器处理。
        // 注意 lpFile 必传：只传 verb 会让 ShellExecuteW 找不到文件而返回 ≤32。
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            eprintln!("open_in_browser: ShellExecuteW failed (code={}) for {}", result.0 as isize, url);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(not(any(windows, target_os = "macos", unix)))]
    {
        let _ = url;
    }
}

/// Poll the in-flight login. On success the session is persisted and the
/// catalogue refreshed.
#[tauri::command]
pub async fn maodao_login_status(ferry: FerryState<'_>) -> Result<Value, String> {
    let snapshot = {
        let guard = ferry.state.maodao_login.lock().await;
        match &*guard {
            Some(flow) => {
                let status = flow.rx.borrow().clone();
                let pending = status == MaodaoLoginStatus::Pending
                    && now_ms() - flow.started_at_ms <= maodao::LOGIN_TIMEOUT_MS as i64;
                let (status, dpop_private, dpop_public) = if pending {
                    (MaodaoLoginStatus::Pending, String::new(), String::new())
                } else {
                    (
                        if status == MaodaoLoginStatus::Pending {
                            MaodaoLoginStatus::TimedOut
                        } else {
                            status
                        },
                        flow.dpop_private.clone(),
                        flow.dpop_public.clone(),
                    )
                };
                (status, dpop_private, dpop_public)
            }
            None => return Ok(json!({ "status": "idle" })),
        }
    };
    match snapshot.0 {
        MaodaoLoginStatus::Pending => Ok(json!({ "status": "pending" })),
        MaodaoLoginStatus::Failed(message) => {
            *ferry.state.maodao_login.lock().await = None;
            Ok(json!({ "status": "failed", "message": message }))
        }
        MaodaoLoginStatus::TimedOut => {
            *ferry.state.maodao_login.lock().await = None;
            Ok(json!({ "status": "timed_out" }))
        }
        MaodaoLoginStatus::Success => {
            let creds = {
                let guard = ferry.state.maodao_login.lock().await;
                let flow = guard.as_ref().ok_or_else(|| "登录流程已结束".to_string())?;
                let result = flow
                    .result
                    .lock()
                    .await
                    .clone()
                    .ok_or_else(|| "登录结果缺失".to_string())?;
                let mut creds = match result {
                    Ok(c) => c,
                    Err(message) => return Ok(json!({ "status": "failed", "message": message })),
                };
                creds.dpop_private = snapshot.1.clone();
                creds.dpop_public = snapshot.2.clone();
                creds
            };
            *ferry.state.maodao_login.lock().await = None;
            {
                let mut store = ferry.state.store.lock().await;
                store.credentials.maodao = creds;
                // Fresh credential: any stale terminal block is void now.
                store.credentials.keepalive.blocks.remove("maodao");
                store.save_credentials();
            }
            let refreshed = maodao_refresh_inner(&ferry.state).await;
            match refreshed {
                Ok(v) => Ok(json!({ "status": "success", "refresh": v })),
                Err(e) => Ok(json!({ "status": "success", "warning": e })),
            }
        }
    }
}

/// Forget the Maodao session (does not log the desktop client out).
#[tauri::command]
pub async fn maodao_logout(ferry: FerryState<'_>) -> Result<Value, String> {
    *ferry.state.maodao_login.lock().await = None;
    let mut store = ferry.state.store.lock().await;
    store.credentials.maodao = Default::default();
    store.credentials.keepalive.blocks.remove("maodao");
    store.models.maodao.clear();
    store.save_credentials();
    store.save_models();
    Ok(json!({ "ok": true }))
}

/// Refresh the Maodao catalogue + token balance (auto-rotates the session).
pub async fn maodao_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    // P1-3: this path rotates too — same gate; clear any terminal block on success.
    let _gate = state.refresh_gate.lock().await;
    let mut creds = {
        let store = state.store.lock().await;
        store.credentials.maodao.clone()
    };
    if !creds.ready() {
        return Err("码道未登录".into());
    }
    maodao::ensure_fresh(&mut creds, 5 * 60 * 1000)
        .await
        .map_err(|e| format!("码道会话失效：{}", e))?;
    {
        let mut store = state.store.lock().await;
        store.credentials.maodao = creds.clone();
        store.credentials.keepalive.blocks.remove("maodao");
        store.save_credentials();
    }
    drop(_gate);
    let models = state
        .maodao
        .catalog(&creds)
        .await
        .map_err(|e| format!("读取码道模型失败：{}", e))?;
    let quota = state.maodao.quota(&creds).await.ok();
    let mut store = state.store.lock().await;
    store.models.maodao = models;
    store.models.fetched_at_ms = now_ms();
    store.save_models();
    if let Some(q) = quota {
        let balance = q
            .get("available")
            .and_then(|v| v.as_f64())
            .or_else(|| q.get("total").and_then(|v| v.as_f64()));
        if let Some(b) = balance {
            store.ledger.maodao_balance = Some(b);
        }
        store.save_ledger();
    }
    let balance = store.ledger.maodao_balance;
    let count = store.models.maodao.len();
    drop(store);
    auto_probe_if_needed(state, "maodao").await;
    Ok(json!({ "models": count, "balance": balance }))
}

#[tauri::command]
pub async fn maodao_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    maodao_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- kilo

/// Refresh the Kilo free-pool roster. Keyless: no credential, just a listing
/// round + (first time) a probe pass so dead ids never reach a client picker.
pub async fn kilo_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let models = state
        .kilo
        .catalog()
        .await
        .map_err(|e| format!("读取 Kilo 模型失败：{}", e))?;
    {
        let mut store = state.store.lock().await;
        store.models.kilo = models;
        store.models.fetched_at_ms = now_ms();
        store.save_models();
    }
    let count = state.store.lock().await.models.kilo.len();
    auto_probe_if_needed(state, "kilo").await;
    Ok(json!({ "models": count }))
}

#[tauri::command]
pub async fn kilo_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    kilo_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- zen

/// Refresh the Zen free-lane roster. Anonymous: no credential, just the
/// listing + (first time) a probe pass.
pub async fn zen_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let models = state
        .zen
        .catalog()
        .await
        .map_err(|e| format!("读取 Zen 模型失败：{}", e))?;
    {
        let mut store = state.store.lock().await;
        store.models.zen = models;
        store.models.fetched_at_ms = now_ms();
        store.save_models();
    }
    let count = state.store.lock().await.models.zen.len();
    auto_probe_if_needed(state, "zen").await;
    Ok(json!({ "models": count }))
}

#[tauri::command]
pub async fn zen_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    zen_refresh_inner(&ferry.state).await
}

// ---------------------------------------------------------------- buddy family

/// One native Tencent-family product: login (browser authUrl + background poll),
/// then refresh the model roster. `workbuddy` differs only in endpoint/UA.
fn buddy_product(source: &str) -> &'static crate::buddy::Product {
    if source == "workbuddy" {
        &crate::buddy::WORKBUDDY
    } else {
        &crate::buddy::CODEBUDDY
    }
}

fn buddy_creds_slot<'a>(
    store: &'a crate::store::Store,
    source: &str,
) -> &'a crate::store::BuddyCreds {
    if source == "workbuddy" {
        &store.credentials.buddy.workbuddy
    } else {
        &store.credentials.buddy.buddy
    }
}

fn buddy_put_creds(store: &mut crate::store::Store, source: &str, creds: crate::store::BuddyCreds) {
    if source == "workbuddy" {
        store.credentials.buddy.workbuddy = creds;
    } else {
        store.credentials.buddy.buddy = creds;
    }
}

/// Refresh the model roster for one buddy-family product.
async fn buddy_refresh_inner(state: &Arc<AppState>, source: &str) -> Result<Value, String> {
    let product = buddy_product(source);
    let creds = {
        let store = state.store.lock().await;
        buddy_creds_slot(&store, source).clone()
    };
    if !creds.ready() {
        return Err(format!("{} 未登录", product.display));
    }
    let models = state
        .buddy
        .catalog(product, &creds)
        .await
        .map_err(|e| format!("读取 {} 模型失败：{}", product.display, e))?;
    let count = models.len();
    {
        let mut store = state.store.lock().await;
        if source == "workbuddy" {
            store.models.workbuddy = models;
        } else {
            store.models.buddy = models;
        }
        store.models.fetched_at_ms = now_ms();
        store.save_models();
    }
    auto_probe_if_needed(state, source).await;
    Ok(json!({ "models": count }))
}

/// Start a browser login: fetch auth/state, open the authUrl, then poll in the
/// background and persist the credential when the user finishes. Returns the
/// URL so the panel can show / re-open it.
async fn buddy_login_inner(state: &Arc<AppState>, source: &str) -> Result<Value, String> {
    let product = buddy_product(source);
    let (login_state, auth_url) = state
        .buddy
        .auth_state(product)
        .await
        .map_err(|e| format!("{} 发起登录失败：{}", product.display, e))?;
    open_in_browser(&auth_url);
    // Background poller: token → account → persist → refresh roster. The
    // panel polls get_snapshot and sees loggedIn flip when it lands.
    let bg = Arc::clone(state);
    let prod = *product;
    let src = source.to_string();
    let ls = login_state.clone();
    tauri::async_runtime::spawn(async move {
        let Ok(mut creds) = bg.buddy.poll_login_token(&prod, &ls).await else {
            return;
        };
        let _ = bg.buddy.poll_login_account(&prod, &mut creds, &ls).await;
        {
            let mut store = bg.store.lock().await;
            buddy_put_creds(&mut store, &src, creds.clone());
            store.save_credentials();
        }
        let _ = buddy_refresh_inner(&bg, &src).await;
    });
    Ok(json!({ "authUrl": auth_url }))
}

#[tauri::command]
pub async fn buddy_login(ferry: FerryState<'_>) -> Result<Value, String> {
    buddy_login_inner(&ferry.state, "buddy").await
}

#[tauri::command]
pub async fn workbuddy_login(ferry: FerryState<'_>) -> Result<Value, String> {
    buddy_login_inner(&ferry.state, "workbuddy").await
}

#[tauri::command]
pub async fn buddy_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    buddy_refresh_inner(&ferry.state, "buddy").await
}

#[tauri::command]
pub async fn workbuddy_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    buddy_refresh_inner(&ferry.state, "workbuddy").await
}

// ------------------------------------------- lobsterai / trae / minimax

fn generic_creds<'a>(store: &'a crate::store::Store, source: &str) -> &'a crate::store::TokenCreds {
    match source {
        "lobsterai" => &store.credentials.lobsterai,
        "trae" => &store.credentials.trae,
        "cline" => &store.credentials.cline,
        _ => &store.credentials.minimax,
    }
}

fn put_generic_creds(store: &mut crate::store::Store, source: &str, creds: crate::store::TokenCreds) {
    match source {
        "lobsterai" => store.credentials.lobsterai = creds,
        "trae" => store.credentials.trae = creds,
        "cline" => store.credentials.cline = creds,
        _ => store.credentials.minimax = creds,
    }
}

/// Pull the model roster for one Bearer-token channel.
async fn generic_refresh_inner(state: &Arc<AppState>, source: &str) -> Result<Value, String> {
    let mut creds = {
        let store = state.store.lock().await;
        generic_creds(&store, source).clone()
    };
    if !creds.ready() {
        return Err(format!("{} 未登录", source));
    }
    // catalog 走的是同一张 access token —— token 临近过期时先按 chat 路径
    // 同款逻辑续期（持全局 refresh_gate，RT 单次消费），否则过期后目录直接
    // 401（minimax 实测）。续期失败不阻断：可能只是时钟抖动，继续试 catalog。
    if creds.expired() || creds.expires_at - crate::store::now_ms() < 5 * 60 * 1000 {
        let _gate = state.refresh_gate.lock().await;
        let refreshed = match source {
            "lobsterai" => state.lobsterai.refresh(&mut creds).await,
            "cline" => state.cline.refresh(&mut creds).await,
            "trae" => Err("TRAE 暂未支持自动续期，请重新登录".to_string()),
            _ => state.minimax.refresh(&mut creds).await,
        };
        match refreshed {
            Ok(()) => {
                let mut store = state.store.lock().await;
                put_generic_creds(&mut store, source, creds.clone());
                store.save_credentials();
            }
            Err(e) if e.starts_with("REFRESH_TERMINAL") => return Err(format!("{} 会话已失效，请重新登录", source)),
            Err(_) => {}
        }
    }
    let models = match source {
        "lobsterai" => state.lobsterai.catalog(&creds).await,
        "trae" => state.trae.catalog(&creds).await,
        "cline" => state.cline.catalog(&creds).await,
        _ => state.minimax.catalog(&creds).await,
    }
    .map_err(|e| format!("读取 {} 模型失败：{}", source, e))?;
    let count = models.len();
    {
        let mut store = state.store.lock().await;
        match source {
            "lobsterai" => store.models.lobsterai = models,
            "trae" => store.models.trae = models,
            "cline" => store.models.cline = models,
            _ => store.models.minimax = models,
        }
        store.models.fetched_at_ms = now_ms();
        store.save_models();
    }
    Ok(json!({ "models": count }))
}

// ----------------------------------------- lobsterai / trae browser login

/// Shared timeout for the one-shot OAuth loopback login windows.
const NATIVE_LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// Context for the LobsterAI loopback callback handler.
struct LobsterLoginCtx {
    state: Arc<AppState>,
    uuid: String,
    first_keyfrom: String,
}

/// Context for the TRAE loopback callback handler.
#[derive(Clone)]
struct TraeLoginCtx {
    state: Arc<AppState>,
    machine_id: String,
    device_id: String,
}

/// 通用成功/失败回显页（与 maodao 同款）。
fn native_login_reply(outcome: Result<TokenCreds, String>) -> (StatusCode, Html<String>) {
    const OK_PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>登录成功</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>登录成功</h1><p>可以关闭这个页面，回到 CreditFerry 面板。</p>"#;
    const FAIL_HEAD: &str = r#"<!doctype html><meta charset="utf-8"><title>登录失败</title>
<body style="font-family:system-ui;padding:48px;text-align:center">
<h1>登录失败</h1><p>原因：</p>"#;
    const TAIL: &str = "<p>可以关闭这个页面。</p>";
    match outcome {
        Ok(_) => (StatusCode::OK, Html(OK_PAGE.to_string())),
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Html(format!("{}{}{}", FAIL_HEAD, message.replace('<', "&lt;"), TAIL)),
        ),
    }
}

async fn lobster_callback(
    AxumState(ctx): AxumState<Arc<LobsterLoginCtx>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let outcome: Result<TokenCreds, String> = if let Some(error) = params.get("error") {
        Err(params.get("error_description").cloned().unwrap_or_else(|| error.clone()))
    } else if let Some(code) = params.get("code") {
        match ctx.state.lobsterai.exchange(code, &ctx.uuid, &ctx.first_keyfrom).await {
            Ok(creds) => {
                {
                    let mut store = ctx.state.store.lock().await;
                    store.credentials.lobsterai = creds.clone();
                    store.credentials.keepalive.blocks.remove("lobsterai");
                    store.save_credentials();
                }
                let _ = generic_refresh_inner(&ctx.state, "lobsterai").await;
                Ok(creds)
            }
            Err(e) => Err(e),
        }
    } else {
        Err("回调缺少授权码".into())
    };
    native_login_reply(outcome)
}

/// 最小 percent-decode（%XX / +），用于解析 TRAE 回调用 URL 编码的 JSON 参数。
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn trae_callback(
    AxumState(ctx): AxumState<Arc<TraeLoginCtx>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> (StatusCode, Html<String>) {
    // 模仿 OFM parseTraeCallbackDetailed：回调在 query 里回吐 refreshToken（或
    // userJwt.RefreshToken）与 userInfo（UserID/ScreenName/TenantID）。
    let parse_json = |raw: &str| -> Option<serde_json::Value> {
        let s = raw.trim();
        if s.is_empty() { return None; }
        serde_json::from_str::<serde_json::Value>(s).ok()
            .or_else(|| percent_decode(s).and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok()))
    };
    let user_info = params.get("userInfo").and_then(|v| parse_json(v));
    let user_jwt = params.get("userJwt").and_then(|v| parse_json(v));
    let refresh_token = params.get("refreshToken").cloned().filter(|s| !s.is_empty())
        .or_else(|| user_jwt.as_ref()
            .and_then(|j| j.get("RefreshToken").or_else(|| j.get("refreshToken")))
            .and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    let access_token_direct = if refresh_token.is_empty() {
        user_jwt.as_ref()
            .and_then(|j| j.get("Token").or_else(|| j.get("token")).or_else(|| j.get("accessToken")))
            .and_then(|v| v.as_str()).unwrap_or("").to_string()
    } else { String::new() };
    let (cb_uid, cb_nick, cb_ent) = user_info.as_ref().map(|u| {
        let g = |k: &str| u.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        (g("UserID"), g("ScreenName"), g("TenantID"))
    }).unwrap_or_default();
    let has_code = params.get("code").or_else(|| params.get("authCode"))
        .or_else(|| params.get("authCodeInfo")).map(|s| !s.is_empty()).unwrap_or(false);

    let outcome: Result<TokenCreds, String> = async {
        let mut creds = if !refresh_token.is_empty() {
            ctx.state.trae.exchange(&refresh_token).await?
        } else if !access_token_direct.is_empty() {
            TokenCreds { access_token: access_token_direct, refresh_token: String::new(), expires_at: 0, ..Default::default() }
        } else if has_code {
            return Err("TRAE 返回了 PKCE 授权码（本实现暂不支持该流程）；请确认授权页是否为新版".into());
        } else {
            return Err("回调未携带 refreshToken / userJwt.Token / code".into());
        };
        // userInfo 里的 uid/nickname 覆盖（ExchangeToken 不返回账号信息）。
        if !cb_uid.is_empty() { creds.uid = cb_uid.clone(); }
        if !cb_nick.is_empty() { creds.nickname = cb_nick.clone(); }
        // 存设备标识，供 chat 头 X-Machine-Id / X-Device-Id 使用。
        creds.extra["machineId"] = json!(ctx.machine_id.clone());
        creds.extra["deviceId"] = json!(ctx.device_id.clone());
        if !cb_ent.is_empty() { creds.extra["enterprise_id"] = json!(cb_ent); }
        // 补 GetUserInfo（NonPlainTextMobile/Email + 确认 uid）。失败不阻塞。
        match ctx.state.trae.fetch_user_info(&creds).await {
            Ok(f) => {
                creds.uid = if f.uid.is_empty() { creds.uid.clone() } else { f.uid };
                creds.nickname = if f.nickname.is_empty() { creds.nickname.clone() } else { f.nickname };
                creds.extra = f.extra;
            }
            Err(e) => { eprintln!("trae fetch_user_info 失败：{}", e); }
        }
        {
            let mut store = ctx.state.store.lock().await;
            store.credentials.trae = creds.clone();
            store.credentials.keepalive.blocks.remove("trae");
            store.save_credentials();
        }
        let _ = generic_refresh_inner(&ctx.state, "trae").await;
        Ok(creds)
    }.await;
    native_login_reply(outcome)
}

/// LobsterAI 浏览器登录：启动临时 loopback + 打开登录页，回调后自动换令牌、落地、拉目录。
async fn lobsterai_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let uuid = crate::store::new_hex(32);
    // 模仿 OFM：firstKeyfrom = 登录会话创建时刻的毫秒时间戳（lobsrateai 用它识别渠道/版本）。
    let first_keyfrom = crate::store::now_ms().to_string();
    let state_param = crate::store::new_hex(16);
    let url = crate::lobsterai::login_url(port, &state_param);
    open_in_browser(&url);
    let ctx = Arc::new(LobsterLoginCtx {
        state: Arc::clone(state),
        uuid,
        first_keyfrom,
    });
    tauri::async_runtime::spawn(async move {
        let app = Router::new()
            .route(crate::lobsterai::CALLBACK_PATH, axum_get(lobster_callback))
            .with_state(ctx);
        let server = axum::serve(listener, app);
        tokio::select! {
            _ = server => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(NATIVE_LOGIN_TIMEOUT_MS)) => {}
        }
    });
    Ok(json!({ "authUrl": url }))
}

/// TRAE 浏览器登录：启动临时 loopback + 打开登录页（无 PKCE，走 machine_id/device_id 原生 IDE 授权），
/// 回调后自动换令牌、补账号、落地、拉目录。
async fn trae_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    // 模仿 OFM generateMachineId / generateDeviceId：每次登录随机生成 32-hex 设备标识。
    let machine_id = crate::store::new_hex(16);
    let device_id = crate::store::new_hex(16);
    let url = crate::trae::login_url(port, &machine_id, &device_id);
    open_in_browser(&url);
    let ctx = Arc::new(TraeLoginCtx {
        state: Arc::clone(state),
        machine_id,
        device_id,
    });
    tauri::async_runtime::spawn(async move {
        let app = Router::new()
            .route(crate::trae::CALLBACK_PATH, axum_get(trae_callback))
            .with_state(ctx);
        let server = axum::serve(listener, app);
        tokio::select! {
            _ = server => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(NATIVE_LOGIN_TIMEOUT_MS)) => {}
        }
    });
    Ok(json!({ "authUrl": url }))
}

#[tauri::command]
pub async fn lobsterai_login(ferry: FerryState<'_>) -> Result<Value, String> {
    lobsterai_login_inner(&ferry.state).await
}

#[tauri::command]
pub async fn trae_login(ferry: FerryState<'_>) -> Result<Value, String> {
    trae_login_inner(&ferry.state).await
}

/// MiniMax Code login: device-code flow — return the verification URL, poll in
/// the background, persist when the user authorizes.
async fn minimax_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let (device_code, verify_url, code_verifier) = state
        .minimax
        .start_device_login()
        .await
        .map_err(|e| format!("minimax 发起登录失败：{}", e))?;
    open_in_browser(&verify_url);
    let bg = Arc::clone(state);
    let dc = device_code.clone();
    let cv = code_verifier.clone();
    tauri::async_runtime::spawn(async move {
        // Poll for a few minutes; the user authorizes in the browser meanwhile.
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(creds) = bg.minimax.poll_device_token(&dc, &cv).await {
                let mut store = bg.store.lock().await;
                store.credentials.minimax = creds;
                store.save_credentials();
                drop(store);
                let _ = generic_refresh_inner(&bg, "minimax").await;
                return;
            }
        }
    });
    Ok(json!({ "authUrl": verify_url }))
}

#[tauri::command]
pub async fn minimax_login(ferry: FerryState<'_>) -> Result<Value, String> {
    minimax_login_inner(&ferry.state).await
}

#[tauri::command]
pub async fn minimax_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    generic_refresh_inner(&ferry.state, "minimax").await
}

// ------------------------------------------------------------ cline / qoder

/// Cline device-code login: open the WorkOS verification page, poll in the
/// background, persist + pull the roster on success.
async fn cline_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let auth = state
        .cline
        .start_device_auth()
        .await
        .map_err(|e| format!("cline 发起登录失败：{}", e))?;
    let device_code = auth.get("deviceCode").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let url = auth
        .get("verificationUriComplete")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| auth.get("verificationUri").and_then(|v| v.as_str()).unwrap_or("").to_string());
    open_in_browser(&url);
    let bg = Arc::clone(state);
    let dc = device_code.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(creds) = bg.cline.poll_device_token(&dc).await {
                let mut store = bg.store.lock().await;
                store.credentials.cline = creds;
                store.save_credentials();
                drop(store);
                let _ = generic_refresh_inner(&bg, "cline").await;
                return;
            }
        }
    });
    Ok(json!({
        "authUrl": url,
        "userCode": auth.get("userCode").and_then(|v| v.as_str()).unwrap_or(""),
    }))
}

#[tauri::command]
pub async fn cline_login(ferry: FerryState<'_>) -> Result<Value, String> {
    cline_login_inner(&ferry.state).await
}

/// Qoder / Qoder CN login (device + PKCE) and daily-quota claim. Inference is
/// gated (WASM signer), so this persists the session and reports the quota.
async fn qoder_login_inner(state: &Arc<AppState>, source: &str) -> Result<Value, String> {
    let product = if source == "qodercn" { &crate::qoder::QODERCN } else { &crate::qoder::QODER };
    let verifier = crate::store::new_hex(32);
    let nonce = crate::store::new_hex(16);
    let machine_id = crate::store::new_hex(16);
    let url = state.qoder.build_auth_url(product, &verifier, &nonce, &machine_id);
    open_in_browser(&url);
    let bg = Arc::clone(state);
    let prod = *product;
    let v = verifier.clone();
    let n = nonce.clone();
    let src = source.to_string();
    tauri::async_runtime::spawn(async move {
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(creds) = bg.qoder.poll_token(&prod, &v, &n).await {
                {
                    let mut store = bg.store.lock().await;
                    if src == "qodercn" {
                        store.credentials.qodercn = creds;
                    } else {
                        store.credentials.qoder = creds;
                    }
                    store.save_credentials();
                }
                // Claim the daily free credits if available.
                let creds = {
                    let store = bg.store.lock().await;
                    if src == "qodercn" { store.credentials.qodercn.clone() } else { store.credentials.qoder.clone() }
                };
                let _ = bg.qoder.claim_daily(&prod, &creds).await;
                return;
            }
        }
    });
    Ok(json!({ "authUrl": url, "gated": true }))
}

#[tauri::command]
pub async fn qoder_login(ferry: FerryState<'_>) -> Result<Value, String> {
    qoder_login_inner(&ferry.state, "qoder").await
}

#[tauri::command]
pub async fn qodercn_login(ferry: FerryState<'_>) -> Result<Value, String> {
    qoder_login_inner(&ferry.state, "qodercn").await
}

/// Pull the Cline roster (needs login).
#[tauri::command]
pub async fn cline_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    generic_refresh_inner(&ferry.state, "cline").await
}

#[tauri::command]
pub async fn lobsterai_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    generic_refresh_inner(&ferry.state, "lobsterai").await
}

#[tauri::command]
pub async fn trae_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    generic_refresh_inner(&ferry.state, "trae").await
}

// ---------------------------------------------------------------- freebuff

/// Import the Freebuff (CodeBuff) desktop login from
/// ~/.config/freebuff-desktop/state.json — the same zero-touch pattern as
/// loomy_import_client / maodao_import_client.
#[tauri::command]
pub async fn freebuff_import_client(ferry: FerryState<'_>) -> Result<Value, String> {
    let imported = freebuff::import_local_state().map_err(|e| format!("导入 Freebuff 失败：{}", e))?;
    let nickname = imported.nickname.clone();
    {
        let mut store = ferry.state.store.lock().await;
        store.credentials.freebuff = imported.to_creds();
        store.save_credentials();
    }
    let refreshed = freebuff_refresh_inner(&ferry.state).await;
    Ok(json!({ "nickname": nickname, "refreshed": refreshed }))
}

/// Catalog + daily freebucks. Read-only GETs — no device signature needed —
/// and never touch the session/admission endpoints, so refreshing costs
/// nothing and cannot burn a daily slot.
pub async fn freebuff_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let creds = {
        let store = state.store.lock().await;
        store.credentials.freebuff.clone()
    };
    if !creds.ready() {
        return Err("freebuff 未登录".into());
    }
    let catalog = state.freebuff.fetch_catalog(&creds).await
        .map_err(|e| format!("读取 Freebuff 目录失败：{}", e))?;
    let session = state.freebuff.fetch_session(&creds).await.unwrap_or_else(|_| json!({}));
    let models = freebuff::FreebuffClient::build_models(&catalog, &session);
    let quota = freebuff::FreebuffClient::extract_quota(&session);
    let count = models.len();
    {
        let mut store = state.store.lock().await;
        store.models.freebuff = models;
        store.models.freebuff_quota = Some(quota.clone());
        store.models.fetched_at_ms = now_ms();
        store.save_models();
    }
    Ok(json!({ "models": count, "quota": quota }))
}

#[tauri::command]
pub async fn freebuff_refresh(ferry: FerryState<'_>) -> Result<Value, String> {
    freebuff_refresh_inner(&ferry.state).await
}

/// Forget the imported Freebuff token and its cached roster.
#[tauri::command]
pub async fn freebuff_logout(ferry: FerryState<'_>) -> Result<Value, String> {
    {
        let mut store = ferry.state.store.lock().await;
        store.credentials.freebuff = TokenCreds::default();
        store.save_credentials();
        store.models.freebuff.clear();
        store.models.freebuff_quota = None;
        store.save_models();
    }
    Ok(json!({ "loggedOut": true }))
}

// ---------------------------------------------------------------- export

/// Merge our entries into the WorkBuddy models.json.
#[tauri::command]
pub async fn export_workbuddy(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    let port = store.settings.port;
    crate::export::export_workbuddy(&store, &format!("http://127.0.0.1:{}/v1", port), &ferry.state.key)
}

/// Remove our entries from the WorkBuddy models.json.
#[tauri::command]
pub async fn cleanup_workbuddy(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::export::cleanup_workbuddy(&store)
}

/// Merge our entries into WorkBuddy International's ~/.workbuddy-ai/models.json.
#[tauri::command]
pub async fn export_workbuddy_intl(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    let port = store.settings.port;
    crate::export::export_workbuddy_intl(&store, &format!("http://127.0.0.1:{}/v1", port), &ferry.state.key)
}

/// Remove our entries from WorkBuddy International's models.json.
#[tauri::command]
pub async fn cleanup_workbuddy_intl(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::export::cleanup_workbuddy_intl(&store)
}

/// Write our provider into Qoder CN's ~/.qoder-cn/settings.json.
#[tauri::command]
pub async fn export_qoder(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    let port = store.settings.port;
    crate::export::export_qoder(&store, &format!("http://127.0.0.1:{}/v1", port), &ferry.state.key)
}

/// Remove our provider from Qoder CN settings.json.
#[tauri::command]
pub async fn cleanup_qoder(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::export::cleanup_qoder(&store)
}

/// Write our provider into Qoder International's ~/.qoder/settings.json.
#[tauri::command]
pub async fn export_qoder_intl(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    let port = store.settings.port;
    crate::export::export_qoder_intl(&store, &format!("http://127.0.0.1:{}/v1", port), &ferry.state.key)
}

/// Remove our provider from Qoder International's settings.json.
#[tauri::command]
pub async fn cleanup_qoder_intl(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::export::cleanup_qoder_intl(&store)
}

/// One-click configure DeepSeek Harness desktop (provider + API key).
#[tauri::command]
pub async fn export_dsh(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    let port = store.settings.port;
    crate::export::export_dsh(&store, &format!("http://127.0.0.1:{}/v1", port), &ferry.state.key)
}

/// Remove our provider + key from DeepSeek Harness.
#[tauri::command]
pub async fn cleanup_dsh(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::export::cleanup_dsh(&store)
}

/// Two community connectors that pull WorkBuddy / Qoder models *into* DSH.
#[tauri::command]
pub async fn dsh_connect_status(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    Ok(crate::dsh_connect::status(&store))
}

/// install | upgrade | enable | disable | uninstall, for one profile.
#[tauri::command]
pub async fn dsh_connect_apply(
    ferry: FerryState<'_>,
    platform: String,
    profile: String,
    action: String,
) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    crate::dsh_connect::apply(&store, &platform, &profile, &action)
}

/// Which export targets exist on this machine (panel button hints).
#[tauri::command]
pub async fn get_snippets(ferry: FerryState<'_>) -> Result<Value, String> {
    let store = ferry.state.store.lock().await;
    Ok(json!({
        "owner": OWNER,
        "qoderProviderId": crate::export::qoder_provider_id(store.settings.port),
        "qoderExists": crate::export::resolve_qoder_settings(&store)
            .map(|f| f.is_file())
            .unwrap_or(false),
        "qoderIntlExists": crate::export::resolve_qoder_settings_intl(&store)
            .map(|f| f.is_file())
            .unwrap_or(false),
        "dshExists": crate::export::resolve_dsh_patch(&store)
            .map(|f| f.is_file())
            .unwrap_or(false),
        "dshConnectFound": crate::dsh_connect::status(&store)
            .get("found")
            .and_then(|f| f.as_bool())
            .unwrap_or(false),
        "intlExists": crate::export::resolve_models_file_intl(&store)
            .map(|f| f.parent().map(|p| p.is_dir()).unwrap_or(false))
            .unwrap_or(false),
    }))
}

/// Point the export at a specific models.json path.
#[tauri::command]
pub async fn set_models_file(ferry: FerryState<'_>, path: String) -> Result<Value, String> {
    let mut store = ferry.state.store.lock().await;
    store.settings.models_file = Some(path);
    store.save_settings();
    Ok(json!({ "ok": true }))
}
