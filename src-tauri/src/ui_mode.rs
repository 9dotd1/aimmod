//! Minimal vs analysis UI mode.
//!
//! AimMod starts in **minimal mode**: no webview windows exist, so no
//! WebView2 (msedgewebview2) processes run. Every tracker — mouse hook, replay
//! capture, Hub sync/uploads and the UE4SS bridge — lives in Rust and keeps
//! working without any window.
//!
//! **Analysis mode** builds the overlay, logs and stats windows from their
//! `tauri.conf.json` definitions (declared with `create: false`). Leaving
//! analysis mode destroys them so WebView2 shuts down again.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::CheckMenuItem;
use tauri::{AppHandle, Manager, WebviewWindowBuilder, Wry};

/// Window labels, in creation order. They must match `tauri.conf.json`.
const WINDOW_LABELS: [&str; 3] = ["overlay", "logs", "stats"];

static ANALYSIS_MODE: AtomicBool = AtomicBool::new(false);
/// Serializes transitions so overlapping toggles never build or destroy
/// windows concurrently.
static TRANSITION: Mutex<()> = Mutex::new(());
static TRAY_ITEM: Mutex<Option<CheckMenuItem<Wry>>> = Mutex::new(None);

pub fn is_analysis_mode() -> bool {
    ANALYSIS_MODE.load(Ordering::SeqCst)
}

/// Register the tray check item that mirrors the current mode.
pub fn register_tray_item(item: CheckMenuItem<Wry>) {
    let _ = item.set_checked(is_analysis_mode());
    *TRAY_ITEM.lock().unwrap() = Some(item);
}

/// Switch modes on a worker thread. Window creation blocks on the event loop,
/// so it must never run on the main thread or inside a sync command.
pub fn set_analysis_mode(app: &AppHandle, enabled: bool) {
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("ui-mode".into())
        .spawn(move || apply(&app, enabled));
}

pub fn toggle_analysis_mode(app: &AppHandle) {
    set_analysis_mode(app, !is_analysis_mode());
}

fn apply(app: &AppHandle, enabled: bool) {
    let _guard = TRANSITION.lock().unwrap_or_else(|e| e.into_inner());
    if enabled {
        open_windows(app);
    } else {
        close_windows(app);
    }
    ANALYSIS_MODE.store(enabled, Ordering::SeqCst);
    if let Some(item) = TRAY_ITEM.lock().unwrap().as_ref() {
        let _ = item.set_checked(enabled);
    }
    log::info!(
        "UI mode: {}",
        if enabled { "analysis (webviews open)" } else { "minimal (no webviews)" }
    );
}

fn open_windows(app: &AppHandle) {
    let configs = app.config().app.windows.clone();
    for label in WINDOW_LABELS {
        if app.get_webview_window(label).is_some() {
            continue;
        }
        let Some(config) = configs.iter().find(|c| c.label == label) else {
            log::error!("ui_mode: window '{label}' missing from tauri.conf.json");
            continue;
        };
        match WebviewWindowBuilder::from_config(app, config).and_then(|b| b.build()) {
            Ok(_) => {}
            Err(e) => log::error!("ui_mode: failed to create '{label}' window: {e}"),
        }
    }

    crate::apply_window_titles(app);

    if let Some(win) = app.get_webview_window("overlay") {
        crate::prepare_overlay_window(&win);
        let index = app
            .state::<crate::AppState>()
            .settings
            .lock()
            .map(|s| s.monitor_index)
            .unwrap_or(0);
        crate::apply_monitor(app, index);
    }

    if let Some(win) = app.get_webview_window("logs") {
        let logs = win.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = logs.hide();
            }
        });
    }

    if let Some(win) = app.get_webview_window("stats") {
        let handle = app.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Closing the analysis window returns to minimal mode, which
                // tears down every webview instead of hiding it.
                api.prevent_close();
                set_analysis_mode(&handle, false);
            }
        });
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn close_windows(app: &AppHandle) {
    crate::window_tracker::set_force_show(false);
    for label in WINDOW_LABELS {
        if let Some(win) = app.get_webview_window(label) {
            if let Err(e) = win.destroy() {
                log::warn!("ui_mode: failed to destroy '{label}' window: {e}");
            }
        }
    }
}
