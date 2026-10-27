//! Retranslator: a local Anthropic-compatible gateway that relays Claude Desktop requests to any
//! provider, swapping model ids on the way.

pub mod commands;
pub mod config;
pub mod convert;
pub mod i18n;
pub mod logs;
pub mod mapping;
pub mod providers;
pub mod server;
pub mod state;
pub mod tray;

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

use commands::AppState;
use state::{Core, Notice};

const MAIN_WINDOW: &str = "main";

/// Shows the main window, creating it again if it was closed (closing destroys the WebView).
/// Window creation is moved off the calling thread: building a webview inside a tray or menu
/// event handler deadlocks on Windows.
pub fn show_main_window(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = open_main_window(&app) {
            eprintln!("window: {e}");
        }
    });
}

fn open_main_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        window.unminimize()?;
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    let config = app.config().app.windows.iter().find(|w| w.label == MAIN_WINDOW).cloned();
    let window = match config {
        Some(cfg) => WebviewWindowBuilder::from_config(app, &cfg)?.build()?,
        None => WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::default()).title("Retranslator").build()?,
    };
    window.set_focus()?;
    Ok(())
}

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| show_main_window(app)))
        // Launch at login goes straight into the tray (see `--hidden` below).
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--hidden"])))
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            let core = Core::load(config::config_path(&dir), Box::new(config::KeyringStore))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let core = Arc::new(core);
            let handle = app.handle().clone();
            core.set_notifier(Arc::new(move |notice| match notice {
                Notice::Log(entry) => {
                    let _ = handle.emit("log-entry", entry);
                }
                Notice::State => {
                    let _ = handle.emit("state-changed", ());
                    tray::refresh(&handle);
                }
            }));
            let server = Arc::new(server::Server::default());
            app.manage(AppState { core: core.clone(), server: server.clone(), app: app.handle().clone() });
            tray::create(app.handle())?;

            if core.config().autostart_server {
                tauri::async_runtime::spawn(async move {
                    let _ = server.start(core).await;
                });
            }
            // `--hidden` starts straight into the tray (e.g. from autorun).
            if !std::env::args().any(|a| a == "--hidden") {
                open_main_window(app.handle())?;
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing frees the WebView entirely; the process, tray and server stay.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let window = window.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = window.destroy();
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::start_server,
            commands::stop_server,
            commands::set_port,
            commands::set_autostart_server,
            commands::set_autostart_app,
            commands::set_language,
            commands::regenerate_key,
            commands::save_provider,
            commands::get_provider_key,
            commands::delete_provider,
            commands::fetch_models,
            commands::test_provider,
            commands::save_rule,
            commands::delete_rule,
            commands::set_rule_enabled,
            commands::check_local_id,
            commands::get_logs,
            commands::clear_logs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Retranslator");

    app.run(|_app, event| {
        // The last window closing must not end the app: only "Выход" in the tray does (code is Some).
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}
