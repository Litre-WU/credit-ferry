//! Tauri command layer: the panel talks to Rust through these, never HTTP.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query, State as AxumState};
use axum::response::Html;
use axum::routing::get as axum_get;
use axum::{http::StatusCode, Router};
use qrcode::render::svg;
use qrcode::QrCode;
use serde_json::{json, Value};
use tauri::State;
use tokio::sync::Mutex;

use crate::loomy::import_client_sessions;
use crate::maodao::{self, MaodaoLoginStatus};
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
    let verdicts = crate::probe::probe_source_bounded(state, source).await;
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
    let creds = {
        let store = state.store.lock().await;
        generic_creds(&store, source).clone()
    };
    if !creds.ready() {
        return Err(format!("{} 未登录", source));
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
    redirect_uri: String,
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

async fn trae_callback(
    AxumState(ctx): AxumState<Arc<TraeLoginCtx>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let outcome: Result<TokenCreds, String> = if let Some(error) = params.get("error") {
        Err(params.get("error_description").cloned().unwrap_or_else(|| error.clone()))
    } else if let Some(code) = params.get("code") {
        match ctx.state.trae.exchange(code, &ctx.redirect_uri).await {
            Ok(creds) => {
                // 补账号信息（uid 供设备指纹派生）。失败也不阻塞：先落基础凭据。
                let full = match ctx.state.trae.fetch_user_info(&creds).await {
                    Ok(mut f) => {
                        f.access_token = creds.access_token.clone();
                        f.refresh_token = creds.refresh_token.clone();
                        f.expires_at = creds.expires_at;
                        f.extra = creds.extra.clone();
                        f
                    }
                    Err(e) => {
                        eprintln!("trae fetch_user_info 失败：{}", e);
                        creds
                    }
                };
                {
                    let mut store = ctx.state.store.lock().await;
                    store.credentials.trae = full.clone();
                    store.credentials.keepalive.blocks.remove("trae");
                    store.save_credentials();
                }
                let _ = generic_refresh_inner(&ctx.state, "trae").await;
                Ok(full)
            }
            Err(e) => Err(e),
        }
    } else {
        Err("回调缺少授权码".into())
    };
    native_login_reply(outcome)
}

/// LobsterAI 浏览器登录：启动临时 loopback + 打开登录页，回调后自动换令牌、落地、拉目录。
async fn lobsterai_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let uuid = crate::store::new_hex(32);
    let first_keyfrom = crate::lobsterai::FIRST_KEYFROM.to_string();
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

/// TRAE 浏览器登录：启动临时 loopback + 打开登录页（PKCE），回调后自动换令牌、补账号、落地、拉目录。
async fn trae_login_inner(state: &Arc<AppState>) -> Result<Value, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("无法监听本地回调端口：{}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let state_param = crate::store::new_hex(16);
    let code_verifier = crate::store::new_hex(64);
    let redirect_uri = format!("http://127.0.0.1:{}{}", port, crate::trae::CALLBACK_PATH);
    let url = crate::trae::login_url(port, &state_param, &code_verifier);
    open_in_browser(&url);
    let ctx = Arc::new(TraeLoginCtx {
        state: Arc::clone(state),
        redirect_uri,
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
    let (device_code, verify_url) = state
        .minimax
        .start_device_login()
        .await
        .map_err(|e| format!("minimax 发起登录失败：{}", e))?;
    open_in_browser(&verify_url);
    let bg = Arc::clone(state);
    tauri::async_runtime::spawn(async move {
        // Poll for a few minutes; the user authorizes in the browser meanwhile.
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(creds) = bg.minimax.poll_device_token(&device_code).await {
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
