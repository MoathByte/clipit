// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod art;
mod capture;
mod config;
mod export;
mod keys;
mod library;
mod media;
mod overlay;
mod scanner;
mod stream;
mod usage;
mod idle;

use config::Config;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tokio::sync::Semaphore;

struct AppState {
    config: Mutex<Config>,
    config_path: PathBuf,
    thumbs_dir: PathBuf,
    art_dir: PathBuf,
    /// Where the overlay's screen snapshot is written.
    overlay_dir: PathBuf,
    ffmpeg: Option<PathBuf>,
    ffmpeg_info: Option<media::FfmpegInfo>,
    /// Limits how many ffmpeg thumbnail jobs run at once.
    thumb_slots: Arc<Semaphore>,
    http: reqwest::Client,
    watcher: Mutex<Option<RecommendedWatcher>>,
    hotkey: Mutex<HotkeyStatus>,
    jobs: Arc<export::Jobs>,
    favorites: Arc<library::Favorites>,
    reviewed: Arc<library::PathSet>,
    meta: Arc<library::ClipMeta>,
    /// Base URL of the local clip streaming server (see stream.rs).
    stream_base: String,
    /// Clipit's own recorder (instant replay + recording).
    engine: capture::Engine,
    clip_hotkey: Mutex<HotkeyStatus>,
    record_hotkey: Mutex<HotkeyStatus>,
}

#[derive(Serialize, Clone, Default)]
struct HotkeyStatus {
    hotkey: String,
    registered: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct Status {
    hotkey: HotkeyStatus,
    clip_hotkey: HotkeyStatus,
    record_hotkey: HotkeyStatus,
    ffmpeg: Option<String>,
    ffmpeg_version: String,
    /// The ffmpeg found can record games (Windows Graphics Capture, ffmpeg 8+).
    ffmpeg_capture: bool,
    has_art_key: bool,
    root_exists: bool,
    export_dir: String,
    autostart: bool,
}

#[derive(Serialize)]
struct ExportOutcome {
    #[serde(flatten)]
    result: export::ExportResult,
    copied: bool,
    copy_error: Option<String>,
}

// ---------- window helpers ----------
//
// One window, two ways to show it:
// * Quick view (Alt+Z): covers the current screen with a blurred snapshot of it
//   and the app panel in the middle; clicking anywhere else, Alt+Z or Esc fades
//   it away and gives focus back to what you were doing. Never on the taskbar.
// * App (tray / launching Clipit): a normal window with a title bar that stays
//   open until you close it.

/// Bumped every time the window is shown or hidden; cancels pending timers.
static VISIBILITY_GEN: AtomicU64 = AtomicU64::new(0);
/// The page in the current window has loaded and can animate itself in.
static UI_READY: AtomicBool = AtomicBool::new(false);
/// A show that is waiting for the page to finish loading (after deep sleep).
static PENDING_SHOW: Mutex<Option<(Present, Shown)>> = Mutex::new(None);
/// How the window is currently shown: 0 hidden, 1 quick view, 2 app.
static MODE: AtomicU8 = AtomicU8::new(0);
/// The window that had focus before the quick view opened (to give it back).
static PREV_FOREGROUND: AtomicIsize = AtomicIsize::new(0);
/// Last app-window position/size, restored next time.
static APP_RECT: Mutex<Option<overlay::Rect>> = Mutex::new(None);

#[derive(Clone, Copy, Debug)]
enum Present {
    Quick(overlay::Rect),
    App,
}

#[derive(Serialize, Clone)]
struct Shown {
    /// "quick" or "app".
    mode: String,
    /// Snapshot of the screen behind the quick view (BMP), if it could be taken.
    backdrop: Option<String>,
    /// The same snapshot, pre-blurred (for the "blur" entrance).
    blurred: Option<String>,
    /// "blur", "dim" or "none".
    animation: String,
    gen: u64,
    /// A page to open on ("settings", "home"), when opened from the tray.
    view: Option<String>,
}

/// Tray menu entries whose text changes.
struct TrayItems {
    record: MenuItem<tauri::Wry>,
}

fn set_tray_recording(app: &AppHandle, on: bool) {
    if let Some(t) = app.try_state::<TrayItems>() {
        let _ = t.record.set_text(if on { "Stop recording" } else { "Start recording" });
    }
}

/// After this long in the tray, the window and its WebView2 engine are closed
/// entirely (≈400 MB freed) and rebuilt the next time Clipit is opened.
fn deep_sleep_after() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var("CLIPIT_SLEEP_SECS").ok().and_then(|s| s.parse().ok()) {
        return Duration::from_secs(s);
    }
    Duration::from_secs(120)
}

/// The monitor under the mouse, in physical pixels.
fn target_monitor(app: &AppHandle) -> overlay::Rect {
    let mon = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    match mon {
        Some(m) => overlay::Rect {
            x: m.position().x,
            y: m.position().y,
            w: m.size().width,
            h: m.size().height,
        },
        None => overlay::Rect { x: 0, y: 0, w: 1920, h: 1080 },
    }
}

fn window_mode(app: &AppHandle) -> String {
    app.try_state::<AppState>().map(|s| s.config.lock().unwrap().window_mode.clone()).unwrap_or_else(|| "both".into())
}

/// Tray click, "Open", launching Clipit again: the app window (or quick view
/// if the user chose "quick view only").
fn open_default(app: &AppHandle) {
    if window_mode(app) == "quick" {
        open_quick(app)
    } else {
        open_app(app)
    }
}

fn open_quick(app: &AppHandle) {
    let gen = VISIBILITY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let rect = target_monitor(app);
    // Remember who had focus, to give it back when the quick view closes.
    #[cfg(windows)]
    {
        let fg = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        let ours = app.get_webview_window("main").and_then(|w| w.hwnd().ok()).map(|h| h.0 == fg.0).unwrap_or(false);
        if !fg.is_invalid() && !ours {
            PREV_FOREGROUND.store(fg.0 as isize, Ordering::SeqCst);
        }
    }
    let (animation, strength) = app
        .try_state::<AppState>()
        .map(|s| {
            let c = s.config.lock().unwrap();
            (c.animation.clone(), c.blur_strength)
        })
        .unwrap_or(("blur".into(), 60));
    // Snapshot the screen *before* our window covers it.
    let (mut backdrop, mut blurred) = (None, None);
    if animation != "none" {
        if let Some(s) = app.try_state::<AppState>() {
            let out = s.overlay_dir.join("backdrop.bmp");
            let blur_out = s.overlay_dir.join("backdrop-blur.bmp");
            let blur = (animation == "blur").then_some((blur_out.as_path(), strength));
            match overlay::capture(rect, 1280, &out, blur) {
                Ok(()) => {
                    backdrop = Some(out.to_string_lossy().into_owned());
                    if animation == "blur" {
                        blurred = Some(blur_out.to_string_lossy().into_owned());
                    }
                }
                Err(_e) => {
                    #[cfg(debug_assertions)]
                    println!("[overlay] no backdrop: {_e}");
                }
            }
        }
    }
    let shown = Shown { mode: "quick".into(), backdrop, blurred, animation, gen, view: None };
    show_or_wake(app, Present::Quick(rect), shown);
}

fn open_app(app: &AppHandle) {
    open_app_on(app, None)
}

fn open_app_on(app: &AppHandle, view: Option<&str>) {
    let gen = VISIBILITY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let shown = Shown { mode: "app".into(), backdrop: None, blurred: None, animation: "none".into(), gen, view: view.map(str::to_string) };
    show_or_wake(app, Present::App, shown);
}

