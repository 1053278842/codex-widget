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
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewWindow, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use watcher::{Snapshot, Watcher};

const POLL_MS: u64 = 500;

/// `CODEX_WIDGET_LONG_SECS=5` shortens the threshold so the reminder can be
/// exercised without waiting minutes.
fn threshold_secs(configured: u32) -> u32 {
    std::env::var("CODEX_WIDGET_LONG_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(configured)
}

/* ----------------------------------------------------------------- settings */

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    notify_on_complete: bool,
    show_widget: bool,
    /// 0 = full-screen reminder off; otherwise the minimum turn length in seconds
    /// before the reminder takes over (180 or 600).
    overlay_threshold_secs: u32,
    /// last dragged position of the capsule, in physical pixels
    window_x: Option<i32>,
    window_y: Option<i32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_on_complete: true,
            show_widget: true,
            overlay_threshold_secs: 600,
            window_x: None,
            window_y: None,
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
    /// whether the overlay window is currently on screen
    overlay_open: Mutex<bool>,
    /// throttles writes of the dragged capsule position
    last_position_save: Mutex<Instant>,
    /// set while the first resize after a position restore still has to run
    restored: Mutex<bool>,
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
    // Right after a restore we owe the user the exact spot they left the capsule
    // in, so skip the one right-edge compensation that the first content resize
    // would otherwise apply.
    let skip_compensation = {
        let mut restored = shared.restored.lock().unwrap();
        let was = *restored;
        *restored = false;
        was
    };
    if dx != 0 && !skip_compensation {
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
        let saved = {
            let settings = shared.settings.lock().unwrap();
            match (settings.window_x, settings.window_y) {
                (Some(x), Some(y)) => Some((x, y)),
                _ => None,
            }
        };
        restore_position(&window, saved)?;
        window.show().map_err(|e| e.to_string())?;
        *armed = true;
    }
    Ok(())
}

/// Put the capsule back where the user dragged it, unless that spot no longer
/// exists (monitor unplugged, resolution changed) — then fall back to the corner.
fn restore_position(window: &WebviewWindow, saved: Option<(i32, i32)>) -> Result<(), String> {
    let Some((x, y)) = saved else {
        return park_bottom_right(window);
    };
    let size = window.outer_size().map_err(|e| e.to_string())?;
    let monitors = window.available_monitors().map_err(|e| e.to_string())?;
    let visible = monitors.iter().any(|monitor| {
        let origin = monitor.position();
        let extent = monitor.size();
        x >= origin.x
            && y >= origin.y
            && x + size.width as i32 <= origin.x + extent.width as i32
            && y + size.height as i32 <= origin.y + extent.height as i32
    });
    if !visible {
        return park_bottom_right(window);
    }
    // remember that this exact spot came from the user, not from our anchoring
    if let Some(state) = window.app_handle().try_state::<Arc<Shared>>() {
        *state.restored.lock().unwrap() = true;
    }
    window
        .set_position(tauri::Position::Physical(PhysicalPosition::new(x, y)))
        .map_err(|e| e.to_string())
}

/// Full-screen reminder: it takes every click and key, because any input
/// dismisses it, so nothing needs to stay click-through underneath.
const OVERLAY_LABEL: &str = "overlay";

