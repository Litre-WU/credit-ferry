//! Tauri command layer: the panel talks to Rust through these, never HTTP.

use std::sync::Arc;

use qrcode::render::svg;
use qrcode::QrCode;
use serde_json::{json, Value};
use tauri::State;
use tokio::sync::Mutex;

use crate::loomy::import_client_sessions;
use crate::maodao::{self, MaodaoLoginStatus};
use crate::raccoon::{QrStatus, RaccoonClient};
use crate::server::{self, AppState};
use crate::store::{now_ms, OWNER};

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

/// Refresh every logged-in upstream (used on server start).
pub async fn refresh_all(state: &Arc<AppState>) {
    let _ = loomy_refresh_inner(state).await;
    let _ = raccoon_refresh_inner(state).await;
    let _ = opencode_refresh_inner(state).await;
    let _ = maodao_refresh_inner(state).await;
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
    let source = if source == "raccoon" || source == "opencode" || source == "maodao" { source } else { "loomy".to_string() };
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
    store.models.raccoon.clear();
    store.save_credentials();
    store.save_models();
    Ok(json!({ "ok": true }))
}

/// Refresh the Raccoon catalogue + credit balance (auto-rotates the token).
pub async fn raccoon_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
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
        store.save_credentials();
    }
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
fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
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
    store.models.maodao.clear();
    store.save_credentials();
    store.save_models();
    Ok(json!({ "ok": true }))
}

/// Refresh the Maodao catalogue + token balance (auto-rotates the session).
pub async fn maodao_refresh_inner(state: &Arc<AppState>) -> Result<Value, String> {
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
        store.save_credentials();
    }
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