fn show_or_wake(app: &AppHandle, how: Present, shown: Shown) {
    let gen = shown.gen;
    match app.get_webview_window("main") {
        Some(w) if UI_READY.load(Ordering::SeqCst) => present(app, &w, how, shown),
        existing => {
            *PENDING_SHOW.lock().unwrap() = Some((how, shown));
            if existing.is_none() {
                // Woke from deep sleep: rebuild the window (hidden). The page calls
                // ui_ready once it has loaded, which presents it. Done on another
                // thread because creating a window from an event handler can
                // deadlock on Windows.
                let app = app.clone();
                std::thread::spawn(move || {
                    let Some(cfg) = app.config().app.windows.iter().find(|w| w.label == "main").cloned() else {
                        return;
                    };
                    match tauri::WebviewWindowBuilder::from_config(&app, &cfg).and_then(|b| b.build()) {
                        // Final size right away, so the page lays out (and restores
                        // its scroll position) for the real screen.
                        Ok(w) => place(&app, &w, how),
                        Err(e) => eprintln!("couldn't recreate window: {e}"),
                    }
                });
            }
            // Safety net: show it anyway if the page never reports in.
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(4));
                if VISIBILITY_GEN.load(Ordering::SeqCst) != gen {
                    return;
                }
                let pending = PENDING_SHOW.lock().unwrap().take();
                if let (Some((how, shown)), Some(w)) = (pending, app.get_webview_window("main")) {
                    present(&app, &w, how, shown);
                }
            });
        }
    }
}

/// Window chrome, size and position for a way of showing it.
fn place(app: &AppHandle, w: &tauri::WebviewWindow, how: Present) {
    use tauri::{PhysicalPosition, PhysicalSize};
    match how {
        Present::Quick(rect) => {
            let _ = w.set_decorations(false);
            let _ = w.set_resizable(false);
            let _ = w.set_skip_taskbar(true);
            let _ = w.set_always_on_top(true);
            let _ = w.set_position(PhysicalPosition::new(rect.x, rect.y));
            let _ = w.set_size(PhysicalSize::new(rect.w, rect.h));
        }
        Present::App => {
            let _ = w.set_always_on_top(false);
            let _ = w.set_skip_taskbar(false);
            let _ = w.set_resizable(true);
            let _ = w.set_decorations(true);
            let rect = APP_RECT.lock().unwrap().unwrap_or_else(|| {
                // First time: 1320×820 (scaled for the display), centered on the mouse's monitor.
                let m = target_monitor(app);
                let scale = app.primary_monitor().ok().flatten().map(|p| p.scale_factor()).unwrap_or(1.0);
                let (ww, hh) = (((1320.0 * scale) as u32).min(m.w), ((820.0 * scale) as u32).min(m.h));
                overlay::Rect { x: m.x + (m.w - ww) as i32 / 2, y: m.y + (m.h - hh) as i32 / 2, w: ww, h: hh }
            });
            let _ = w.set_size(PhysicalSize::new(rect.w, rect.h));
            let _ = w.set_position(PhysicalPosition::new(rect.x, rect.y));
        }
    }
}

/// Shows the window, focuses it and lets the page animate in.
fn present(app: &AppHandle, w: &tauri::WebviewWindow, how: Present, shown: Shown) {
    let gen = shown.gen;
    // Leaving the app window for the quick view: remember where it was.
    if MODE.load(Ordering::SeqCst) == 2 && matches!(how, Present::Quick(_)) {
        remember_app_rect(w);
    }
    set_low_memory(app, false);
    place(app, w, how);
    MODE.store(if matches!(how, Present::App) { 2 } else { 1 }, Ordering::SeqCst);
    let _ = w.show();
    let _ = w.unminimize();
    let _ = w.set_focus();
    let _ = app.emit("app-shown", shown);
    if matches!(how, Present::Quick(_)) {
        watch_focus(app, w, gen);
    }
}

fn remember_app_rect(w: &tauri::WebviewWindow) {
    if w.is_minimized().unwrap_or(false) || w.is_maximized().unwrap_or(false) {
        return;
    }
    if let (Ok(p), Ok(s)) = (w.outer_position(), w.outer_size()) {
        if s.width > 200 && s.height > 200 {
            *APP_RECT.lock().unwrap() = Some(overlay::Rect { x: p.x, y: p.y, w: s.width, h: s.height });
        }
    }
}

/// While the quick view is shown: if the user clicks into another app (another
/// monitor, alt-tab…), fade out like a popup. Clipit's own dialogs don't count.
fn watch_focus(app: &AppHandle, w: &tauri::WebviewWindow, gen: u64) {
    let Ok(hwnd) = w.hwnd() else { return };
    let hwnd = hwnd.0 as isize;
    let app = app.clone();
    std::thread::spawn(move || {
        let mut was_ours = false;
        let mut away = 0;
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if VISIBILITY_GEN.load(Ordering::SeqCst) != gen || MODE.load(Ordering::SeqCst) != 1 {
                return;
            }
            match overlay::foreground_is_ours(hwnd) {
                Some(true) => {
                    was_ours = true;
                    away = 0;
                }
                // Only after we actually had focus, so a refused focus doesn't
                // close the overlay the moment it opens.
                Some(false) if was_ours => {
                    away += 1;
                    if away >= 2 {
                        request_hide(&app);
                        return;
                    }
                }
                _ => {}
            }
        }
    });
}

/// Asks the page to animate out (it then calls hide_window). Falls back to
/// hiding directly if the page doesn't respond.
fn request_hide(app: &AppHandle) {
    if !UI_READY.load(Ordering::SeqCst) {
        return hide_main(app);
    }
    let gen = VISIBILITY_GEN.load(Ordering::SeqCst);
    let _ = app.emit("overlay-close", ());
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        if VISIBILITY_GEN.load(Ordering::SeqCst) == gen {
            hide_main(&app);
        }
    });
}

/// Hides to the tray and lets WebView2 give memory back while we're not visible.
fn hide_main(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    let was = MODE.swap(0, Ordering::SeqCst);
    if was == 2 {
        remember_app_rect(&w);
    }
    // Closing the quick view with Alt+Z / Esc / a click: give focus back to the
    // game. (If the user clicked into another app, that app keeps it.)
    let still_ours = w.hwnd().ok().and_then(|h| overlay::foreground_is_ours(h.0 as isize)).unwrap_or(false);
    let _ = w.hide();
    #[cfg(windows)]
    if was == 1 && still_ours {
        let prev = PREV_FOREGROUND.load(Ordering::SeqCst);
        if prev != 0 {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SetForegroundWindow};
            unsafe {
                let h = HWND(prev as *mut _);
                if IsWindow(Some(h)).as_bool() {
                    let _ = SetForegroundWindow(h);
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = still_ours;
    let _ = app.emit("app-hidden", ());
    set_low_memory(app, true);
    schedule_deep_sleep(app);
}

/// Closes the hidden window entirely after a while (rebuilt on the next open).
fn schedule_deep_sleep(app: &AppHandle) {
    let gen = VISIBILITY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(deep_sleep_after());
        if VISIBILITY_GEN.load(Ordering::SeqCst) != gen {
            return; // shown again (or hidden again) since
        }
        let busy = app.try_state::<AppState>().map(|s| s.jobs.busy()).unwrap_or(false);
        if busy {
            return; // an export is running; it will finish in the background
        }
        if let Some(w) = app.get_webview_window("main") {
            if !w.is_visible().unwrap_or(true) {
                UI_READY.store(false, Ordering::SeqCst);
                let _ = w.destroy();
            }
        }
    });
}

/// WebView2's MemoryUsageTargetLevel: LOW makes it trim caches and memory.
fn set_low_memory(app: &AppHandle, low: bool) {
    let Some(w) = app.get_webview_window("main") else { return };
    let _ = w.with_webview(move |wv| {
        #[cfg(windows)]
        unsafe {
            use webview2_com::Microsoft::Web::WebView2::Win32::{
                ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
            };
            use windows_core::Interface;
            if let Ok(core) = wv.controller().CoreWebView2() {
                if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                    let level = if low {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
                    } else {
                        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
                    };
                    let _ = core19.SetMemoryUsageTargetLevel(level);
                }
            }
        }
    });
}

