//! Codex Widget — Tauri shell.
//!
//! Responsibilities kept here (and deliberately *not* in the renderer):
//!   • owning the polling thread that feeds `watcher`
//!   • the tray menu, which is the only settings surface the app has
//!   • window mechanics: transparent, always-on-top, sized to the capsule,
//!     click-through while idle, parked bottom-right on first launch
//!   • persisting the two toggles

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod watcher;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewWindow, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use watcher::{Snapshot, Watcher};

const POLL_MS: u64 = 500;

/// A turn that ran at least this long means you had already looked away, so it
/// earns the full-screen treatment instead of just the capsule.
const LONG_RUN_SECS: u32 = 600;

/// `CODEX_WIDGET_LONG_SECS=5` shortens the threshold so the overlay can be
/// exercised without waiting ten minutes.
fn long_run_secs() -> u32 {
    std::env::var("CODEX_WIDGET_LONG_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(LONG_RUN_SECS)
}

/* ----------------------------------------------------------------- settings */

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    notify_on_complete: bool,
    show_widget: bool,
    /// full-screen overlay for turns that ran ≥ `LONG_RUN_SECS`
    fullscreen_on_long: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_on_complete: true,
            show_widget: true,
            fullscreen_on_long: true,
        }
    }
}

/// What the overlay renders; replayed on request so a reload cannot miss it.
#[derive(Clone, Serialize)]
struct OverlayPayload {
    kind: String,
    label: String,
    elapsed: u32,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    last_overlay: Mutex<Option<OverlayPayload>>,
    /// whether the two overlay windows are currently on screen
    overlay_open: Mutex<bool>,
    /// true once the renderer has painted; the window stays hidden until then
    /// so a transparent window never flashes white on launch
    armed: Mutex<bool>,
}

impl Shared {
    fn persist(&self) {
        let settings = self.settings.lock().unwrap().clone();
        if let Ok(text) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(&self.settings_path, text);
        }
    }
}

