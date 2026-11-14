//! What's on screen: the focused window, whether it's a (fullscreen) game,
//! monitors, and date-style names for clip files.

use serde::Serialize;

#[derive(Clone, Debug)]
pub struct Window {
    pub hwnd: isize,
    /// Executable file name, e.g. "VALORANT-Win64-Shipping.exe".
    pub exe: String,
    pub title: String,
    /// Covers its whole monitor (fullscreen or borderless).
    pub fullscreen: bool,
    pub class: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct Monitor {
    /// Stable id, e.g. "\\.\DISPLAY1".
    pub id: String,
    pub label: String,
    #[serde(skip)]
    pub handle: isize,
    pub primary: bool,
    pub width: i32,
    pub height: i32,
}

/// Apps that go fullscreen but aren't games.
const NOT_GAMES: &[&str] = &[
    "explorer.exe", "clipit.exe", "clipidia.exe", "msedgewebview2.exe", "applicationframehost.exe",
    "shellexperiencehost.exe", "startmenuexperiencehost.exe", "searchhost.exe", "searchapp.exe",
    "textinputhost.exe", "lockapp.exe", "systemsettings.exe", "taskmgr.exe", "screenclippinghost.exe",
    "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "opera_gx.exe", "vivaldi.exe",
    "arc.exe", "zen.exe", "vlc.exe", "mpv.exe", "potplayermini64.exe", "wmplayer.exe", "video.ui.exe",
    "mpc-hc64.exe", "obs64.exe", "discord.exe", "spotify.exe", "steamwebhelper.exe", "medal.exe",
    "nvidia overlay.exe", "nvidia app.exe", "powerpnt.exe", "claude.exe", "code.exe",
];

pub fn is_not_game(exe: &str) -> bool {
    let e = exe.to_ascii_lowercase();
    NOT_GAMES.contains(&e.as_str())
}

/// A fullscreen, non-system window.
pub fn looks_like_game(w: &Window) -> bool {
    w.fullscreen && !is_not_game(&w.exe) && !matches!(w.class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd")
}

/// A folder-friendly game name: the window title without junk, or the exe name.
pub fn display_name(title: &str, exe: &str) -> String {
    let t = sanitize(title.trim());
    if !t.is_empty() && t.len() <= 60 {
        return t;
    }
    let stem = exe.trim_end_matches(".exe").trim_end_matches(".EXE");
    let stem = stem.split(['-', '_']).next().unwrap_or(stem);
    let s = sanitize(stem);
    if s.is_empty() { "Game".into() } else { s }
}

/// Removes characters Windows doesn't allow in file names.
pub fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { ' ' } else { c })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches('.').to_string()
}

/// Timestamp for file names: "2026.11.11 - 14.03.22.07".
pub fn timestamp(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16) -> String {
    format!("{y:04}.{mo:02}.{d:02} - {h:02}.{mi:02}.{s:02}.{:02}", ms / 10)
}

#[cfg(windows)]
pub fn now_timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    timestamp(t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

#[cfg(windows)]
pub fn foreground() -> Option<Window> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let h = unsafe { GetForegroundWindow() };
    if h.is_invalid() {
        return None;
    }
    describe(h.0 as isize)
}

#[cfg(windows)]
pub fn describe(hwnd: isize) -> Option<Window> {
    use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let h = HWND(hwnd as *mut _);
        if !IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let mut exe = String::new();
        if let Ok(ph) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(ph, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                let full = String::from_utf16_lossy(&buf[..len as usize]);
                exe = full.rsplit('\\').next().unwrap_or("").to_string();
            }
            let _ = CloseHandle(ph);
        }
        let mut tbuf = [0u16; 512];
        let n = GetWindowTextW(h, &mut tbuf);
        let title = String::from_utf16_lossy(&tbuf[..n.max(0) as usize]);
        let mut cbuf = [0u16; 256];
        let n = GetClassNameW(h, &mut cbuf);
        let class = String::from_utf16_lossy(&cbuf[..n.max(0) as usize]);

        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let fullscreen = GetMonitorInfoW(mon, &mut mi).as_bool()
            && !IsIconic(h).as_bool()
            && r.left <= mi.rcMonitor.left
            && r.top <= mi.rcMonitor.top
            && r.right >= mi.rcMonitor.right
            && r.bottom >= mi.rcMonitor.bottom;
        Some(Window { hwnd, exe, title, fullscreen, class })
    }
}