/// The quick-view hotkey.
fn toggle_main(app: &AppHandle) {
    let app_only = window_mode(app) == "app";
    let Some(w) = app.get_webview_window("main") else {
        return if app_only { open_app(app) } else { open_quick(app) }; // in deep sleep: wake up
    };
    let visible = w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false);
    let ours = w.hwnd().ok().and_then(|h| overlay::foreground_is_ours(h.0 as isize)).unwrap_or(false);
    match MODE.load(Ordering::SeqCst) {
        // Quick view open and focused: close it.
        1 if visible && ours => request_hide(app),
        // App window in front: the hotkey hides it.
        2 if visible && ours => hide_main(app),
        // App window open but behind something or minimized: bring it forward
        // instead of turning it into the quick view.
        2 => open_app(app),
        _ if app_only => open_app(app),
        _ => open_quick(app),
    }
}

// ---------- background services ----------

/// Registers the quick-view, save-clip and record hotkeys (empty = not set).
fn register_hotkeys(app: &AppHandle, cfg: &Config) -> [HotkeyStatus; 3] {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let mut singles = Vec::new();
    let mut reg = |hotkey: &str, role: usize| {
        let hotkey = hotkey.trim();
        if hotkey.is_empty() {
            return HotkeyStatus::default();
        }
        // A single key ("L", "/") is read without taking it away from other apps.
        if let Some(sc) = keys::single_key(hotkey) {
            singles.push((sc, role));
            return HotkeyStatus { hotkey: hotkey.into(), registered: true, error: None };
        }
        match gs.register(hotkey) {
            Ok(()) => HotkeyStatus { hotkey: hotkey.into(), registered: true, error: None },
            Err(e) => HotkeyStatus { hotkey: hotkey.into(), registered: false, error: Some(e.to_string()) },
        }
    };
    let out = [reg(&cfg.hotkey, 0), reg(&cfg.capture.clip_hotkey, 1), reg(&cfg.capture.record_hotkey, 2)];
    keys::set_bindings(singles);
    out
}

fn apply_hotkeys(app: &AppHandle, state: &AppState) {
    let cfg = state.config.lock().unwrap().clone();
    let [open, clip, record] = register_hotkeys(app, &cfg);
    *state.hotkey.lock().unwrap() = open;
    *state.clip_hotkey.lock().unwrap() = clip;
    *state.record_hotkey.lock().unwrap() = record;
}

/// Which of our hotkeys was pressed: 0 = quick view, 1 = save clip, 2 = record.
fn hotkey_role(cfg: &Config, pressed: &tauri_plugin_global_shortcut::Shortcut) -> Option<usize> {
    use tauri_plugin_global_shortcut::Shortcut;
    [cfg.hotkey.as_str(), cfg.capture.clip_hotkey.as_str(), cfg.capture.record_hotkey.as_str()]
        .iter()
        .position(|k| !k.trim().is_empty() && k.trim().parse::<Shortcut>().map(|s| s.id() == pressed.id()).unwrap_or(false))
}

fn on_hotkey(app: &AppHandle, pressed: &tauri_plugin_global_shortcut::Shortcut) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let cfg = state.config.lock().unwrap().clone();
    if let Some(role) = hotkey_role(&cfg, pressed) {
        on_role(app, role);
    }
}

/// 0 = quick view, 1 = save clip, 2 = record.
fn on_role(app: &AppHandle, role: usize) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let cfg = state.config.lock().unwrap().clone();
    match Some(role) {
        Some(0) => toggle_main(app),
        Some(1) => {
            // Off the main thread (hotkeys arrive there): a busy lock in the
            // recorder must never freeze the window or delay the next key.
            let (app, engine) = (app.clone(), state.engine.clone());
            std::thread::spawn(move || match engine.save_replay() {
                // The chime goes with the key press; the file is written right after in
                // the background (if that fails, the error sound plays instead).
                Ok(()) if cfg.capture.sounds => capture::sound::play(capture::sound::Cue::Saved),
                Ok(()) => {}
                Err(e) => capture_error(&app, &cfg, &e),
            });
        }
        Some(2) => {
            let app = app.clone();
            std::thread::spawn(move || {
                let state = app.state::<AppState>();
                if let Err(e) = state.engine.toggle_recording() {
                    let cfg = state.config.lock().unwrap().clone();
                    capture_error(&app, &cfg, &e);
                }
            });
        }
        _ => {}
    }
}

fn capture_error(app: &AppHandle, cfg: &Config, msg: &str) {
    if cfg.capture.sounds {
        capture::sound::play(capture::sound::Cue::Error);
    }
    let _ = app.emit("capture-error", msg);
}

/// Handles everything the recorder reports.
fn capture_event(app: &AppHandle, ev: capture::Event) {
    use capture::Event;
    let Some(state) = app.try_state::<AppState>() else { return };
    let cfg_now = state.config.lock().unwrap().clone();
    match ev {
        Event::ClipSaved { path, size, kills, kill_times } => {
            // Auto clips chime when saved; your own clips already did on the key press.
            if cfg_now.capture.sounds && kills.is_some() {
                capture::sound::play(capture::sound::Cue::Saved);
            }
            // `kills` is set for auto clips (the round's count); live kill times
            // make the timeline marks show up right away.
            let count = kills.or(kill_times.as_ref().map(|t| t.len() as u32));
            if count.is_some() || kill_times.is_some() {
                let info = library::ClipInfo { kills: count, auto: kills.is_some(), kill_times: kill_times.clone(), ..Default::default() };
                let _ = state.meta.set(&path.to_string_lossy(), &info);
            }
            let _ = app.emit("clip-saved", serde_json::json!({ "path": path, "size": size, "kills": count, "auto": kills.is_some(), "kill_times": kill_times }));
        }
        Event::ClipInfo { path, info } => {
            let merged = state.meta.set(&path.to_string_lossy(), &info).unwrap_or(info);
            let _ = app.emit("clip-info", serde_json::json!({ "path": path, "info": merged }));
        }
        Event::ClipFailed(e) => capture_error(app, &cfg_now, &format!("Couldn't save the clip: {e}")),
        Event::Warning(m) => {
            let _ = app.emit("capture-error", m);
        }
        Event::ClipUpdated { path, size } => {
            let _ = app.emit("clip-updated", serde_json::json!({ "path": path, "size": size }));
        }
        Event::ClipBusy { path, task } => {
            let _ = app.emit("clip-busy", serde_json::json!({ "path": path, "task": task }));
        }
        Event::RecordingStarted(path) => {
            set_tray_recording(app, true);
            set_tray_tooltip(app, "Clipit \u{2014} \u{25CF} recording");
            let _ = app.emit("recording", serde_json::json!({ "active": true, "path": path }));
        }
        Event::RecordingStopped { path, size } => {
            set_tray_recording(app, false);
            set_tray_tooltip(app, "Clipit");
            let _ = app.emit("recording", serde_json::json!({ "active": false, "path": path, "size": size }));
        }
        Event::RecordingFailed(e) => {
            set_tray_recording(app, false);
            set_tray_tooltip(app, "Clipit");
            let _ = app.emit("recording", serde_json::json!({ "active": false }));
            let _ = app.emit("capture-error", format!("Recording: {e}"));
        }
        Event::GameDetected(rule) => {
            let mut cfg = state.config.lock().unwrap();
            if !cfg.capture.games.iter().any(|g| g.exe == rule.exe) {
                cfg.capture.games.push(rule);
                let _ = cfg.save(&state.config_path);
                state.engine.update(engine_settings(&cfg), cfg.clips_root.clone());
                drop(cfg);
                let _ = app.emit("games-changed", ());
            }
        }
        Event::Status(st) => {
            let _ = app.emit("capture-status", st);
        }
    }
}

/// What the recorder runs with: nothing until the first-run setup is done, so
/// it never captures a game or the mic before the user has chosen to.
fn engine_settings(cfg: &Config) -> config::Capture {
    let mut c = cfg.capture.clone();
    c.enabled &= cfg.setup_done;
    c
}

fn set_tray_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(text));
    }
}

