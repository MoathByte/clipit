//! Background work, done only while you're away and no game is running:
//! - reading the kill times of older VALORANT clips (timeline marks),
//! - making VALORANT clips without kills smaller (compression), with undo.
//!
//! Everything runs at low priority and stops the moment a game starts or you
//! come back (unless you asked for it yourself with "Compress" on a selection).

use crate::config::Background;
use crate::{library, scanner, AppState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Where originals wait (inside the clips folder they came from, so moving
/// them is instant and they stay on the same drive). Hidden from the library.
pub const ORIGINALS_DIR: &str = ".clipit-originals";
/// A compressed copy being written (not a video extension, so never listed).
const TEMP_EXT: &str = "clipit-tmp";
/// Keep a compressed copy only if it's at most this share of the original.
const WORTH_IT: f64 = 0.85;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub original: u64,
    pub size: u64,
    /// The original, kept for undo (None = not kept, or kept time is over).
    pub backup: Option<String>,
    pub at_ms: u64,
    /// Shown in a "Cleared …" notice already.
    #[serde(default)]
    pub seen: bool,
    /// Undone, or not worth it: never compressed automatically again.
    #[serde(default)]
    pub skip: bool,
}

pub struct Worker {
    file: PathBuf,
    entries: Mutex<Vec<Entry>>,
    /// Clips you asked to compress (from a selection): done first, even when not away.
    queue: Mutex<Vec<String>>,
    busy: Mutex<Option<String>>,
    wake: AtomicBool,
}

#[derive(Serialize, Clone)]
pub struct State {
    pub entries: Vec<Entry>,
    pub busy: Option<String>,
    pub queued: usize,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn key(p: &str) -> String {
    p.to_lowercase()
}

impl Worker {
    pub fn load(file: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { file, entries: Mutex::new(entries), queue: Mutex::new(Vec::new()), busy: Mutex::new(None), wake: AtomicBool::new(false) }
    }

    fn save(&self, entries: &[Entry]) {
        if let Some(d) = self.file.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(json) = serde_json::to_string(entries) {
            let tmp = self.file.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
    }

    pub fn state(&self) -> State {
        State {
            entries: self.entries.lock().unwrap().iter().filter(|e| !e.skip).cloned().collect(),
            busy: self.busy.lock().unwrap().clone(),
            queued: self.queue.lock().unwrap().len(),
        }
    }

    pub fn mark_seen(&self) {
        let mut e = self.entries.lock().unwrap();
        e.iter_mut().for_each(|x| x.seen = true);
        self.save(&e);
    }

    pub fn enqueue(&self, paths: Vec<String>) -> usize {
        let mut q = self.queue.lock().unwrap();
        for p in paths {
            if !q.iter().any(|x| key(x) == key(&p)) {
                q.push(p);
            }
        }
        self.wake.store(true, Ordering::SeqCst);
        q.len()
    }

    fn known(&self, path: &str) -> bool {
        self.entries.lock().unwrap().iter().any(|e| key(&e.path) == key(path))
    }

    fn record(&self, entry: Entry) {
        let mut e = self.entries.lock().unwrap();
        e.retain(|x| key(&x.path) != key(&entry.path));
        e.push(entry);
        self.save(&e);
    }

    /// A clip was renamed or moved: its record follows it.
    pub fn moved(&self, from: &str, to: Option<&str>) {
        let mut e = self.entries.lock().unwrap();
        match to {
            Some(to) => e.iter_mut().filter(|x| key(&x.path) == key(from)).for_each(|x| x.path = to.to_string()),
            None => e.retain(|x| key(&x.path) != key(from)),
        }
        self.save(&e);
    }

    /// Puts the originals back. Returns how many were restored.
    pub fn revert(&self, paths: &[String]) -> Result<u32, String> {
        let mut entries = self.entries.lock().unwrap();
        let mut done = 0;
        let mut errors = Vec::new();
        for p in paths {
            let Some(e) = entries.iter_mut().find(|x| key(&x.path) == key(p) && !x.skip) else { continue };
            let Some(backup) = e.backup.clone().filter(|b| Path::new(b).is_file()) else {
                errors.push(format!("{}: the original isn't kept any more", file_name(p)));
                continue;
            };
            let target = Path::new(&e.path);
            let aside = target.with_extension(TEMP_EXT);
            // Compressed copy aside first, so a failed move loses nothing.
            if let Err(err) = std::fs::rename(target, &aside) {
                errors.push(format!("{}: {err}", file_name(p)));
                continue;
            }
            match std::fs::rename(&backup, target) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&aside);
                    e.backup = None;
                    e.skip = true;
                    done += 1;
                }
                Err(err) => {
                    let _ = std::fs::rename(&aside, target);
                    errors.push(format!("{}: {err}", file_name(p)));
                }
            }
        }
        self.save(&entries);
        if done == 0 && !errors.is_empty() {
            return Err(errors.join("; "));
        }
        Ok(done)
    }

    /// Deletes originals kept longer than `keep_days`.
    fn purge(&self, keep_days: u32) {
        let limit = keep_days as u64 * 86_400_000;
        let now = now_ms();
        let mut entries = self.entries.lock().unwrap();
        let mut changed = false;
        for e in entries.iter_mut() {
            if let Some(b) = &e.backup {
                if now.saturating_sub(e.at_ms) >= limit || !Path::new(b).is_file() {
                    let _ = std::fs::remove_file(b);
                    e.backup = None;
                    changed = true;
                }
            }
        }
        if changed {
            self.save(&entries);
        }
    }
}

fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ---------- when is it OK to work ----------

/// Milliseconds since the last mouse or keyboard input.
#[cfg(windows)]
fn idle_ms() -> u64 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            return GetTickCount().wrapping_sub(info.dwTime) as u64;
        }
    }
    0
}
#[cfg(not(windows))]
fn idle_ms() -> u64 {
    0
}

/// A game is running: in front, being captured, or being recorded.
pub(crate) fn game_running(app: &AppHandle) -> bool {
    use crate::capture::game;
    if game::foreground().map(|w| game::looks_like_game(&w)).unwrap_or(false) {
        return true;
    }
    let Some(s) = app.try_state::<AppState>() else { return true };
    let st = s.engine.status();
    let target_game = s.config.lock().unwrap().capture.target == "game";
    st.recording.is_some() || (target_game && st.state == "buffering")
}

fn away_enough(b: &Background) -> bool {
    b.away_minutes == 0 || idle_ms() >= b.away_minutes as u64 * 60_000
}

// ---------- the loop ----------

pub fn start(app: AppHandle) {
    std::thread::Builder::new()
        .name("idle-work".into())
        .spawn(move || {
            // Let the app start up first.
            std::thread::sleep(Duration::from_secs(60));
            let mut candidates: Vec<scanner::Clip> = Vec::new();
            let mut scanned: Option<Instant> = None;
            loop {
                let Some(w) = app.try_state::<std::sync::Arc<Worker>>().map(|w| w.inner().clone()) else { return };
                let manual = !w.queue.lock().unwrap().is_empty();
                if manual {
                    run_queue(&app, &w);
                }
                let cfg = app.state::<AppState>().config.lock().unwrap().background.clone();
                w.purge(cfg.keep_days);
                if (cfg.index_kills || cfg.compress) && !game_running(&app) && away_enough(&cfg) {
                    if scanned.map_or(true, |t| t.elapsed() > Duration::from_secs(600)) || candidates.is_empty() && scanned.map_or(true, |t| t.elapsed() > Duration::from_secs(120)) {
                        candidates = library_clips(&app);
                        scanned = Some(Instant::now());
                    }
                    idle_pass(&app, &w, &cfg, &mut candidates);
                }
                // Sleep ~20 s, or less when a selection is queued.
                for _ in 0..40 {
                    if w.wake.swap(false, Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .expect("idle thread");
}

fn library_clips(app: &AppHandle) -> Vec<scanner::Clip> {
    let s = app.state::<AppState>();
    let folders = s.config.lock().unwrap().library_folders();
    let (favorites, reviewed, meta) = (s.favorites.snapshot(), s.reviewed.snapshot(), s.meta.snapshot());
    scanner::scan(&folders, &scanner::Flags { favorites: &favorites, reviewed: &reviewed, meta: &meta }).clips
}

fn is_valorant(c: &scanner::Clip) -> bool {
    c.game.trim().eq_ignore_ascii_case("valorant") || c.map.is_some() || c.agent.is_some()
}

/// One step at a time, re-checking that you're still away between steps.
fn idle_pass(app: &AppHandle, w: &Worker, cfg: &Background, candidates: &mut Vec<scanner::Clip>) {
    let s = app.state::<AppState>();
    let count_kills = s.config.lock().unwrap().capture.count_kills;
    let ok = || !game_running(app) && away_enough(cfg) && w.queue.lock().unwrap().is_empty();
    // 1. Kill times for older VALORANT clips, newest first.
    if cfg.index_kills && count_kills {
        let mut todo: Vec<usize> = (0..candidates.len()).filter(|&i| is_valorant(&candidates[i]) && candidates[i].kill_times.is_none() && !candidates[i].edited).collect();
        todo.sort_by_key(|&i| std::cmp::Reverse(candidates[i].modified_ms));
        for i in todo {
            if !ok() {
                return;
            }
            let path = candidates[i].path.clone();
            *w.busy.lock().unwrap() = Some(format!("Reading kills: {}", file_name(&path)));
            let res = s.engine.kill_times(Path::new(&path));
            *w.busy.lock().unwrap() = None;
            match res {
                Ok(times) => {
                    let info = library::ClipInfo { kills: Some(candidates[i].kills.unwrap_or(times.len() as u32)), kill_times: Some(times.clone()), ..Default::default() };
                    if let Ok(merged) = s.meta.set(&path, &info) {
                        let _ = app.emit("clip-info", serde_json::json!({ "path": path, "info": merged, "quiet": true }));
                    }
                    candidates[i].kill_times = Some(times);
                    candidates[i].kills = info.kills;
                }
                // Unreadable (in use, broken): don't try again this session.
                Err(_) => candidates[i].kill_times = Some(Vec::new()),
            }
        }
    }
    // 2. Compression of VALORANT clips without kills.
    if !cfg.compress {
        return;
    }
    let min_age = cfg.min_age_hours as u64 * 3_600_000;
    let now = now_ms();
    let todo: Vec<String> = candidates
        .iter()
        .filter(|c| is_valorant(c) && c.kills == Some(0) && !c.edited && !(cfg.skip_favorites && c.favorite))
        .filter(|c| now.saturating_sub(c.modified_ms) >= min_age && !w.known(&c.path))
        .map(|c| c.path.clone())
        .collect();
    let mut done: Vec<Entry> = Vec::new();
    for path in todo {
        if !ok() {
            break;
        }
        match compress_one(app, w, &path, cfg, &|| ok()) {
            Ok(Some(e)) => done.push(e),
            Ok(None) => {}
            Err(_) => {}
        }
    }
    candidates.retain(|c| !w.known(&c.path));
    if !done.is_empty() {
        announce(app, cfg, &done);
    }
}

/// Clips you selected and asked to compress.
fn run_queue(app: &AppHandle, w: &Worker) {
    let cfg = app.state::<AppState>().config.lock().unwrap().background.clone();
    let mut done = Vec::new();
    let mut failed = Vec::new();
    loop {
        let Some(path) = w.queue.lock().unwrap().first().cloned() else { break };
        // Never while you play; it waits for the game to close.
        while game_running(app) {
            std::thread::sleep(Duration::from_secs(5));
        }
        match compress_one(app, w, &path, &cfg, &|| !game_running(app)) {
            Ok(Some(e)) => done.push(e),
            Ok(None) => {}
            Err(e) => failed.push(format!("{}: {e}", file_name(&path))),
        }
        w.queue.lock().unwrap().retain(|p| key(p) != key(&path));
        let _ = app.emit("compress-progress", w.state());
    }
    if !done.is_empty() {
        announce(app, &cfg, &done);
    } else {
        let _ = app.emit("compress-done", serde_json::json!({ "count": 0, "saved": 0, "failed": failed }));
    }
}

fn announce(app: &AppHandle, cfg: &Background, done: &[Entry]) {
    let saved: u64 = done.iter().map(|e| e.original.saturating_sub(e.size)).sum();
    let _ = app.emit("compress-done", serde_json::json!({ "count": done.len(), "saved": saved, "paths": done.iter().map(|e| e.path.clone()).collect::<Vec<_>>() }));
    if cfg.notify {
        use tauri_plugin_notification::NotificationExt;
        let body = if cfg.keep_days > 0 {
            format!("{} clip{} without kills made smaller. Open Clipit to see them or undo it (originals kept {} days).", done.len(), if done.len() == 1 { "" } else { "s" }, cfg.keep_days)
        } else {
            format!("{} clip{} without kills made smaller.", done.len(), if done.len() == 1 { "" } else { "s" })
        };
        let _ = app.notification().builder().title(format!("Cleared {}", fmt_size(saved))).body(body).show();
    }
}

pub fn fmt_size(b: u64) -> String {
    let mb = b as f64 / 1_048_576.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

// ---------- compressing one clip ----------

/// Video bitrate to aim for, as a share of the clip's own.
fn share(quality: &str) -> f64 {
    match quality {
        "light" => 0.6,
        "small" => 0.25,
        _ => 0.4,
    }
}

/// Seconds and overall kb/s, from ffmpeg's description of the file.
fn probe(ffmpeg: &Path, clip: &Path) -> Option<(f64, f64)> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-i"]).arg(clip).stdout(Stdio::null()).stderr(Stdio::piped()).stdin(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    let d = text.split("Duration: ").nth(1)?.split(',').next()?;
    let parts: Vec<f64> = d.trim().split(':').filter_map(|x| x.parse().ok()).collect();
    if parts.len() != 3 {
        return None;
    }
    let secs = parts[0] * 3600.0 + parts[1] * 60.0 + parts[2];
    let size = std::fs::metadata(clip).ok()?.len() as f64;
    (secs > 0.5).then(|| (secs, size * 8.0 / secs / 1000.0))
}

pub fn compress_args(input: &Path, output: &Path, encoder: &str, kbps: u32) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(input.to_string_lossy().into_owned());
    a.extend(["-map", "0:v:0", "-map", "0:a?", "-map_metadata", "0", "-c:a", "copy"].iter().map(|s| s.to_string()));
    let (b, m, buf) = (format!("{kbps}k"), format!("{}k", kbps * 3 / 2), format!("{}k", kbps * 2));
    let enc: Vec<&str> = match encoder {
        "h264_nvenc" => vec!["-c:v", "h264_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-multipass", "qres", "-spatial-aq", "1", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf],
        "h264_amf" => vec!["-c:v", "h264_amf", "-quality", "quality", "-rc", "vbr_peak", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf],
        "h264_qsv" => vec!["-c:v", "h264_qsv", "-preset", "slower", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf],
        _ => vec!["-c:v", "libx264", "-preset", "medium", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf],
    };
    a.extend(enc.iter().map(|s| s.to_string()));
    a.extend(["-pix_fmt", "yuv420p", "-movflags", "+faststart", "-f", "mp4"].iter().map(|s| s.to_string()));
    a.push(output.to_string_lossy().into_owned());
    a
}

/// Ok(Some) = compressed; Ok(None) = stopped or not worth it (recorded so it
/// isn't tried again); Err = failed.
fn compress_one(app: &AppHandle, w: &Worker, path: &str, cfg: &Background, keep_going: &dyn Fn() -> bool) -> Result<Option<Entry>, String> {
    let s = app.state::<AppState>();
    let ffmpeg = s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
    let clip = PathBuf::from(path);
    let meta = std::fs::metadata(&clip).map_err(|_| "the clip is gone".to_string())?;
    let original = meta.len();
    let mtime = meta.modified().ok();
    let (_, kbps) = probe(&ffmpeg, &clip).ok_or("couldn't read the clip")?;
    let target = ((kbps - 320.0).max(1000.0) * share(&cfg.quality)).max(2500.0) as u32;
    let tmp = clip.with_extension(TEMP_EXT);
    let encoders = s.engine.encoders();
    let mut order: Vec<&str> = ["h264_nvenc", "h264_amf", "h264_qsv"].into_iter().filter(|e| encoders.iter().any(|x| x == e)).collect();
    if encoders.is_empty() {
        order.push("h264_nvenc"); // not known yet: try the GPU first anyway
    }
    order.push("libx264");
    *w.busy.lock().unwrap() = Some(format!("Compressing {}", file_name(path)));
    let _ = app.emit("compress-progress", w.state());
    let mut result = Err("couldn't compress".to_string());
    for enc in order {
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(compress_args(&clip, &tmp, enc, target)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000 | 0x0000_4000); // no window, below normal priority
        let Ok(mut child) = cmd.spawn() else { continue };
        let stopped = loop {
            match child.try_wait() {
                Ok(Some(st)) => break if st.success() { None } else { Some(false) },
                Ok(None) => {}
                Err(_) => break Some(false),
            }
            if !keep_going() {
                let _ = child.kill();
                let _ = child.wait();
                break Some(true);
            }
            std::thread::sleep(Duration::from_millis(400));
        };
        match stopped {
            None => {
                result = Ok(());
                break;
            }
            Some(true) => {
                let _ = std::fs::remove_file(&tmp);
                *w.busy.lock().unwrap() = None;
                return Ok(None); // you came back / a game started: try later
            }
            Some(false) => {
                let _ = std::fs::remove_file(&tmp);
                continue; // that encoder didn't work here: next one
            }
        }
    }
    *w.busy.lock().unwrap() = None;
    result?;
    let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(u64::MAX);
    if size as f64 > original as f64 * WORTH_IT {
        let _ = std::fs::remove_file(&tmp);
        w.record(Entry { path: path.to_string(), original, size: original, backup: None, at_ms: now_ms(), seen: true, skip: true });
        return Ok(None);
    }
    // Original aside (kept for undo, or deleted), then the small copy in its place.
    let backup = if cfg.keep_days > 0 { Some(backup_path(app, &clip)?) } else { None };
    let moved = match &backup {
        Some(b) => {
            if let Some(d) = b.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                hide(d.ancestors().find(|a| a.file_name().map(|n| n == ORIGINALS_DIR).unwrap_or(false)).unwrap_or(d));
            }
            std::fs::rename(&clip, b)
        }
        None => std::fs::remove_file(&clip),
    };
    if let Err(e) = moved {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("the clip is in use ({e})"));
    }
    if let Err(e) = std::fs::rename(&tmp, &clip) {
        if let Some(b) = &backup {
            let _ = std::fs::rename(b, &clip);
        }
        return Err(e.to_string());
    }
    // Same date as before, so it stays where it was in the list.
    if let (Some(t), Ok(f)) = (mtime, std::fs::File::options().write(true).open(&clip)) {
        let _ = f.set_modified(t);
    }
    let entry = Entry { path: path.to_string(), original, size, backup: backup.map(|b| b.to_string_lossy().into_owned()), at_ms: now_ms(), seen: false, skip: false };
    w.record(entry.clone());
    let _ = app.emit("clip-updated", serde_json::json!({ "path": path, "size": size, "quiet": true }));
    Ok(Some(entry))
}

/// `<library folder>\.clipit-originals\<same sub-path>`.
fn backup_path(app: &AppHandle, clip: &Path) -> Result<PathBuf, String> {
    let folders = app.state::<AppState>().config.lock().unwrap().library_folders();
    let root = folders.iter().map(PathBuf::from).filter(|f| clip.starts_with(f)).max_by_key(|f| f.as_os_str().len()).ok_or("not in a clips folder")?;
    let rel = clip.strip_prefix(&root).map_err(|e| e.to_string())?;
    let mut b = root.join(ORIGINALS_DIR).join(rel);
    let mut n = 1;
    while b.exists() {
        b = root.join(ORIGINALS_DIR).join(rel.with_file_name(format!("{} ({n}).{}", rel.file_stem().unwrap_or_default().to_string_lossy(), rel.extension().unwrap_or_default().to_string_lossy())));
        n += 1;
    }
    Ok(b)
}

#[cfg(windows)]
fn hide(dir: &Path) {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    unsafe {
        let _ = SetFileAttributesW(&HSTRING::from(dir.as_os_str()), FILE_ATTRIBUTE_HIDDEN);
    }
}
#[cfg(not(windows))]
fn hide(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_args_keep_audio_and_date() {
        let a = compress_args(Path::new("C:\\c\\a.mp4"), Path::new("C:\\c\\a.clipit-tmp"), "h264_nvenc", 8000).join(" ");
        assert!(a.contains("-map 0:a? -map_metadata 0 -c:a copy"), "{a}");
        assert!(a.contains("-c:v h264_nvenc") && a.contains("-b:v 8000k -maxrate 12000k"), "{a}");
        assert!(a.ends_with("-f mp4 C:\\c\\a.clipit-tmp"), "{a}");
        assert_eq!(share("small"), 0.25);
    }

    #[test]
    fn records_revert_and_purge() {
        let dir = std::env::temp_dir().join(format!("clipit-idle-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(ORIGINALS_DIR)).unwrap();
        let clip = dir.join("a.mp4");
        let backup = dir.join(ORIGINALS_DIR).join("a.mp4");
        std::fs::write(&clip, b"small").unwrap();
        std::fs::write(&backup, b"the original").unwrap();
        let w = Worker::load(dir.join("compressed.json"));
        let p = clip.to_string_lossy().into_owned();
        w.record(Entry { path: p.clone(), original: 12, size: 5, backup: Some(backup.to_string_lossy().into_owned()), at_ms: now_ms(), seen: false, skip: false });
        assert!(w.known(&p.to_uppercase()));
        assert_eq!(w.state().entries.len(), 1);
        assert_eq!(w.revert(&[p.clone()]).unwrap(), 1);
        assert_eq!(std::fs::read(&clip).unwrap(), b"the original");
        assert!(!backup.exists());
        assert!(w.state().entries.is_empty(), "undone clips aren't listed");
        assert!(w.known(&p), "and never compressed again");
        // purge: a kept original past its time is deleted
        std::fs::write(&backup, b"x").unwrap();
        w.record(Entry { path: p.clone(), original: 12, size: 5, backup: Some(backup.to_string_lossy().into_owned()), at_ms: 0, seen: true, skip: false });
        w.purge(7);
        assert!(!backup.exists());
        assert!(w.revert(&[p]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