fn load_settings(path: &PathBuf) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Where `settings.json` lives, resolved without an `AppHandle` so the state can
/// be registered on the Builder before any webview exists.
fn app_config_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("io.github.codexwidget"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/* ---------------------------------------------------------------- sessions */

fn codex_home() -> Option<PathBuf> {
    // CODEX_WIDGET_HOME points at a *`.codex` directory* and exists so the app
    // can be demoed and tested against synthetic sessions.
    if let Ok(custom) = std::env::var("CODEX_WIDGET_HOME") {
        if !custom.is_empty() {
            return Some(PathBuf::from(custom));
        }
    }
    std::env::var("USERPROFILE")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex"))
}

/* ---------------------------------------------------------------- commands */

#[tauri::command]
fn snapshot(shared: State<'_, Arc<Shared>>) -> Snapshot {
    shared.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn get_settings(shared: State<'_, Arc<Shared>>) -> Settings {
    shared.settings.lock().unwrap().clone()
}

#[tauri::command]
fn set_visible(window: WebviewWindow, visible: bool) -> Result<(), String> {
    // Idle means "invisible *and* not in the way": the window keeps running so
    // nothing has to be re-shown (which would steal focus), but the cursor goes
    // straight through it.
    window
        .set_ignore_cursor_events(!visible)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn resize_to(window: WebviewWindow, shared: State<'_, Arc<Shared>>, width: f64, height: f64) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let size = PhysicalSize::new(
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    );
    let position = window.outer_position().map_err(|e| e.to_string())?;
    let previous = window.outer_size().map_err(|e| e.to_string())?;

    window.set_size(tauri::Size::Physical(size)).map_err(|e| e.to_string())?;

    // Keep the right edge pinned: the widget lives in a corner and should grow
    // leftwards, not drift off the screen edge.
    let dx = previous.width as i32 - size.width as i32;
    if dx != 0 {
        window
            .set_position(tauri::Position::Physical(PhysicalPosition::new(
                position.x + dx,
                position.y,
            )))
            .map_err(|e| e.to_string())?;
    }

    if !*shared.armed.lock().unwrap() {
        park_bottom_right(&window)?;
        window.show().map_err(|e| e.to_string())?;
        *shared.armed.lock().unwrap() = true;
    }
    Ok(())
}

#[tauri::command]
fn ready(window: WebviewWindow, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    let mut armed = shared.armed.lock().unwrap();
    if !*armed {
        park_bottom_right(&window)?;
        window.show().map_err(|e| e.to_string())?;
        *armed = true;
    }
    Ok(())
}

/// Rust owns the "is this worth a full-screen interruption?" decision so the
/// threshold and the user's toggle live in exactly one place.
/// Two windows, because they need opposite input behaviour:
///   `overlay`       full-screen ambient layer, click-through, keeps pulsing
///   `overlay-card`  small centred card, interactive — clicking it dismisses
const OVERLAY_LABEL: &str = "overlay";
const CARD_LABEL: &str = "overlay-card";

#[tauri::command]
fn maybe_flash_overlay(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    kind: String,
    label: String,
    elapsed: u32,
) -> Result<bool, String> {
    let enabled = shared.settings.lock().unwrap().fullscreen_on_long;
    if !enabled || elapsed < long_run_secs() {
        return Ok(false);
    }

    let ambient = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "overlay window is not configured".to_string())?;
    let card = app
        .get_webview_window(CARD_LABEL)
        .ok_or_else(|| "overlay card window is not configured".to_string())?;

    let monitor = ambient
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no primary monitor".to_string())?;

    // ambient layer: exactly the monitor, fully click-through
    ambient
        .set_size(tauri::Size::Physical(*monitor.size()))
        .map_err(|e| e.to_string())?;
    ambient
        .set_position(tauri::Position::Physical(*monitor.position()))
        .map_err(|e| e.to_string())?;
    ambient
        .set_always_on_top(true)
        .map_err(|e| e.to_string())?;
    ambient
        .set_ignore_cursor_events(true)
        .map_err(|e| e.to_string())?;
    ambient.show().map_err(|e| e.to_string())?;

    // card: centred, interactive, and shown last so it sits above the layer
    let card_size = card.outer_size().map_err(|e| e.to_string())?;
    let x = monitor.position().x + (monitor.size().width as i32 - card_size.width as i32) / 2;
    let y = monitor.position().y + (monitor.size().height as i32 - card_size.height as i32) / 2;
    card
        .set_position(tauri::Position::Physical(PhysicalPosition::new(x, y)))
        .map_err(|e| e.to_string())?;
    card.set_always_on_top(true).map_err(|e| e.to_string())?;
    card.show().map_err(|e| e.to_string())?;

    let payload = OverlayPayload {
        kind,
        label,
        elapsed,
    };
    *shared.last_overlay.lock().unwrap() = Some(payload.clone());
    *shared.overlay_open.lock().unwrap() = true;

    app.emit_to(OVERLAY_LABEL, "overlay://play", payload.clone())
        .map_err(|e| e.to_string())?;
    app.emit_to(CARD_LABEL, "overlay://play", payload)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// The single way out: the card was clicked, or a new turn started.
#[tauri::command]
fn dismiss_overlay(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    {
        let mut open = shared.overlay_open.lock().unwrap();
        if !*open {
            return Ok(());
        }
        *open = false;
    }

    // let both windows fade themselves out, then take them off screen
    let _ = app.emit_to(OVERLAY_LABEL, "overlay://closing", ());
    let _ = app.emit_to(CARD_LABEL, "overlay://closing", ());

    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(260));
        for label in [OVERLAY_LABEL, CARD_LABEL] {
            if let Some(window) = handle.get_webview_window(label) {
                let _ = window.hide();
            }
        }
    });
    Ok(())
}

/// A freshly loaded webview asks for the payload it may have missed. Answers
/// only while the overlay is open, so a dismissed overlay cannot come back.
#[tauri::command]
fn overlay_last(shared: State<'_, Arc<Shared>>) -> Option<OverlayPayload> {
    if !*shared.overlay_open.lock().unwrap() {
        return None;
    }
    shared.last_overlay.lock().unwrap().clone()
}

/// Temporary: surfaces renderer exceptions in the dev console.
#[tauri::command]
fn js_log(message: String) {
    eprintln!("[js] {message}");
}

fn park_bottom_right(window: &WebviewWindow) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let size = window.outer_size().map_err(|e| e.to_string())?;
    let monitor = match window.current_monitor() {
        Ok(Some(monitor)) => monitor,
        _ => return Ok(()),
    };
    let margin_x = (24.0 * scale) as i32;
    // Windows 11 draws its toasts in that same corner, and a finished turn is
    // exactly when one arrives. Park far enough up that the toast never covers
    // the capsule.
    let margin_y = (168.0 * scale) as i32;
    let screen = monitor.size();
    let origin = monitor.position();
    let x = origin.x + screen.width as i32 - size.width as i32 - margin_x;
    let y = origin.y + screen.height as i32 - size.height as i32 - margin_y;
    window
        .set_position(tauri::Position::Physical(PhysicalPosition::new(x, y)))
        .map_err(|e| e.to_string())
}