/// Watches the clips folder and tells the UI when clips are added/removed.
fn start_watcher(app: &AppHandle, folders: &[String]) -> Option<RecommendedWatcher> {
    let handle = app.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let relevant = !matches!(ev.kind, notify::EventKind::Access(_))
                && ev.paths.iter().any(|p| scanner::is_video(p));
            if relevant {
                let _ = handle.emit("library-changed", ());
            }
        }
    })
    .ok()?;
    let mut any = false;
    for f in folders {
        any |= watcher.watch(Path::new(f), RecursiveMode::Recursive).is_ok();
    }
    any.then_some(watcher)
}

/// Only allow file operations on video files inside the clips folder.
fn ensure_clip(state: &AppState, path: &str) -> Result<PathBuf, String> {
    let folders = state.config.lock().unwrap().library_folders();
    let p = PathBuf::from(path)
        .canonicalize()
        .map_err(|_| "Clip not found".to_string())?;
    let inside = folders.iter().filter_map(|f| PathBuf::from(f).canonicalize().ok()).any(|root| p.starts_with(&root));
    if inside && scanner::is_video(&p) {
        Ok(PathBuf::from(path))
    } else {
        Err("That file isn't in your clips folder".into())
    }
}

// ---------- commands ----------

#[tauri::command]
fn get_config(state: State<AppState>) -> Config {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn get_status(app: AppHandle, state: State<AppState>) -> Status {
    // If another app held a hotkey, try again — it may have been freed.
    let failed = |h: &Mutex<HotkeyStatus>| {
        let h = h.lock().unwrap();
        !h.hotkey.is_empty() && !h.registered
    };
    if failed(&state.hotkey) || failed(&state.clip_hotkey) || failed(&state.record_hotkey) {
        apply_hotkeys(&app, &state);
    }
    let cfg = state.config.lock().unwrap();
    Status {
        hotkey: state.hotkey.lock().unwrap().clone(),
        clip_hotkey: state.clip_hotkey.lock().unwrap().clone(),
        record_hotkey: state.record_hotkey.lock().unwrap().clone(),
        ffmpeg: state
            .ffmpeg
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        ffmpeg_version: state.ffmpeg_info.as_ref().map(|f| f.version.clone()).unwrap_or_default(),
        ffmpeg_capture: state.ffmpeg_info.as_ref().is_some_and(|f| f.capture),
        has_art_key: !cfg.steamgriddb_key.trim().is_empty(),
        root_exists: Path::new(&cfg.clips_root).is_dir(),
        // Empty = edits are saved next to the original clip (inside the library).
        export_dir: custom_export_dir(&cfg)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        autostart: library::autostart_enabled(),
    }
}

/// The folder chosen in Settings for edits, if any. None = next to the original.
fn custom_export_dir(cfg: &Config) -> Option<PathBuf> {
    let d = cfg.export_dir.trim();
    (!d.is_empty()).then(|| PathBuf::from(d))
}

/// Folders outside the library that may hold Clipit exports: the custom folder,
/// plus Videos\Clipit where v0.2–v0.3 saved them.
fn export_dirs(app: &AppHandle, cfg: &Config) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = custom_export_dir(cfg).into_iter().collect();
    if let Ok(v) = app.path().video_dir() {
        dirs.push(v.join("Clipit"));
        dirs.push(v.join("Clipidia")); // older versions
    }
    dirs
}

/// True if `path` exists and is inside `dir`.
fn is_inside(path: &Path, dir: &Path) -> bool {
    match (path.canonicalize(), dir.canonicalize()) {
        (Ok(p), Ok(d)) => p.starts_with(d),
        _ => false,
    }
}

/// A clip in the clips folder, or a file Clipit exported.
fn ensure_clip_or_export(app: &AppHandle, state: &AppState, path: &str) -> Result<PathBuf, String> {
    if let Ok(p) = ensure_clip(state, path) {
        return Ok(p);
    }
    let p = PathBuf::from(path);
    let dirs = export_dirs(app, &state.config.lock().unwrap());
    if scanner::is_video(&p) && dirs.iter().any(|d| is_inside(&p, d)) {
        Ok(p)
    } else {
        Err("That file isn't a clip Clipit knows about".into())
    }
}

/// Puts the file on the clipboard the same way Explorer's "Copy" does,
/// so it can be pasted straight into Discord, a chat, or a folder.
fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    let s = path.to_string_lossy().into_owned();
    let _open = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| format!("Couldn't open the clipboard: {e}"))?;
    clipboard_win::raw::empty().map_err(|e| format!("Couldn't clear the clipboard: {e}"))?;
    clipboard_win::raw::set_file_list(&[s.as_str()])
        .map_err(|e| format!("Couldn't copy to clipboard: {e}"))
}

/// Merges `patch` (any subset of the settings, nested objects allowed) into the config.
fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                merge_json(b.entry(k.clone()).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

#[tauri::command]
fn save_config(app: AppHandle, state: State<AppState>, patch: serde_json::Value) -> Result<Status, String> {
    let mut merged = serde_json::to_value(state.config.lock().unwrap().clone()).map_err(|e| e.to_string())?;
    merge_json(&mut merged, &patch);
    let mut config: Config = serde_json::from_value(merged).map_err(|e| format!("Invalid settings: {e}"))?;
    config.capture.replay_secs = config.capture.replay_secs.clamp(10, 1200);
    config.blur_strength = config.blur_strength.min(100);
    config.clips_root = config.clips_root.trim().to_string();
    config.steamgriddb_key = config.steamgriddb_key.trim().to_string();
    config.hotkey = config.hotkey.trim().to_string();
    config.export_dir = config.export_dir.trim().to_string();
    if config.hotkey.is_empty() {
        config.hotkey = "Alt+Z".into();
    }

    let old = state.config.lock().unwrap().clone();
    config.save(&state.config_path)?;
    *state.config.lock().unwrap() = config.clone();

    if old.library_folders() != config.library_folders() {
        *state.watcher.lock().unwrap() = start_watcher(&app, &config.library_folders());
    }
    let keys = |c: &Config| (c.hotkey.clone(), c.capture.clip_hotkey.clone(), c.capture.record_hotkey.clone());
    if keys(&old) != keys(&config) {
        apply_hotkeys(&app, &state);
    }
    state.engine.update(engine_settings(&config), config.clips_root.clone());
    Ok(get_status(app, state))
}

#[tauri::command]
async fn scan_library(state: State<'_, AppState>) -> Result<scanner::Library, String> {
    let folders = state.config.lock().unwrap().library_folders();
    let favorites = state.favorites.snapshot();
    let reviewed = state.reviewed.snapshot();
    let meta = state.meta.snapshot();
    tauri::async_runtime::spawn_blocking(move || {
        scanner::scan(&folders, &scanner::Flags { favorites: &favorites, reviewed: &reviewed, meta: &meta })
    })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_thumbnail(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let clip = ensure_clip(&state, &path)?;
    let ffmpeg = state
        .ffmpeg
        .clone()
        .ok_or("ffmpeg not found — thumbnails are disabled")?;
    let dir = state.thumbs_dir.clone();
    let _slot = state
        .thumb_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| e.to_string())?;
    let out = tauri::async_runtime::spawn_blocking(move || media::thumbnail(&ffmpeg, &dir, &clip))
        .await
        .map_err(|e| e.to_string())??;
    Ok(out.to_string_lossy().into_owned())
}

#[tauri::command]
async fn get_game_art(state: State<'_, AppState>, name: String) -> Result<art::GameArt, String> {
    if name.eq_ignore_ascii_case("Desktop") || name == "Other" {
        return Ok(art::GameArt::default());
    }
    if let Some(a) = art::cached(&state.art_dir, &name) {
        return Ok(a);
    }
    let key = state.config.lock().unwrap().steamgriddb_key.clone();
    if key.is_empty() {
        return Ok(art::GameArt::default());
    }
    art::fetch(&state.http, &key, &state.art_dir, &name).await
}

#[tauri::command]
fn refresh_art(state: State<AppState>) {
    art::clear(&state.art_dir);
    let _ = std::fs::create_dir_all(&state.art_dir);
}

#[tauri::command]
async fn export_clip(
    app: AppHandle,
    state: State<'_, AppState>,
    job: String,
    request: export::ExportRequest,
    copy: bool,
) -> Result<ExportOutcome, String> {
    let src = ensure_clip(&state, &request.path)?;
    let ffmpeg = state
        .ffmpeg
        .clone()
        .ok_or("ffmpeg not found — install it with: winget install Gyan.FFmpeg")?;
    let custom = custom_export_dir(&state.config.lock().unwrap());
    let dir = match custom {
        Some(d) => d,
        None => src.parent().map(Path::to_path_buf).ok_or("Clip has no folder")?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let out = export::unique_output(&dir, &src);
    let jobs = state.jobs.clone();
    let emitter = app.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        let progress = |percent: f64, pass: u32| {
            let _ = emitter.emit(
                "export-progress",
                export::Progress { job: job.clone(), percent, pass },
            );
        };
        export::run(&ffmpeg, &jobs, &job, &request, &out, &progress)
    })
    .await
    .map_err(|e| e.to_string())??;

    let (copied, copy_error) = if copy {
        match copy_file_to_clipboard(Path::new(&result.output)) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        }
    } else {
        (false, None)
    };
    Ok(ExportOutcome { result, copied, copy_error })
}