#[cfg(windows)]
pub fn alive(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

/// Size of a window's client area (what gets captured).
#[cfg(windows)]
pub fn client_size(hwnd: isize) -> (u32, u32) {
    use windows::Win32::Foundation::{HWND, RECT};
    let mut r = RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(HWND(hwnd as *mut _), &mut r);
    }
    ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32)
}

/// The active window's handle (0 when there is none).
#[cfg(windows)]
pub fn foreground_hwnd() -> isize {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
}

/// The monitor a window is (mostly) on, as the same handle `monitors()` reports.
#[cfg(windows)]
pub fn monitor_of(hwnd: isize) -> isize {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    unsafe { MonitorFromWindow(HWND(hwnd as *mut _), MONITOR_DEFAULTTONULL).0 as isize }
}

/// Free space on the drive holding `path`, in bytes.
#[cfg(windows)]
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    use windows::core::PCWSTR;
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let mut free = 0u64;
    unsafe {
        windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None).ok()?;
    }
    Some(free)
}

/// All monitors, the main one first.
#[cfg(windows)]
pub fn monitors() -> Vec<Monitor> {
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::BOOL;
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<Monitor>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
            let r = mi.monitorInfo.rcMonitor;
            let n = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            let id = String::from_utf16_lossy(&mi.szDevice[..n]);
            let primary = mi.monitorInfo.dwFlags & 1 != 0; // MONITORINFOF_PRIMARY
            let (w, hgt) = (r.right - r.left, r.bottom - r.top);
            let num = id.trim_start_matches(r"\\.\DISPLAY").to_string();
            list.push(Monitor {
                label: format!("Display {num} · {w}×{hgt}{}", if primary { " (main)" } else { "" }),
                id,
                handle: h.0 as isize,
                primary,
                width: w,
                height: hgt,
            });
        }
        BOOL(1)
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| !m.primary);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(display_name("VALORANT  ", "VALORANT-Win64-Shipping.exe"), "VALORANT");
        assert_eq!(display_name("", "VALORANT-Win64-Shipping.exe"), "VALORANT");
        assert_eq!(display_name("Counter-Strike 2", "cs2.exe"), "Counter-Strike 2");
        assert_eq!(display_name("A: B / C?", "x.exe"), "A B C");
        assert_eq!(display_name("", ""), "Game");
        assert_eq!(timestamp(2026, 11, 9, 5, 52, 50, 607), "2026.11.09 - 05.52.50.60");
        // Our file names must parse as dated clips.
        let name = format!("VALORANT {}.DVR.mp4", timestamp(2026, 11, 9, 5, 52, 50, 607));
        assert_eq!(crate::scanner::parse_recorded(&name).as_deref(), Some("2026-11-09T05:52:50"));
        assert!(crate::scanner::custom_title(&name, "VALORANT").is_none());
    }

    #[test]
    fn not_games() {
        let w = |exe: &str, fullscreen: bool| Window { hwnd: 1, exe: exe.into(), title: "x".into(), fullscreen, class: "C".into() };
        assert!(looks_like_game(&w("VALORANT-Win64-Shipping.exe", true)));
        assert!(!looks_like_game(&w("VALORANT-Win64-Shipping.exe", false)));
        assert!(!looks_like_game(&w("chrome.exe", true)));
        assert!(!looks_like_game(&w("Explorer.EXE", true)));
    }
}