/// Give the process an explicit AppUserModelID so Windows attributes toasts and
/// taskbar grouping to this app instead of to whatever shell launched it.
#[cfg(windows)]
fn claim_app_identity() {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new("io.github.codexwidget")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
}

#[cfg(not(windows))]
fn claim_app_identity() {}

/* -------------------------------------------------------------------- tray */

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let settings = {
        let shared = app.state::<Arc<Shared>>();
        let current = shared.settings.lock().unwrap().clone();
        current
    };

    let show = CheckMenuItem::with_id(app, "show", "显示控件", true, settings.show_widget, None::<&str>)?;
    let notify = CheckMenuItem::with_id(
        app,
        "notify",
        "完成时弹出 Windows 通知",
        true,
        settings.notify_on_complete,
        None::<&str>,
    )?;
    let fullscreen = CheckMenuItem::with_id(
        app,
        "fullscreen",
        "长任务完成时全屏提醒（≥10 分钟）",
        true,
        settings.fullscreen_on_long,
        None::<&str>,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &notify,
            &fullscreen,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Codex Widget")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                let shared = app.state::<Arc<Shared>>();
                let mut settings = shared.settings.lock().unwrap();
                settings.show_widget = !settings.show_widget;
                let next = settings.clone();
                drop(settings);
                shared.persist();
                let _ = app.emit("settings://changed", &next);
            }
            "notify" => {
                let shared = app.state::<Arc<Shared>>();
                let mut settings = shared.settings.lock().unwrap();
                settings.notify_on_complete = !settings.notify_on_complete;
                let next = settings.clone();
                drop(settings);
                shared.persist();
                let _ = app.emit("settings://changed", &next);
            }
            "fullscreen" => {
                let shared = app.state::<Arc<Shared>>();
                let mut settings = shared.settings.lock().unwrap();
                settings.fullscreen_on_long = !settings.fullscreen_on_long;
                let next = settings.clone();
                drop(settings);
                shared.persist();
                let _ = app.emit("settings://changed", &next);
            }
            "autostart" => {
                let enabled = app.autolaunch().is_enabled().unwrap_or(false);
                let _ = if enabled {
                    app.autolaunch().disable()
                } else {
                    app.autolaunch().enable()
                };
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/* -------------------------------------------------------------------- main */

fn main() {
    claim_app_identity();

    // State is registered on the Builder, not inside `setup`: the three webviews
    // start their renderers immediately, and the main one can call `snapshot`
    // before `setup` has run. Registering earlier removes that race entirely.
    let config_dir = app_config_dir();
    let _ = std::fs::create_dir_all(&config_dir);
    let settings_path = config_dir.join("settings.json");
    let settings = load_settings(&settings_path);
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(Snapshot::default()),
        settings: Mutex::new(settings),
        settings_path,
        last_overlay: Mutex::new(None),
        overlay_open: Mutex::new(false),
        armed: Mutex::new(false),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .manage(shared.clone())
        .setup(move |app| {
            let handle = app.handle().clone();

            // The polling thread owns the watcher; the UI thread only ever sees
            // snapshots, which keeps the renderer dumb and the tray cheap.
            if let Some(home) = codex_home() {
                let sessions = home.join("sessions");
                let app_handle = handle.clone();
                let shared_feed = shared.clone();
                std::thread::spawn(move || {
                    let mut watcher = Watcher::new(sessions);
                    loop {
                        if watcher.scan() {
                            let snapshot = watcher.snapshot();

                            *shared_feed.snapshot.lock().unwrap() = snapshot.clone();
                            let _ = app_handle.emit("codex://update", snapshot);
                        }
                        std::thread::sleep(Duration::from_millis(POLL_MS));
                    }
                });
            }

            build_tray(&handle)?;

            // closing the widget must not kill the process — the tray lives on
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_ignore_cursor_events(true);
                let window_for_close = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_for_close.hide();
                    }
                });
            }

            // the full-screen layer must never take focus or eat a click; the card must
            // take clicks (that is how it is dismissed) but still not take focus
            if let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) {
                let _ = overlay.set_ignore_cursor_events(true);
                let _ = overlay.set_focusable(false);
                let _ = overlay.set_skip_taskbar(true);
            }
            if let Some(card) = app.get_webview_window(CARD_LABEL) {
                let _ = card.set_focusable(false);
                let _ = card.set_skip_taskbar(true);
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            get_settings,
            set_visible,
            resize_to,
            ready,
            maybe_flash_overlay,
            dismiss_overlay,
            overlay_last,
            js_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running codex-widget");
}