#[tauri::command]
fn cancel_export(state: State<AppState>, job: String) {
    state.jobs.cancel(&job);
}

#[tauri::command]
fn copy_file(app: AppHandle, state: State<AppState>, path: String) -> Result<(), String> {
    let p = ensure_clip_or_export(&app, &state, &path)?;
    copy_file_to_clipboard(&p)
}

#[tauri::command]
fn reveal_clip(app: AppHandle, state: State<AppState>, path: String) -> Result<(), String> {
    let p = ensure_clip_or_export(&app, &state, &path)?;
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(format!("/select,\"{}\"", p.display()));
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_clip(state: State<AppState>, path: String) -> Result<(), String> {
    let p = ensure_clip(&state, &path)?;
    trash::delete(&p).map_err(|e| e.to_string())?;
    let _ = state.favorites.set(&path, false);
    let _ = state.reviewed.set(&path, false);
    state.meta.moved(&path, None);
    Ok(())
}

// ---------- several clips at once, folders, compression ----------

fn worker(app: &AppHandle) -> Arc<idle::Worker> {
    app.state::<Arc<idle::Worker>>().inner().clone()
}

/// Moves a clip and everything Clipit knows about it. Returns the new path.
fn move_clip_to(app: &AppHandle, state: &AppState, path: &str, dir: &Path) -> Result<String, String> {
    let old = ensure_clip(state, path)?;
    if old.parent() == Some(dir) {
        return Ok(path.to_string());
    }
    let name = old.file_name().ok_or("bad path")?;
    let mut new = dir.join(name);
    let mut n = 2;
    while new.exists() {
        let stem = old.file_stem().unwrap_or_default().to_string_lossy();
        let ext = old.extension().unwrap_or_default().to_string_lossy();
        new = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    std::fs::rename(&old, &new).or_else(|_| std::fs::copy(&old, &new).and_then(|_| std::fs::remove_file(&old))).map_err(|e| format!("{}: {e}", name.to_string_lossy()))?;
    let new_str = new.to_string_lossy().into_owned();
    let _ = state.favorites.moved(path, &new_str);
    let _ = state.reviewed.moved(path, &new_str);
    state.meta.moved(path, Some(&new_str));
    worker(app).moved(path, Some(&new_str));
    Ok(new_str)
}

/// A new folder in the clips folder. Returns its name.
#[tauri::command]
fn create_folder(state: State<AppState>, name: String) -> Result<String, String> {
    let name = library::validate_name(&name)?;
    let root = PathBuf::from(state.config.lock().unwrap().clips_root.clone());
    let dir = root.join(&name);
    if dir.exists() {
        return Err(format!("There's already a folder called {name}"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create the folder: {e}"))?;
    Ok(name)
}

#[tauri::command]
fn rename_folder(app: AppHandle, state: State<AppState>, from: String, to: String) -> Result<String, String> {
    let to = library::validate_name(&to)?;
    let root = PathBuf::from(state.config.lock().unwrap().clips_root.clone());
    let (a, b) = (root.join(&from), root.join(&to));
    if !a.is_dir() || from.contains(['\\', '/']) || from.starts_with('.') {
        return Err("That folder isn't in your clips folder".into());
    }
    if b.exists() && !from.eq_ignore_ascii_case(&to) {
        return Err(format!("There's already a folder called {to}"));
    }
    // Everything Clipit knows about the clips inside follows them.
    let mut clips = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&a) {
        clips.extend(rd.flatten().map(|e| e.path()).filter(|p| scanner::is_video(p)));
    }
    std::fs::rename(&a, &b).map_err(|e| format!("Couldn't rename the folder: {e}"))?;
    for old in clips {
        let new = b.join(old.file_name().unwrap_or_default());
        let (o, n) = (old.to_string_lossy().into_owned(), new.to_string_lossy().into_owned());
        let _ = state.favorites.moved(&o, &n);
        let _ = state.reviewed.moved(&o, &n);
        state.meta.moved(&o, Some(&n));
        worker(&app).moved(&o, Some(&n));
    }
    Ok(to)
}

/// Removes an empty folder (one with clips in it is never deleted).
#[tauri::command]
fn delete_folder(state: State<AppState>, name: String) -> Result<(), String> {
    let root = PathBuf::from(state.config.lock().unwrap().clips_root.clone());
    let dir = root.join(&name);
    if name.contains(['\\', '/']) || name.starts_with('.') || !dir.is_dir() {
        return Err("That folder isn't in your clips folder".into());
    }
    std::fs::remove_dir(&dir).map_err(|_| "Only empty folders can be deleted. Move or delete its clips first.".to_string())
}

/// Moves clips into a folder of the clips folder. Returns the new paths (in order).
#[tauri::command]
fn move_clips(app: AppHandle, state: State<AppState>, paths: Vec<String>, folder: String) -> Result<Vec<String>, String> {
    let root = PathBuf::from(state.config.lock().unwrap().clips_root.clone());
    let dir = if folder.is_empty() { root } else { root.join(library::validate_name(&folder)?) };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for p in &paths {
        match move_clip_to(&app, &state, p, &dir) {
            Ok(n) => out.push(n),
            Err(e) => {
                out.push(p.clone());
                errors.push(e);
            }
        }
    }
    if errors.len() == paths.len() && !paths.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(out)
}

#[tauri::command]
fn favorite_clips(state: State<AppState>, paths: Vec<String>, favorite: bool) -> Result<(), String> {
    for p in &paths {
        ensure_clip(&state, p)?;
        state.favorites.set(p, favorite)?;
    }
    Ok(())
}

/// To the Recycle Bin. Returns the paths that were deleted.
#[tauri::command]
fn delete_clips(app: AppHandle, state: State<AppState>, paths: Vec<String>) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for p in &paths {
        let Ok(file) = ensure_clip(&state, p) else { continue };
        if trash::delete(&file).is_ok() {
            let _ = state.favorites.set(p, false);
            let _ = state.reviewed.set(p, false);
            state.meta.moved(p, None);
            worker(&app).moved(p, None);
            done.push(p.clone());
        }
    }
    if done.is_empty() && !paths.is_empty() {
        return Err("Couldn't delete them (in use?)".into());
    }
    Ok(done)
}

/// Copies several clips, to paste into Discord, a chat or a folder.
#[tauri::command]
fn copy_files(state: State<AppState>, paths: Vec<String>) -> Result<(), String> {
    let files: Vec<String> = paths.iter().filter_map(|p| ensure_clip(&state, p).ok()).map(|p| p.to_string_lossy().into_owned()).collect();
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let _open = clipboard_win::Clipboard::new_attempts(10).map_err(|e| format!("Couldn't open the clipboard: {e}"))?;
    clipboard_win::raw::empty().map_err(|e| format!("Couldn't clear the clipboard: {e}"))?;
    clipboard_win::raw::set_file_list(&refs).map_err(|e| format!("Couldn't copy to clipboard: {e}"))
}

#[tauri::command]
fn compress_state(app: AppHandle) -> idle::State {
    worker(&app).state()
}

#[tauri::command]
fn compress_seen(app: AppHandle) {
    worker(&app).mark_seen()
}

/// Puts the originals back.
#[tauri::command]
fn compress_revert(app: AppHandle, paths: Vec<String>) -> Result<u32, String> {
    let n = worker(&app).revert(&paths)?;
    for p in &paths {
        if let Ok(m) = std::fs::metadata(p) {
            let _ = app.emit("clip-updated", serde_json::json!({ "path": p, "size": m.len(), "quiet": true }));
        }
    }
    Ok(n)
}

/// Compress these clips now (low priority, never while a game runs).
#[tauri::command]
fn compress_clips(app: AppHandle, state: State<AppState>, paths: Vec<String>) -> Result<usize, String> {
    let ok: Vec<String> = paths.into_iter().filter(|p| ensure_clip(&state, p).is_ok()).collect();
    Ok(worker(&app).enqueue(ok))
}

/// Kills, map and agent in a VALORANT clip, read from the saved file.
#[tauri::command]
async fn analyze_clip(state: State<'_, AppState>, path: String) -> Result<library::ClipInfo, String> {
    let clip = ensure_clip(&state, &path)?;
    let engine = state.engine.clone();
    let info = tauri::async_runtime::spawn_blocking(move || engine.analyze(&clip))
        .await
        .map_err(|e| e.to_string())??;
    state.meta.set(&path, &info)
}

/// Cameras for the hand cam.
#[tauri::command]
async fn list_cameras(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || engine.cameras()).await.map_err(|e| e.to_string())
}

/// A still from the hand cam with its picture adjustments, as a data: URL.
/// `fresh` = take a new picture (else the last one is adjusted again).
#[tauri::command]
async fn camera_preview(state: State<'_, AppState>, handcam: config::HandCam, fresh: bool) -> Result<String, String> {
    let engine = state.engine.clone();
    let jpeg = tauri::async_runtime::spawn_blocking(move || engine.camera_preview(&handcam, fresh)).await.map_err(|e| e.to_string())??;
    Ok(format!("data:image/jpeg;base64,{}", capture::handcam::base64(&jpeg)))
}

/// The camera's own settings window (exposure, white balance, focus…).
#[tauri::command]
async fn camera_settings(state: State<'_, AppState>, device: String) -> Result<(), String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || engine.camera_settings(&device)).await.map_err(|e| e.to_string())?
}

/// Reads when your kills happen in a clip, to mark them on the timeline.
/// A known kill count (an auto clip's) is kept.
#[tauri::command]
async fn read_kill_times(state: State<'_, AppState>, path: String) -> Result<library::ClipInfo, String> {
    let clip = ensure_clip(&state, &path)?;
    let engine = state.engine.clone();
    let times = tauri::async_runtime::spawn_blocking(move || engine.kill_times(&clip))
        .await
        .map_err(|e| e.to_string())??;
    let known = library::ClipMeta::get_in(&state.meta.snapshot(), &path).kills;
    let info = library::ClipInfo { kills: known.or(Some(times.len() as u32)), kill_times: Some(times), ..Default::default() };
    state.meta.set(&path, &info)
}

/// Light or dark title bar for the app window, to match the chosen theme.
#[tauri::command]
fn set_window_theme(app: AppHandle, dark: bool) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_theme(Some(if dark { tauri::Theme::Dark } else { tauri::Theme::Light }));
    }
}

