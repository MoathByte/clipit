//! Single-key hotkeys ("L", "/", "]").
//!
//! Windows' hotkey API takes a key away from every other app, so a plain letter
//! could no longer be typed anywhere. Single keys are read with Raw Input instead
//! (the way voice apps read push-to-talk): Clipit is told about key presses but
//! the key still reaches the game. Nothing is hooked or injected.
//!
//! They only fire while a game is in front (a fullscreen window that isn't a
//! browser, Discord…), so typing in other apps never saves a clip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Keys that work alone: letters, digits and punctuation, by physical position
/// (the browser's `KeyboardEvent.code` without its prefix) → scan code.
const KEYS: &[(&str, u16)] = &[
    ("`", 0x29), ("Backquote", 0x29),
    ("1", 0x02), ("2", 0x03), ("3", 0x04), ("4", 0x05), ("5", 0x06),
    ("6", 0x07), ("7", 0x08), ("8", 0x09), ("9", 0x0A), ("0", 0x0B),
    ("Minus", 0x0C), ("Equal", 0x0D),
    ("Q", 0x10), ("W", 0x11), ("E", 0x12), ("R", 0x13), ("T", 0x14), ("Y", 0x15), ("U", 0x16),
    ("I", 0x17), ("O", 0x18), ("P", 0x19), ("BracketLeft", 0x1A), ("BracketRight", 0x1B),
    ("A", 0x1E), ("S", 0x1F), ("D", 0x20), ("F", 0x21), ("G", 0x22), ("H", 0x23), ("J", 0x24),
    ("K", 0x25), ("L", 0x26), ("Semicolon", 0x27), ("Quote", 0x28), ("Backslash", 0x2B),
    ("Z", 0x2C), ("X", 0x2D), ("C", 0x2E), ("V", 0x2F), ("B", 0x30), ("N", 0x31), ("M", 0x32),
    ("Comma", 0x33), ("Period", 0x34), ("Slash", 0x35), ("IntlBackslash", 0x56),
];

/// The scan code of a single-key hotkey, or None for combinations ("Alt+Z") and
/// keys the normal hotkey API handles well (F9, Insert…).
pub fn single_key(hotkey: &str) -> Option<u16> {
    let k = hotkey.trim();
    KEYS.iter().find(|(name, _)| name.eq_ignore_ascii_case(k)).map(|&(_, sc)| sc)
}

type Handler = Box<dyn Fn(usize) + Send + Sync>;

static BINDINGS: Mutex<Vec<(u16, usize)>> = Mutex::new(Vec::new());
static PAUSED: AtomicBool = AtomicBool::new(false);
static HANDLER: OnceLock<Handler> = OnceLock::new();
static DOWN: Mutex<Vec<u16>> = Mutex::new(Vec::new());
/// Off only in the live test (no game is in front there).
static NEED_GAME: AtomicBool = AtomicBool::new(true);

/// Which single keys do what (role = index of the hotkey). Starts the listener
/// the first time a single key is set.
pub fn set_bindings(keys: Vec<(u16, usize)>) {
    let any = !keys.is_empty();
    *BINDINGS.lock().unwrap() = keys;
    if any {
        start();
    }
}

/// While the user records a new hotkey, single keys must not fire.
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::SeqCst);
}

/// `on_press(role)` runs on the listener thread.
pub fn init(on_press: impl Fn(usize) + Send + Sync + 'static) {
    let _ = HANDLER.set(Box::new(on_press));
}

/// A key went down (scan code, already without repeats).
fn pressed(sc: u16) {
    if PAUSED.load(Ordering::SeqCst) {
        return;
    }
    let role = BINDINGS.lock().unwrap().iter().find(|(k, _)| *k == sc).map(|&(_, r)| r);
    let Some(role) = role else { return };
    if modifier_held() || (NEED_GAME.load(Ordering::SeqCst) && !game_in_front()) {
        return;
    }
    if let Some(h) = HANDLER.get() {
        h(role);
    }
}

/// Ctrl+L in a browser, Alt+Tab… aren't the single key.
#[cfg(windows)]
fn modifier_held() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN};
    [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN].iter().any(|v| unsafe { GetAsyncKeyState(v.0 as i32) } as u16 & 0x8000 != 0)
}

