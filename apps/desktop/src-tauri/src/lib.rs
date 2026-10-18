//! The desktop shell.
//!
//! The window is not the application: the gateway behind it is. This shell starts the
//! same HTTP server the headless binary runs, then loads the same GUI the gateway serves
//! on its port. One process, one configuration file, and no way for the window and the
//! API to disagree about what is configured.
use std::sync::Arc;

use gateway_server::state::AppState;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WindowEvent,
};

/// The label Tauri gives the single window declared in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";
const SHOW_WINDOW: &str = "show-window";
const QUIT: &str = "quit";

/// Injects the gateway's origin into the page.
///
/// The webview is served from Tauri's own asset protocol, so the GUI's relative requests
/// would otherwise never reach the gateway.
const GATEWAY_ORIGIN: &str = "window.__LLM_GATEWAY_BASE__ = '{origin}'";

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = match gateway_server::config_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("llm-gateway: {error}");
            std::process::exit(1);
        }
    };

    let state = match AppState::load(config_path.clone()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "llm-gateway: {error}\nconfiguration: {}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    let origin = gateway_origin(&state);
    tauri::Builder::default()
        .setup(move |app| {
            let state = state.clone();
            // The server runs for the life of the process rather than the life of the
            // window: closing the GUI must not take the API down with it.
            tauri::async_runtime::spawn(async move {
                if let Err(error) = gateway_server::serve(state).await {
                    tracing::error!(%error, "the gateway stopped serving");
                }
            });
            app.manage(origin);
            install_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // A window close is not an application exit: the gateway keeps serving after
            // the GUI is dismissed, and the tray icon is how it comes back. Destroying the
            // window instead would leave nothing to show again.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .on_page_load(|window, _payload| {
            let origin = window.state::<String>().inner().clone();
            let _ = window.eval(GATEWAY_ORIGIN.replace("{origin}", &origin));
        })
        .build(tauri::generate_context!())
        .expect("the desktop shell could not be built")
        // Exiting is the tray menu's job, so nothing here has to hold the process open.
        .run(|_app, _event| {});
}

/// Puts the application in the notification area.
///
/// Closing the window only hides it, so without a tray icon there would be no way back to
/// the window and no way to quit short of the task manager — the shell is often launched
/// elevated, which puts its process out of reach of an ordinary one.
fn install_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, SHOW_WINDOW, "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("llm-gateway")
        .icon(
            app.default_window_icon()
                .expect("the bundle always carries the window icon")
                .clone(),
        )
        .tooltip("LLM Gateway")
        .menu(&menu)
        // The menu belongs on the right button; a left click brings the window back.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            SHOW_WINDOW => reveal_window(app),
            QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// The origin the GUI should call, taken from the configuration the gateway loaded.
fn gateway_origin(state: &Arc<AppState>) -> String {
    let server = &state.snapshot().config.server;
    // `0.0.0.0` and `::` are listen addresses, not reachable ones.
    let host = match server.bind.as_str() {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{}", server.port)
}