// ---------- updates (signed releases on GitHub) ----------

#[derive(Serialize)]
struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release, if there is one. Releases are signed; the app only installs
/// files signed with the project's key.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateInfo { version: u.version.clone(), notes: u.body.clone() }))
}

/// Downloads the update (progress as "update-progress"), finishes a recording
/// properly, then runs the installer, which restarts Clipit.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Clipit is already up to date.")?;
    let progress = app.clone();
    let mut done = 0u64;
    let bytes = update
        .download(
            move |chunk, total| {
                done += chunk as u64;
                let _ = progress.emit("update-progress", serde_json::json!({ "done": done, "total": total }));
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(s) = app.try_state::<AppState>() {
        s.engine.shutdown();
    }
    update.install(bytes).map_err(|e| e.to_string())?;
    app.restart();
}

/// Installs or updates ffmpeg with winget in a console window the user can watch.
#[tauri::command]
fn update_ffmpeg() -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let script = "title Updating ffmpeg for Clipit & echo Installing the latest ffmpeg with winget... & echo. & \
            (winget upgrade --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements || \
             winget install --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements) & \
            echo. & echo Done. Go back to Clipit and click Restart Clipit. & pause";
        std::process::Command::new("cmd.exe")
            .args(["/c", script])
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Couldn't start winget: {e}"))
    }
    #[cfg(not(windows))]
    Err("Only on Windows".into())
}

/// Installs (or updates) ffmpeg with winget, hidden, reporting progress as
/// "ffmpeg-install" events ({ phase, percent }). Returns the new version once an
/// ffmpeg that can record games is found.
#[tauri::command]
async fn install_ffmpeg(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || install_ffmpeg_blocking(&app)).await.map_err(|e| e.to_string())?
}

fn install_ffmpeg_blocking(app: &AppHandle) -> Result<String, String> {
    use std::io::Read;
    let emit = |phase: &str, percent: Option<u32>| {
        let _ = app.emit("ffmpeg-install", serde_json::json!({ "phase": phase, "percent": percent }));
    };
    let found = media::find_ffmpeg();
    if let Some(f) = found.as_ref().filter(|f| f.capture) {
        return Ok(f.version.clone());
    }
    emit("download", None);
    let verb = if found.is_some() { "upgrade" } else { "install" };
    let mut cmd = std::process::Command::new("winget");
    cmd.args([verb, "--id", "Gyan.FFmpeg", "-e", "--silent", "--disable-interactivity", "--accept-source-agreements", "--accept-package-agreements"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|_| "Windows' package manager (winget) isn't available on this PC. Install \"App Installer\" from the Microsoft Store, then try again.".to_string())?;
    let mut out = child.stdout.take().unwrap();
    let mut text = String::new();
    let mut buf = [0u8; 4096];
    let mut last = None;
    while let Ok(n) = out.read(&mut buf) {
        if n == 0 {
            break;
        }
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
        if let Some(p) = last_percent(&text) {
            if last != Some(p) {
                last = Some(p);
                emit(if text.contains("installing") || text.contains("Installing") { "install" } else { "download" }, Some(p));
            }
        }
        if text.len() > 64_000 {
            text.drain(..32_000);
        }
    }
    let _ = child.wait();
    emit("check", None);
    match media::find_ffmpeg() {
        Some(f) if f.capture => Ok(f.version),
        _ => {
            let why = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains('%') && !l.starts_with(['-', '\\', '|', '/'])).last().unwrap_or("").to_string();
            Err(if why.is_empty() { "ffmpeg didn't install.".into() } else { format!("ffmpeg didn't install: {why}") })
        }
    }
}

/// An ffmpeg that can record games is installed now (after a manual install).
#[tauri::command]
fn ffmpeg_ready() -> bool {
    media::find_ffmpeg().map(|f| f.capture).unwrap_or(false)
}

/// The last "NN%" in winget's progress output.
fn last_percent(text: &str) -> Option<u32> {
    let i = text.rfind('%')?;
    let digits: String = text[..i].chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().ok().filter(|p| *p <= 100)
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    if let Some(s) = app.try_state::<AppState>() {
        s.engine.shutdown();
    }
    app.restart();
}

/// Settings from the quick view: switch to the normal app window.
#[tauri::command]
fn open_app_window(app: AppHandle) {
    open_app(&app);
}