#[cfg(windows)]
fn game_in_front() -> bool {
    use crate::capture::game;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let Some(w) = game::foreground() else { return false };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(HWND(w.hwnd as *mut _), Some(&mut pid)) };
    pid != std::process::id() && game::looks_like_game(&w)
}

#[cfg(not(windows))]
fn modifier_held() -> bool {
    false
}
#[cfg(not(windows))]
fn game_in_front() -> bool {
    false
}

/// Tracks held keys so holding one down fires once. Returns true on a new press.
fn key_event(sc: u16, up: bool) -> bool {
    let mut down = DOWN.lock().unwrap();
    let held = down.contains(&sc);
    if up {
        down.retain(|&k| k != sc);
        false
    } else if held {
        false
    } else {
        down.push(sc);
        true
    }
}

#[cfg(windows)]
fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("clipit-keys".into()).spawn(listen);
    });
}

#[cfg(not(windows))]
fn start() {}

/// A message-only window that receives WM_INPUT for the keyboard, also while
/// another app (the game) has focus.
#[cfg(windows)]
fn listen() {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_INPUT {
            let mut raw = RAWINPUT::default();
            let mut size = std::mem::size_of::<RAWINPUT>() as u32;
            let got = unsafe {
                GetRawInputData(HRAWINPUT(lp.0 as *mut _), RID_INPUT, Some(&mut raw as *mut _ as *mut _), &mut size, std::mem::size_of::<RAWINPUTHEADER>() as u32)
            };
            if got != u32::MAX && raw.header.dwType == RIM_TYPEKEYBOARD.0 {
                let kb = unsafe { raw.data.keyboard };
                // E0-prefixed keys (arrows, numpad Enter…) aren't the ones we bind.
                let e0 = kb.Flags as u32 & RI_KEY_E0 != 0;
                let up = kb.Flags as u32 & RI_KEY_BREAK != 0;
                if !e0 && key_event(kb.MakeCode, up) {
                    pressed(kb.MakeCode);
                }
            }
        }
        unsafe { DefWindowProcW(h, msg, wp, lp) }
    }

    unsafe {
        let Ok(module) = GetModuleHandleW(None) else { return };
        let class = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: module.into(), lpszClassName: w!("ClipitKeys"), ..Default::default() };
        RegisterClassW(&class);
        let Ok(hwnd) = CreateWindowExW(Default::default(), w!("ClipitKeys"), w!(""), Default::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(module.into()), None) else {
            return;
        };
        let dev = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        if RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32).is_err() {
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_keys_are_letters_digits_and_punctuation() {
        assert_eq!(single_key("L"), Some(0x26));
        assert_eq!(single_key("l"), Some(0x26));
        assert_eq!(single_key("Slash"), Some(0x35));
        assert_eq!(single_key("BracketRight"), Some(0x1B));
        assert_eq!(single_key("0"), Some(0x0B));
        assert_eq!(single_key("Alt+Z"), None);
        assert_eq!(single_key("F9"), None);
        assert_eq!(single_key(""), None);
    }

    /// Presses F24 (a key nothing uses) and checks the listener sees it once:
    /// cargo test keys_live -- --ignored --nocapture
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn keys_live() {
        use std::sync::atomic::AtomicUsize;
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        static HITS: AtomicUsize = AtomicUsize::new(0);
        NEED_GAME.store(false, Ordering::SeqCst);
        init(|role| {
            assert_eq!(role, 7);
            HITS.fetch_add(1, Ordering::SeqCst);
        });
        set_bindings(vec![(0x76, 7)]); // F24
        std::thread::sleep(std::time::Duration::from_millis(300));
        let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F24, wScan: 0x76, dwFlags: flags, ..Default::default() } },
        };
        // Down, auto-repeat, up: one press.
        let inputs = [key(KEYEVENTF_SCANCODE), key(KEYEVENTF_SCANCODE), key(KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP)];
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
        set_paused(true);
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(HITS.load(Ordering::SeqCst), 1, "paused while recording a hotkey");
        println!("raw input: F24 seen once, ignored while paused");
    }

    #[test]
    fn holding_a_key_fires_once() {
        assert!(key_event(0x26, false));
        assert!(!key_event(0x26, false)); // auto-repeat
        assert!(!key_event(0x26, true));
        assert!(key_event(0x26, false));
        key_event(0x26, true);
    }
}
