//! Tray icon: open the window, start/stop the gateway, quit. The icon shows whether it runs.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::commands::AppState;
use crate::i18n::t;

const TRAY_ID: &str = "main";
const LIVE_ICON: &[u8] = include_bytes!("../icons/tray-live.png");
const IDLE_ICON: &[u8] = include_bytes!("../icons/tray-idle.png");

pub struct TrayItems {
    open: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    quit: MenuItem<Wry>,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    // Texts are set by refresh() in the current language.
    let open = MenuItem::with_id(app, "open", "Retranslator", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "…", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &toggle, &PredefinedMenuItem::separator(app)?, &quit_item])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(IDLE_ICON)?)
        .tooltip("Retranslator")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => crate::show_main_window(app),
            "toggle" => toggle_server(app),
            "quit" => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(TrayItems { open, toggle, quit: quit_item });
    refresh(app);
    Ok(())
}

/// Syncs menu text, tooltip and icon with the server state.
pub fn refresh(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let status = state.server.status();
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.open.set_text(t("Открыть Retranslator", "Open Retranslator"));
        let _ = items.toggle.set_text(if status.running {
            t("Остановить сервер", "Stop server")
        } else {
            t("Запустить сервер", "Start server")
        });
        let _ = items.quit.set_text(t("Выход", "Quit"));
    }
    let Some(tray): Option<TrayIcon> = app.tray_by_id(TRAY_ID) else { return };
    let tooltip = match (&status.running, &status.error) {
        (true, _) => t(
            format!("Retranslator — работает на 127.0.0.1:{}", status.port),
            format!("Retranslator — running on 127.0.0.1:{}", status.port),
        ),
        (false, Some(_)) => t("Retranslator — ошибка сервера", "Retranslator — server error").to_string(),
        (false, None) => t("Retranslator — остановлен", "Retranslator — stopped").to_string(),
    };
    let _ = tray.set_tooltip(Some(&tooltip));
    if let Ok(icon) = Image::from_bytes(if status.running { LIVE_ICON } else { IDLE_ICON }) {
        let _ = tray.set_icon(Some(icon));
    }
}

fn toggle_server(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if state.server.status().running {
            state.server.stop(&state.core).await;
        } else {
            let _ = state.server.start(state.core.clone()).await;
        }
    });
}

fn quit_app(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        state.server.stop(&state.core).await;
        app.exit(0);
    });
}