/// Undo for deletes: puts the most recently deleted copy of `path` back from the Recycle Bin.
#[tauri::command]
fn restore_clip(state: State<AppState>, path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let folders = state.config.lock().unwrap().library_folders();
    if !scanner::is_video(&p) || !folders.iter().any(|f| p.starts_with(f)) {
        return Err("That file isn't in your clips folder".into());
    }
    let name = p.file_name().ok_or("bad path")?.to_os_string();
    let parent = p.parent().ok_or("bad path")?.to_path_buf();
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;
    let item = items
        .into_iter()
        .filter(|i| i.name == name && i.original_parent == parent)
        .max_by_key(|i| i.time_deleted)
        .ok_or("Couldn't find it in the Recycle Bin")?;
    trash::os_limited::restore_all([item]).map_err(|e| format!("Couldn't restore: {e}"))
}

#[tauri::command]
fn set_reviewed(state: State<AppState>, paths: Vec<String>, reviewed: bool) -> Result<(), String> {
    for p in &paths {
        ensure_clip(&state, p)?;
    }
    state.reviewed.set_many(&paths, reviewed)
}

#[tauri::command]
fn set_favorite(state: State<AppState>, path: String, favorite: bool) -> Result<(), String> {
    ensure_clip(&state, &path)?;
    state.favorites.set(&path, favorite)
}

#[tauri::command]
fn rename_clip(state_worker: AppHandle, state: State<AppState>, path: String, name: String) -> Result<scanner::Clip, String> {
    let old = ensure_clip(&state, &path)?;
    let folders = state.config.lock().unwrap().library_folders();
    let new = library::rename(&old, &name)?;
    let new_str = new.to_string_lossy().into_owned();
    if let Err(e) = state.favorites.moved(&path, &new_str) {
        eprintln!("favorites not updated after rename: {e}");
    }
    let _ = state.reviewed.moved(&path, &new_str);
    let game = scanner::game_of(&new, &folders);
    state.meta.moved(&path, Some(&new_str));
    if let Some(w) = state_worker.try_state::<Arc<idle::Worker>>() {
        w.moved(&path, Some(&new_str));
    }
    let (favorites, reviewed, meta) = (state.favorites.snapshot(), state.reviewed.snapshot(), state.meta.snapshot());
    scanner::clip_from(&new, &game, &scanner::Flags { favorites: &favorites, reviewed: &reviewed, meta: &meta })
        .ok_or_else(|| "Renamed, but couldn't read the clip afterwards".into())
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    library::set_autostart(enabled)?;
    Ok(library::autostart_enabled())
}

