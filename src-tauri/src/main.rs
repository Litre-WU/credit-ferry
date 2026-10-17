#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod buddy;
mod cline;
mod commands;
mod dsh_connect;
mod export;
mod keepalive;
mod kilo;
mod loomy;
mod lobsterai;
mod maodao;
mod minimax;
mod opencode;
mod probe;
mod qoder;
mod raccoon;
mod server;
mod store;
mod trae;
mod zen;

use std::sync::Arc;

use commands::Ferry;
use server::AppState;
use store::Store;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewWindow,
};

fn data_dir(app: &tauri::App) -> std::path::PathBuf {
    // Keep the documented, human-friendly location: %APPDATA%\CreditFerry.
    if let Ok(appdata) = std::env::var("APPDATA") {
        return std::path::Path::new(&appdata).join("CreditFerry");
    }
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let dir = data_dir(app);
            let _ = std::fs::create_dir_all(&dir);
            let store = Store::open(dir);
            let key = store.load_or_create_api_key();
            let port = store.settings.port;
            // A second copy can never serve: the forwarding port and the store
            // files belong to the instance already running. Exit before any
            // window exists rather than leaving a dead panel on screen.
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
                eprintln!(
                    "端口 {} 已被占用，CreditFerry 已在运行中，本次启动退出",
                    port
                );
                app.handle().exit(0);
                return Ok(());
            }
            let state = {
                let zen_dir = store.dir.clone();
                Arc::new(AppState {
                store: tokio::sync::Mutex::new(store),
                loomy: loomy::LoomyClient::new(loomy::LOOMY_BASE),
                raccoon: raccoon::RaccoonClient::new(),
                opencode: opencode::OpenCodeClient::new(),
                opencode_runtime: tokio::sync::Mutex::new(None),
                kilo: kilo::KiloClient::new(),
                zen: zen::ZenClient::new(),
                buddy: buddy::BuddyClient::new(),
                lobsterai: lobsterai::LobsterClient::new(),
                trae: trae::TraeClient::new(),
                minimax: minimax::MinimaxClient::new(),
                cline: cline::ClineClient::new(),
                qoder: qoder::QoderClient::new(),
                zen_sessions: tokio::sync::Mutex::new(zen::load_session_map(&zen_dir)),
                maodao: maodao::MaodaoClient::new(),
                maodao_login: tokio::sync::Mutex::new(None),
                key,
                sem: Arc::new(tokio::sync::Semaphore::new(server::MAX_CONCURRENCY)),
                maodao_sem: Arc::new(tokio::sync::Semaphore::new(server::MAODAO_CONCURRENCY)),
                refresh_gate: tokio::sync::Mutex::new(()),
                keepalive_started: std::sync::atomic::AtomicBool::new(false),
                })
            };
            let ferry = Arc::new(Ferry {
                state,
                qr: tokio::sync::Mutex::new(None),
                server_task: tokio::sync::Mutex::new(None),
            });
            app.manage(ferry.clone());

            // P1-3 keepalive: renewal is a property of the *credentials*,
            // not of the forwarding port, so it starts unconditionally.
            {
                let ka_state = Arc::clone(&ferry.state);
                commands::start_keepalive(&ka_state);
            }

            // Tray: left-click shows the panel; menu exposes show + quit.
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let ferry_for_tray = ferry.clone();
            TrayIconBuilder::with_id("main-tray")
                .tooltip("CreditFerry 积分摆渡")
                .show_menu_on_left_click(false)
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        // Stop the isolated OpenCode child, remove our entries
                        // from WorkBuddy, then exit — all fast sync operations,
                        // done inline so nothing is lost to the exit race.
                        if let Ok(mut guard) = ferry_for_tray.state.opencode_runtime.try_lock() {
                            if let Some(mut runtime) = guard.take() {
                                runtime.stop();
                            }
                        }
                        if let Ok(store) = ferry_for_tray.state.store.try_lock() {
                            let _ = export::cleanup_workbuddy(&store);
                            let _ = export::cleanup_workbuddy_intl(&store);
                            let _ = export::cleanup_qoder(&store);
                            let _ = export::cleanup_qoder_intl(&store);
                            let _ = export::cleanup_dsh(&store);
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Close hides to tray; quit happens through the tray menu.
            // The window is built here (not from config) because the CDP
            // browser argument must be set before the webview is created.
            // 9222 is often already owned by another Chromium on this machine;
            // CREDITFERRY_CDP_PORT lets a panel check actually attach.
            let cdp_port = std::env::var("CREDITFERRY_CDP_PORT").unwrap_or_else(|_| "9222".to_string());
            let window: WebviewWindow = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("CreditFerry 积分摆渡")
            .inner_size(1140.0, 800.0)
            .min_inner_size(900.0, 620.0)
            .background_color(tauri::webview::Color(0xf5, 0xf6, 0xf8, 0xff))
            .additional_browser_args(&format!("--remote-debugging-port={}", cdp_port))
            .build()?;
            let window_clone = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window_clone.hide();
                }
            });

            // Auto-start the forwarding service when enabled.
            let auto_start = {
                let store = ferry.state.store.try_lock();
                store.map(|s| s.settings.auto_start_server).unwrap_or(true)
            };
            if auto_start {
                let ferry = ferry.clone();
                tauri::async_runtime::spawn(async move {
                    let state = Arc::clone(&ferry.state);
                    let handle = tauri::async_runtime::spawn(async move {
                        if let Err(e) = server::serve(state.clone()).await {
                            eprintln!("ferry server exited: {}", e);
                        }
                    });
                    *ferry.server_task.lock().await = Some(handle);
                    // Bring up the isolated OpenCode runtime in the background
                    // (first run downloads the binary, hence the patience).
                    let oc_state = Arc::clone(&ferry.state);
                    tauri::async_runtime::spawn(async move {
                        match commands::ensure_opencode_runtime(&oc_state).await {
                            Ok(true) => {
                                let _ = commands::opencode_refresh_inner(&oc_state).await;
                            }
                            Ok(false) => {}
                            Err(e) => eprintln!("opencode runtime start failed: {}", e),
                        }
                    });
                    // Refresh logged-in upstreams in the background so the
                    // model list is populated by the time the panel opens.
                    let refresh_state = Arc::clone(&ferry.state);
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        commands::refresh_all(&refresh_state).await;
                    });
                });
            }

            eprintln!("CreditFerry ready on http://127.0.0.1:{}", port);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_snapshot,
            commands::get_api_key,
            commands::server_start,
            commands::server_stop,
            commands::server_status,
            commands::opencode_start,
            commands::opencode_stop,
            commands::opencode_refresh,
            commands::probe_models,
            commands::loomy_import_client,
            commands::loomy_set_session,
            commands::loomy_select,
            commands::loomy_forget,
            commands::loomy_refresh,
            commands::raccoon_qr_start,
            commands::raccoon_qr_status,
            commands::raccoon_logout,
            commands::raccoon_refresh,
            commands::maodao_import_client,
            commands::maodao_login_start,
            commands::maodao_login_status,
            commands::maodao_logout,
            commands::maodao_refresh,
            commands::kilo_refresh,
            commands::zen_refresh,
            commands::buddy_login,
            commands::buddy_refresh,
            commands::workbuddy_login,
            commands::workbuddy_refresh,
            commands::minimax_login,
            commands::minimax_refresh,
            commands::lobsterai_refresh,
            commands::trae_refresh,
            commands::cline_login,
            commands::cline_refresh,
            commands::qoder_login,
            commands::qodercn_login,
            commands::keepalive_run_once,
            commands::keepalive_status,
            commands::export_workbuddy,
            commands::cleanup_workbuddy,
            commands::export_workbuddy_intl,
            commands::cleanup_workbuddy_intl,
            commands::export_qoder,
            commands::cleanup_qoder,
            commands::export_qoder_intl,
            commands::cleanup_qoder_intl,
            commands::export_dsh,
            commands::cleanup_dsh,
            commands::dsh_connect_status,
            commands::dsh_connect_apply,
            commands::get_snippets,
            commands::set_models_file,
        ])
        .build(tauri::generate_context!())
        .expect("error while building CreditFerry")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // Final cleanup happens through the tray quit handler.
            }
        });
}