#[tauri::command]
fn maybe_flash_overlay(
    app: AppHandle,
    shared: State<'_, Arc<Shared>>,
    kind: String,
    label: String,
    elapsed: u32,
) -> Result<bool, String> {
    let configured = shared.settings.lock().unwrap().overlay_threshold_secs;
    if configured == 0 {
        return Ok(false);
    }
    if elapsed < threshold_secs(configured) {
        return Ok(false);
    }

    let overlay = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "overlay window is not configured".to_string())?;

    let monitor = overlay
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no primary monitor".to_string())?;

    overlay
        .set_size(tauri::Size::Physical(*monitor.size()))
        .map_err(|e| e.to_string())?;
    overlay
        .set_position(tauri::Position::Physical(*monitor.position()))
        .map_err(|e| e.to_string())?;
    overlay
        .set_always_on_top(true)
        .map_err(|e| e.to_string())?;
    overlay.show().map_err(|e| e.to_string())?;
    let _ = overlay.set_focus();

    let payload = OverlayPayload {
        kind,
        label,
        elapsed,
    };
    *shared.last_overlay.lock().unwrap() = Some(payload.clone());
    *shared.overlay_open.lock().unwrap() = true;

    app.emit_to(OVERLAY_LABEL, "overlay://play", payload)
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Any click, any key, or a new turn: the single way out.
#[tauri::command]
fn dismiss_overlay(app: AppHandle, shared: State<'_, Arc<Shared>>) -> Result<(), String> {
    {
        let mut open = shared.overlay_open.lock().unwrap();
        if !*open {
            return Ok(());
        }
        *open = false;
    }

    // let the renderer fade itself out, then take the window off screen
    let _ = app.emit_to(OVERLAY_LABEL, "overlay://closing", ());

    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(260));
        if let Some(window) = handle.get_webview_window(OVERLAY_LABEL) {
            let _ = window.hide();
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

fn current_settings(app: &AppHandle) -> Settings {
    let shared = app.state::<Arc<Shared>>();
    let current = shared.settings.lock().unwrap().clone();
    current
}

fn build_menu(app: &AppHandle, settings: &Settings) -> tauri::Result<Menu<tauri::Wry>> {
    let show = CheckMenuItem::with_id(app, "show", "显示控件", true, settings.show_widget, None::<&str>)?;
    let notify = CheckMenuItem::with_id(
        app,
        "notify",
        "完成时弹出 Windows 通知",
        true,
        settings.notify_on_complete,
        None::<&str>,
    )?;

    let threshold = settings.overlay_threshold_secs;
    let off = CheckMenuItem::with_id(app, "os_off", "关闭", true, threshold == 0, None::<&str>)?;
    let three = CheckMenuItem::with_id(app, "os_3", "≥ 3 分钟", true, threshold == 180, None::<&str>)?;
    let ten = CheckMenuItem::with_id(app, "os_10", "≥ 10 分钟", true, threshold == 600, None::<&str>)?;
    let submenu = SubmenuBuilder::new(app, "长任务全屏提醒")
        .item(&off)
        .item(&three)
        .item(&ten)
        .build()?;

    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &show,
            &notify,
            &submenu,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

/// The menu is small, so rebuilding it after every change is simpler (and less
/// error-prone) than tracking individual check items.
fn refresh_tray(app: &AppHandle) {
    let settings = current_settings(app);
    if let Ok(menu) = build_menu(app, &settings) {
        if let Some(tray) = app.tray_by_id("main") {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn mutate_settings(app: &AppHandle, edit: impl FnOnce(&mut Settings)) {
    let shared = app.state::<Arc<Shared>>();
    let next = {
        let mut settings = shared.settings.lock().unwrap();
        edit(&mut settings);
        settings.clone()
    };
    shared.persist();
    let _ = app.emit("settings://changed", &next);
    refresh_tray(app);
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let settings = current_settings(app);
    let menu = build_menu(app, &settings)?;

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
            "show" => mutate_settings(app, |s| s.show_widget = !s.show_widget),
            "notify" => mutate_settings(app, |s| s.notify_on_complete = !s.notify_on_complete),
            "os_off" => mutate_settings(app, |s| s.overlay_threshold_secs = 0),
            "os_3" => mutate_settings(app, |s| s.overlay_threshold_secs = 180),
            "os_10" => mutate_settings(app, |s| s.overlay_threshold_secs = 600),
            "autostart" => {
                let enabled = app.autolaunch().is_enabled().unwrap_or(false);
                let _ = if enabled {
                    app.autolaunch().disable()
                } else {
                    app.autolaunch().enable()
                };
                refresh_tray(app);
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
        last_position_save: Mutex::new(Instant::now()),
        restored: Mutex::new(false),
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

            // closing the widget must not kill the process — the tray lives on.
            // Also remember where the user drops the capsule.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_ignore_cursor_events(true);
                let window_for_events = window.clone();
                let shared_pos = shared.clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = window_for_events.hide();
                    }
                    WindowEvent::Moved(position) => {
                        let persist = {
                            let mut settings = shared_pos.settings.lock().unwrap();
                            if settings.window_x == Some(position.x)
                                && settings.window_y == Some(position.y)
                            {
                                false
                            } else {
                                settings.window_x = Some(position.x);
                                settings.window_y = Some(position.y);
                                let mut last = shared_pos.last_position_save.lock().unwrap();
                                let now = Instant::now();
                                if now.duration_since(*last).as_millis() > 500 {
                                    *last = now;
                                    true
                                } else {
                                    false
                                }
                            }
                        };
                        if persist {
                            shared_pos.persist();
                        }
                    }
                    _ => {}
                });
            }

            // the reminder takes clicks and keys on purpose: any input dismisses it
            if let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) {
                let _ = overlay.set_skip_taskbar(true);
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