#[tauri::command]
async fn pick_folder(app: AppHandle, title: Option<String>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let title = title.unwrap_or_else(|| "Choose a folder".into());
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().set_title(title).blocking_pick_folder()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(picked
        .and_then(|f| f.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

/// Debug builds only: lets a test script drive the UI (CLIPIT_AUTOTEST=scroll-all).
#[tauri::command]
fn autotest() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var("CLIPIT_AUTOTEST").ok()
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// Debug builds only: lets autotests report what the page sees.
#[tauri::command]
fn debug_log(msg: String) {
    #[cfg(debug_assertions)]
    println!("[ui] {msg}");
    #[cfg(not(debug_assertions))]
    let _ = msg;
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    hide_main(&app);
}

/// The page has loaded; present it if Alt+Z was pressed while it was loading.
#[tauri::command]
fn ui_ready(app: AppHandle) {
    UI_READY.store(true, Ordering::SeqCst);
    let pending = PENDING_SHOW.lock().unwrap().take();
    if let (Some((how, shown)), Some(w)) = (pending, app.get_webview_window("main")) {
        present(&app, &w, how, shown);
    }
}

/// While the user records a new hotkey, ours must not fire.
#[tauri::command]
fn set_hotkeys_paused(app: AppHandle, state: State<AppState>, paused: bool) {
    keys::set_paused(paused);
    if paused {
        let _ = app.global_shortcut().unregister_all();
    } else {
        apply_hotkeys(&app, &state);
    }
}

/// Asks the overlay to fade out (used by the page for Esc / clicking outside).
#[tauri::command]
fn close_overlay(app: AppHandle) {
    request_hide(&app);
}

#[tauri::command]
async fn get_waveform(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<Vec<u8>, String> {
    let clip = ensure_clip_or_export(&app, &state, &path)?;
    let ffmpeg = state.ffmpeg.clone().ok_or("ffmpeg not found")?;
    let dir = state.thumbs_dir.clone();
    tauri::async_runtime::spawn_blocking(move || media::waveform(&ffmpeg, &dir, &clip))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn capture_status(state: State<AppState>) -> capture::Status {
    state.engine.status()
}

/// Clips still being worked on after saving: [[path, "handcam" | "reading"], ...].
#[tauri::command]
fn busy_clips(state: State<AppState>) -> Vec<(String, &'static str)> {
    state.engine.busy_clips()
}

/// Settings → Recording → Restart recorder.
#[tauri::command]
fn restart_capture(state: State<AppState>) {
    state.engine.restart();
}

#[derive(Serialize)]
struct CaptureInfo {
    devices: capture::audio::AudioDevices,
    monitors: Vec<capture::game::Monitor>,
    encoders: Vec<String>,
}

#[tauri::command]
async fn capture_info(state: State<'_, AppState>) -> Result<CaptureInfo, String> {
    let encoders = state.engine.encoders();
    tauri::async_runtime::spawn_blocking(move || CaptureInfo {
        devices: capture::audio::list_devices(),
        monitors: capture::game::monitors(),
        encoders,
    })
    .await
    .map_err(|e| e.to_string())
}

/// Async so it never runs on (and can never stall) the window's main thread.
#[tauri::command]
async fn save_replay(state: State<'_, AppState>) -> Result<(), String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || engine.save_replay())
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn toggle_recording(state: State<'_, AppState>) -> Result<bool, String> {
    let engine = state.engine.clone();
    tauri::async_runtime::spawn_blocking(move || engine.toggle_recording())
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn stream_base(state: State<AppState>) -> String {
    state.stream_base.clone()
}

// ---------- entry point ----------

/// The app used to be called Clipidia: move its settings, favorites and caches
/// (and the WebView's saved state) to Clipit's folders, once.
fn migrate_old_name() {
    for var in ["APPDATA", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(var).map(PathBuf::from) else { continue };
        let (old, new) = (base.join("com.clipidia.app"), base.join("com.clipit.app"));
        if old.is_dir() && !new.exists() {
            if let Err(e) = std::fs::rename(&old, &new) {
                eprintln!("couldn't move {} to {}: {e}", old.display(), new.display());
            }
        }
    }
}

fn main() {
    migrate_old_name();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            open_default(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        on_hotkey(app, shortcut);
                    }
                })
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            let config_path = app.path().app_config_dir()?.join("config.json");
            let favorites_path = app.path().app_config_dir()?.join("favorites.json");
            let reviewed_path = app.path().app_config_dir()?.join("reviewed.json");
            let meta_path = app.path().app_config_dir()?.join("clip-info.json");
            let cache_dir = app.path().app_cache_dir()?;
            let thumbs_dir = cache_dir.join("thumbs");
            let art_dir = cache_dir.join("art");
            let overlay_dir = cache_dir.join("overlay");
            std::fs::create_dir_all(&thumbs_dir)?;
            std::fs::create_dir_all(&art_dir)?;
            std::fs::create_dir_all(&overlay_dir)?;

            let config = Config::load(&config_path);
            if !config_path.exists() {
                let _ = config.save(&config_path);
            }


            let watcher = start_watcher(&handle, &config.library_folders());

            let http = reqwest::Client::builder()
                .user_agent(concat!("Clipit/", env!("CARGO_PKG_VERSION")))
                .build()?;
            // VALORANT ability icons and map layouts for tagging clips (weekly refresh).
            {
                let (http, dir) = (http.clone(), cache_dir.join("valorant"));
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = capture::valorant::fetch_refs(&http, &dir).await {
                        eprintln!("valorant references: {e}");
                    }
                });
            }

            // Local file server for clips, thumbnails and art. It only serves clips
            // and exports Clipit knows about, plus images in its own cache folders.
            let stream_handle = handle.clone();
            let (thumbs_allowed, art_allowed, overlay_allowed) =
                (thumbs_dir.clone(), art_dir.clone(), overlay_dir.clone());
            let streamer = stream::start(Arc::new(move |p: &str| {
                let path = Path::new(p);
                let is_image = path
                    .extension()
                    .map(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp" | "ico" | "bmp"))
                    .unwrap_or(false);
                // Checked by path shape, not canonicalize(): when an app is started from a
                // packaged (MSIX) app, Windows can redirect AppData files to a different
                // real folder, so canonical paths of cache files don't match the cache dir.
                if is_image
                    && (stream::is_plain_file_in(path, &thumbs_allowed, 1)
                        || stream::is_plain_file_in(path, &art_allowed, 2)
                        || stream::is_plain_file_in(path, &overlay_allowed, 1))
                {
                    return Some(path.to_path_buf());
                }
                let state = stream_handle.state::<AppState>();
                ensure_clip_or_export(&stream_handle, &state, p).ok()
            }))?;

            let ffmpeg_info = media::find_ffmpeg();
            let ffmpeg = ffmpeg_info.as_ref().map(|f| f.path.clone());
            let note = match &ffmpeg_info {
                None => "ffmpeg isn't installed, so Clipit can't record yet.".to_string(),
                Some(f) => format!("Your ffmpeg ({}) is too old to record games. Clipit needs ffmpeg 8 or newer.", if f.version.is_empty() { "unknown version" } else { &f.version }),
            };
            let events_app = handle.clone();
            let engine = capture::Engine::start(
                ffmpeg_info.as_ref().filter(|f| f.capture).map(|f| f.path.clone()),
                note,
                engine_settings(&config),
                config.clips_root.clone(),
                cache_dir.clone(),
                Box::new(move |ev| capture_event(&events_app, ev)),
            );
            // Recordings interrupted by a crash or power loss: make them visible again.
            let root = config.clips_root.clone();
            std::thread::spawn(move || capture::recover_parts(Path::new(&root)));

            app.manage(AppState {
                engine,
                clip_hotkey: Mutex::new(HotkeyStatus::default()),
                record_hotkey: Mutex::new(HotkeyStatus::default()),
                stream_base: streamer.base,
                config: Mutex::new(config),
                config_path,
                thumbs_dir,
                art_dir,
                overlay_dir,
                ffmpeg,
                ffmpeg_info,
                thumb_slots: Arc::new(Semaphore::new(3)),
                http,
                watcher: Mutex::new(watcher),
                hotkey: Mutex::new(HotkeyStatus::default()),
                jobs: Arc::new(export::Jobs::default()),
                favorites: Arc::new(library::Favorites::load(favorites_path)),
                reviewed: Arc::new(library::PathSet::load(reviewed_path)),
                meta: Arc::new(library::ClipMeta::load(meta_path)),
            });

            app.manage(Arc::new(idle::Worker::load(app.path().app_config_dir()?.join("compressed.json"))));
            idle::start(handle.clone());

            // Open the sound device now, so the first chime isn't late.
            capture::sound::warm();
            let keys_app = handle.clone();
            keys::init(move |role| {
                // Back on the main thread, like the normal hotkeys.
                let app = keys_app.clone();
                let _ = keys_app.run_on_main_thread(move || on_role(&app, role));
            });
            apply_hotkeys(&handle, &handle.state::<AppState>());

            // Launched by Windows at sign-in: stay in the tray. Otherwise show the window.
            let minimized = std::env::args().any(|a| a == "--minimized");
            if minimized {
                // Nothing to show yet: free the WebView after a while like any hide.
                schedule_deep_sleep(&handle);
            } else {
                open_default(&handle);
            }
            // Keep the startup entry pointing at this exe if the app was moved/updated.
            #[cfg(not(debug_assertions))]
            library::migrate_autostart();
            #[cfg(not(debug_assertions))]
            if library::autostart_enabled() {
                let _ = library::set_autostart(true);
            }

            // Tray icon: Clipit keeps running in the background so the hotkey works.
            let clip = MenuItem::with_id(app, "clip", "Save clip", true, None::<&str>)?;
            let record = MenuItem::with_id(app, "record", "Start recording", true, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "View clips", true, None::<&str>)?;
            let folder = MenuItem::with_id(app, "folder", "Open clips folder", true, None::<&str>)?;
            let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let usage_item = MenuItem::with_id(app, "usage", "CPU \u{2013} \u{00B7} RAM \u{2013}", false, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Clipit", true, None::<&str>)?;
            let sep = || PredefinedMenuItem::separator(app);
            let menu = Menu::with_items(app, &[&clip, &record, &sep()?, &open, &folder, &settings, &sep()?, &usage_item, &sep()?, &quit])?;
            app.manage(TrayItems { record: record.clone() });
            // Clipit's own CPU and RAM (with ffmpeg and the page), every few seconds.
            std::thread::spawn(move || {
                let mut meter = usage::Meter::default();
                meter.read();
                loop {
                    std::thread::sleep(Duration::from_secs(3));
                    let _ = usage_item.set_text(usage::label(meter.read()));
                }
            });
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("app icon missing"))
                .tooltip("Clipit")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "clip" => on_role(app, 1),
                    "record" => on_role(app, 2),
                    "open" => open_app_on(app, Some("home")),
                    "settings" => open_app_on(app, Some("settings")),
                    "folder" => {
                        if let Some(s) = app.try_state::<AppState>() {
                            let root = s.config.lock().unwrap().clips_root.clone();
                            let _ = std::fs::create_dir_all(&root);
                            let _ = std::process::Command::new("explorer").arg(&root).spawn();
                        }
                    }
                    "quit" => {
                        // Finish a recording properly before quitting.
                        if let Some(s) = app.try_state::<AppState>() {
                            s.engine.shutdown();
                        }
                        app.exit(0)
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
                        open_default(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it to the tray instead of quitting.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    hide_main(window.app_handle());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            save_config,
            scan_library,
            get_thumbnail,
            get_game_art,
            refresh_art,
            reveal_clip,
            delete_clip,
            pick_folder,
            hide_window,
            ui_ready,
            set_hotkeys_paused,
            close_overlay,
            capture_status,
            capture_info,
            save_replay,
            busy_clips,
            restart_capture,
            toggle_recording,
            get_waveform,
            export_clip,
            cancel_export,
            copy_file,
            set_favorite,
            rename_clip,
            set_autostart,
            set_reviewed,
            restore_clip,
            analyze_clip,
            read_kill_times,
            list_cameras,
            camera_preview,
            camera_settings,
            open_app_window,
            set_window_theme,
            app_version,
            update_ffmpeg,
            restart_app,
            install_ffmpeg,
            create_folder,
            rename_folder,
            delete_folder,
            move_clips,
            favorite_clips,
            delete_clips,
            copy_files,
            compress_state,
            compress_seen,
            compress_revert,
            compress_clips,
            ffmpeg_ready,
            check_update,
            install_update,
            autotest,
            stream_base,
            debug_log,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Clipit")
        .run(|app, event| match event {
            // Deep sleep closes the only window; keep running in the tray.
            // (Quit from the tray calls exit(0), which has a code and is allowed.)
            tauri::RunEvent::ExitRequested { api, code, .. } if code.is_none() => api.prevent_exit(),
            tauri::RunEvent::Exit => {
                if let Some(s) = app.try_state::<AppState>() {
                    s.engine.shutdown();
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod install_tests {
    #[test]
    fn reads_winget_progress() {
        assert_eq!(super::last_percent("  ██████        12%\r  ████████████  57%"), Some(57));
        assert_eq!(super::last_percent("100%"), Some(100));
        assert_eq!(super::last_percent("Found FFmpeg"), None);
    }
}
