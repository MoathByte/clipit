//! Clipit's recorder: an always-ready instant replay plus manual recording.
//!
//! How it stays light:
//! * Video: Windows Graphics Capture → hardware encoder (NVENC/AMF) in one ffmpeg
//!   process. Frames stay on the GPU (≈1% of one CPU core).
//! * The replay buffer lives in RAM as already-encoded video (≈150 MB per minute at
//!   1080p60) — zero SSD writes while you play. Replay tools that buffer on disk instead
//!   rewrites a temp file on disk all the time.
//! * Audio is kept as raw PCM in RAM and only encoded for the seconds you save.
//! * Saving writes the clip once, front to back (no temp file, no second pass).
//! * In "game" mode nothing runs at all until a game is in focus.

pub mod audio;
pub mod encoder;
pub mod game;
pub mod handcam;
pub mod killfeed;
pub mod valorant;
pub mod denoise;
pub mod mp4;
pub mod mux;
pub mod native;
pub mod packets;
pub mod sound;
pub mod ts;

use crate::config::{Capture, GameRule};
use audio::PcmRing;
use encoder::{Plan, Source};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Audio is shifted by this much relative to video (measured, see tests in
/// the docs/README). Positive = audio later.
const AUDIO_SHIFT_HNS: i64 = 0;

/// Longest wait, after the clip key, for the buffer to reach that moment.
const MAX_CATCH_UP_MS: u64 = 3000;
/// The buffer counts as up to date this close to the key press (encoder delay).
const CAUGHT_UP_HNS: i64 = 4_000_000;
/// A kill is counted in the killfeed about this long after it happens.
const KILL_SEEN_LATE_HNS: i64 = 8_000_000;
/// Kill times older than this are forgotten (no clip can reach that far back).
const KILL_LOG_KEEP_HNS: i64 = 30 * 60 * 10_000_000;

/// Shortest clip when clips don't repeat each other.
const MIN_CLIP_SECS: f64 = 5.0;

/// QPC clock in 100 ns units — the same clock WASAPI timestamps audio with.
pub fn now_hns() -> i64 {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
        let (mut c, mut f) = (0i64, 0i64);
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
        return (c as i128 * 10_000_000 / f.max(1) as i128) as i64;
    }
    #[allow(unreachable_code)]
    0
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct RecordingStatus {
    pub secs: f64,
    pub path: String,
}

#[derive(Serialize, Clone, Default, Debug)]
pub struct Status {
    /// "off", "waiting", "starting", "buffering", "error", "no-ffmpeg"
    pub state: String,
    pub message: String,
    pub source: String,
    pub encoder: String,
    pub detail: String,
    pub buffered_secs: f64,
    pub ram_bytes: u64,
    pub recording: Option<RecordingStatus>,
    /// Hand cam: the camera is being recorded.
    pub handcam_on: bool,
    /// Why the hand cam isn't working ("" = fine or off).
    pub handcam_error: String,
}

pub enum Event {
    /// `kills` is set for clips saved automatically (auto clipped highlights).
    /// `kill_times`: your kills (seconds into the clip), when they were seen live.
    ClipSaved { path: PathBuf, size: u64, kills: Option<u32>, kill_times: Option<Vec<f32>> },
    /// What a saved VALORANT clip shows: your kills, the map and your agent.
    ClipInfo { path: PathBuf, info: valorant::ClipInfo },
    ClipFailed(String),
    /// Saved, but something extra didn't work (e.g. the hand cam).
    Warning(String),
    /// A saved clip was replaced by a new version (the hand cam was added).
    ClipUpdated { path: PathBuf, size: u64 },
    /// A saved clip is still being worked on ("handcam": being replaced with the
    /// hand cam version; "reading": its map and agent are read). None = done.
    ClipBusy { path: PathBuf, task: Option<&'static str> },
    RecordingStarted(PathBuf),
    RecordingStopped { path: PathBuf, size: u64 },
    RecordingFailed(String),
    GameDetected(GameRule),
    Status(Status),
}

/// Fields that require restarting the capture pipeline when they change.
fn pipeline_key(c: &Capture) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        c.resolution, c.fps, c.quality, c.codec, c.encoder, c.cursor, c.target, c.monitor, c.desktop_audio, c.desktop_device, c.mic, c.mic_device, c.auto_clip_kills
    )
}

struct Running {
    source: Source,
    /// Game exe (lower case) + folder name, when capturing a game window.
    game: Option<(String, String)>,
    label: String,
    plan: Plan,
    key: String,
    backend: Backend,
    audio_stop: Arc<AtomicBool>,
    audio_threads: Vec<JoinHandle<()>>,
    /// Size of the recorded frames.
    size: (u32, u32),
    started: Instant,
}

/// What does the capturing: Clipit's own engine, or the classic ffmpeg pipeline.
enum Backend {
    Classic {
        child: Child,
        reader: Option<JoinHandle<()>>,
        stderr: Arc<Mutex<String>>,
        /// Killfeed tap pipe + the thread reading it (VALORANT kill clips).
        tap: Option<(String, JoinHandle<()>)>,
    },
    Native {
        engine: native::Engine,
        hevc: bool,
        tap: Option<JoinHandle<()>>,
    },
}

impl Running {
    fn is_native(&self) -> bool {
        matches!(self.backend, Backend::Native { .. })
    }

    /// Why it stopped working, if it did.
    fn trouble(&mut self) -> Option<String> {
        match &mut self.backend {
            Backend::Classic { child, stderr, .. } => match child.try_wait() {
                Ok(Some(_)) => Some(stderr.lock().unwrap().trim().lines().last().unwrap_or("The capture stopped unexpectedly").to_string()),
                _ => None,
            },
            Backend::Native { engine, .. } => {
                let s = engine.stats();
                if s.failed {
                    return Some(if s.error.is_empty() { "The new recorder stopped".into() } else { s.error });
                }
                // Watchdog: frames are paced even when the game doesn't draw, so no
                // packets for a while means capture or the encoder is stuck.
                let since = if s.last_packet_hns > 0 { now_hns() - s.last_packet_hns } else { 0 };
                // (No frames from Windows at all, e.g. a minimized game, isn't the engine's fault.)
                let stuck = self.started.elapsed() > Duration::from_secs(4) && s.captured > 0 && (s.encoded == 0 || since > NATIVE_STALL_HNS);
                stuck.then(|| "The new recorder stopped receiving frames".to_string())
            }
        }
    }
}

/// No packets for this long = the native engine is stuck: restart it (keeping the buffer).
const NATIVE_STALL_HNS: i64 = 30_000_000;
/// After this many failures in a row, use the classic recorder until Clipit restarts.
const NATIVE_MAX_FAILURES: u32 = 3;

/// A piece of the buffer, from whichever recorder made it.
enum Snap {
    Ts(ts::Snapshot),
    Native(packets::Snapshot),
}

impl Snap {
    fn start_hns(&self) -> i64 {
        match self {
            Snap::Ts(s) => s.start_hns(),
            Snap::Native(s) => s.start_hns(),
        }
    }
    fn end_hns(&self) -> i64 {
        match self {
            Snap::Ts(s) => s.end_hns(),
            Snap::Native(s) => s.end_hns(),
        }
    }
}

/// The hand cam's ffmpeg (camera -> its own RAM buffer).
struct CamRun {
    key: String,
    device: String,
    /// Started with a mode we picked (else the camera's defaults), and when.
    picked_mode: bool,
    started: Instant,
    child: Child,
    reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
}

struct Recording {
    path: PathBuf,
    /// QPC time of the first recorded frame (0 until the first keyframe arrives).
    start_hns: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    video: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
    pipe_name: Option<String>,
    mux: Option<mux::LiveMux>,
    failed: Arc<AtomicBool>,
    /// Hand cam for this recording: its file + QPC time of its first frame.
    cam: Option<(PathBuf, Arc<AtomicI64>)>,
    /// Recorded by the native engine (then `mux`, `video`, `audio` are unused).
    native: Option<NativeRec>,
}

struct NativeRec {
    part: PathBuf,
    tx: std::sync::mpsc::Sender<packets::RecMsg>,
    writer: JoinHandle<Result<u64, String>>,
    audio: JoinHandle<()>,
    /// Where the audio stops (the end of the last video frame), set when stopping.
    end_hns: Arc<AtomicI64>,
}

struct Shared {
    ffmpeg: Option<PathBuf>,
    ffmpeg_note: String,
    cfg: Mutex<Capture>,
    clips_root: Mutex<String>,
    encoders: Mutex<Option<Vec<String>>>,
    ring: Mutex<ts::Ring>,
    desktop: Arc<Mutex<PcmRing>>,
    mic: Arc<Mutex<PcmRing>>,
    keep_secs: AtomicU32,
    /// Extra seconds kept while watching for kills (a whole round fits).
    kill_keep: AtomicU32,
    /// Data folder (VALORANT reference images live in `valorant/`).
    data_dir: PathBuf,
    run: Mutex<Option<Running>>,
    recording: Mutex<Option<Recording>>,
    want_record: AtomicBool,
    cam: Mutex<Option<CamRun>>,
    cam_ring: Mutex<ts::Ring>,
    /// Last hand cam failure (shown in Settings) and when to try again.
    cam_error: Mutex<(String, Instant)>,
    /// The camera is lent to its settings window: don't record it meanwhile.
    cam_hold: AtomicBool,
    /// Last preview picture (device, JPEG), so adjustments preview instantly.
    preview: Mutex<Option<(String, Vec<u8>)>>,
    /// QPC end of the last clip saved with the clip key (the next one starts there).
    last_clip_end: Mutex<Option<i64>>,
    /// The native engine's buffer and how many seconds it keeps.
    pring: Arc<Mutex<packets::Ring>>,
    pkeep: Arc<AtomicU32>,
    /// A native recording in progress takes video packets from here.
    rec_tee: native::RecTee,
    /// The native engine is the one running now.
    native_on: AtomicBool,
    /// Native failures in a row (reset after a good run).
    native_failures: AtomicU32,
    /// Seconds the audio buffers hold (kept across recorder restarts when unchanged).
    audio_keep: AtomicU32,
    /// Your kills seen live in the killfeed (QPC, 100 ns). Only times, never video:
    /// a saved clip takes the ones inside it, the rest are dropped as they age.
    kill_log: Mutex<Vec<i64>>,
    /// Since when the killfeed has been watched without a break (None = not now).
    kill_watch_since: Mutex<Option<i64>>,
    /// One "running behind" warning per capture run is enough.
    warned_lag: AtomicBool,
    /// Clips still being worked on after saving, with what's being done.
    busy: Mutex<Vec<(PathBuf, &'static str)>>,
    /// Settings → Recording → Restart recorder was pressed.
    restart: AtomicBool,
    status: Mutex<Status>,
    job: Job,
    events: Box<dyn Fn(Event) + Send + Sync>,
}

#[derive(Clone)]
pub struct Engine {
    s: Arc<Shared>,
}

impl Engine {
    /// `ffmpeg` = one that can capture (None: `ffmpeg_note` says why recording can't start).
    pub fn start(ffmpeg: Option<PathBuf>, ffmpeg_note: String, cfg: Capture, clips_root: String, data_dir: PathBuf, events: Box<dyn Fn(Event) + Send + Sync>) -> Self {
        let s = Arc::new(Shared {
            ffmpeg,
            ffmpeg_note,
            keep_secs: AtomicU32::new(cfg.replay_secs + 2),
            kill_keep: AtomicU32::new(0),
            data_dir,
            cfg: Mutex::new(cfg),
            clips_root: Mutex::new(clips_root),
            encoders: Mutex::new(None),
            ring: Mutex::new(ts::Ring::new(166_667)),
            desktop: Arc::new(Mutex::new(PcmRing::new(10.0))),
            mic: Arc::new(Mutex::new(PcmRing::new(10.0))),
            run: Mutex::new(None),
            recording: Mutex::new(None),
            want_record: AtomicBool::new(false),
            cam: Mutex::new(None),
            cam_ring: Mutex::new(ts::Ring::new(333_333)),
            cam_error: Mutex::new((String::new(), Instant::now())),
            cam_hold: AtomicBool::new(false),
            preview: Mutex::new(None),
            last_clip_end: Mutex::new(None),
            pring: Arc::new(Mutex::new(packets::Ring::default())),
            pkeep: Arc::new(AtomicU32::new(62)),
            rec_tee: Arc::new(Mutex::new(None)),
            native_on: AtomicBool::new(false),
            native_failures: AtomicU32::new(0),
            audio_keep: AtomicU32::new(0),
            kill_log: Mutex::new(Vec::new()),
            kill_watch_since: Mutex::new(None),
            warned_lag: AtomicBool::new(false),
            busy: Mutex::new(Vec::new()),
            restart: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            job: Job::new(),
            events,
        });
        let engine = Engine { s };
        let e = engine.clone();
        std::thread::Builder::new()
            .name("capture-supervisor".into())
            .spawn(move || e.supervise())
            .expect("supervisor thread");
        engine
    }

    pub fn update(&self, cfg: Capture, clips_root: String) {
        self.s.keep_secs.store(cfg.replay_secs + 2, Ordering::SeqCst);
        *self.s.cfg.lock().unwrap() = cfg;
        *self.s.clips_root.lock().unwrap() = clips_root;
    }

    pub fn status(&self) -> Status {
        self.s.status.lock().unwrap().clone()
    }

    pub fn encoders(&self) -> Vec<String> {
        self.s.encoders.lock().unwrap().clone().unwrap_or_default()
    }

    pub fn is_recording(&self) -> bool {
        self.s.recording.lock().unwrap().is_some()
    }

    /// Settings → Recording → Restart recorder: starts capture, audio and the hand
    /// cam fresh (the replay buffer is emptied). Returns right away.
    pub fn restart(&self) {
        self.s.restart.store(true, Ordering::SeqCst);
    }

    fn restart_now(&self) {
        self.set_status("starting", "Restarting the recorder…", None);
        self.stop_recording(); // saved properly, like when quitting
        if let Some(r) = self.s.run.lock().unwrap().take() {
            stop_running(r);
        }
        if let Some(c) = self.s.cam.lock().unwrap().take() {
            stop_cam(c);
        }
        // Look for the video encoder again (a driver update or GPU reset may have changed it).
        *self.s.encoders.lock().unwrap() = None;
        *self.s.cam_error.lock().unwrap() = (String::new(), Instant::now());
        *self.s.last_clip_end.lock().unwrap() = None;
        self.s.warned_lag.store(false, Ordering::SeqCst);
    }

    /// Stops everything (app is quitting). Finishes a recording properly.
    pub fn shutdown(&self) {
        self.stop_recording();
        if let Some(r) = self.s.run.lock().unwrap().take() {
            stop_running(r);
        }
        if let Some(c) = self.s.cam.lock().unwrap().take() {
            stop_cam(c);
        }
    }

    // ---------- hand cam ----------

    /// Starts, restarts or stops the camera: on only while the game is captured.
    fn sync_cam(&self, ffmpeg: &Path, cfg: &Capture) {
        let hc = &cfg.handcam;
        let want = hc.enabled && !hc.device.trim().is_empty() && self.s.run.lock().unwrap().is_some() && !self.s.cam_hold.load(Ordering::SeqCst);
        let key = handcam::key(hc);
        let mut cam = self.s.cam.lock().unwrap();
        // Died (unplugged, used by another app)?
        let died = cam.as_mut().is_some_and(|c| matches!(c.child.try_wait(), Ok(Some(_))));
        if died {
            let c = cam.take().unwrap();
            let err = c.stderr.lock().unwrap().trim().lines().last().unwrap_or("").to_string();
            // Refused the size/rate we asked for? Try its own defaults right away.
            let retry = c.picked_mode && c.started.elapsed() < Duration::from_secs(8) && handcam::use_plain(&c.device);
            stop_cam(c);
            if retry {
                return;
            }
            let msg = if err.is_empty() || err.contains("Could not") || err.contains("I/O error") {
                "The camera isn't connected or is in use by another app.".to_string()
            } else {
                err
            };
            *self.s.cam_error.lock().unwrap() = (msg, Instant::now() + Duration::from_secs(10));
        }
        if cam.as_ref().is_some_and(|c| !want || c.key != key) {
            stop_cam(cam.take().unwrap());
        }
        if !want {
            if !hc.enabled {
                self.s.cam_error.lock().unwrap().0.clear();
            }
            return;
        }
        if cam.is_some() || Instant::now() < self.s.cam_error.lock().unwrap().1 {
            return;
        }
        match self.start_cam(ffmpeg, hc, key) {
            Ok(c) => {
                *cam = Some(c);
                self.s.cam_error.lock().unwrap().0.clear();
            }
            Err(e) => *self.s.cam_error.lock().unwrap() = (e, Instant::now() + Duration::from_secs(10)),
        }
    }

    fn start_cam(&self, ffmpeg: &Path, hc: &crate::config::HandCam, key: String) -> Result<CamRun, String> {
        let enc = encoder::pick(&self.encoders(), "h264", "auto").unwrap_or_else(|| "libx264".into());
        let mode = if handcam::plain(&hc.device) { None } else { handcam::pick_mode(&handcam::camera_modes(ffmpeg, &hc.device), hc.height, hc.fps) };
        let mut cmd = Command::new(ffmpeg);
        cmd.args(handcam::camera_args(hc, &enc, mode.as_ref())).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(encoder::CREATE_NO_WINDOW);
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
        self.s.job.add(&child);
        let fps: i64 = if hc.fps >= 60 { 60 } else { 30 };
        *self.s.cam_ring.lock().unwrap() = ts::Ring::new(10_000_000 / fps);
        let mut stdout = child.stdout.take().unwrap();
        let shared = self.s.clone();
        let per_sec = encoder::buffer_bytes(handcam::kbps(hc) * 3 / 2, 1) as usize;
        let reader = std::thread::Builder::new()
            .name("handcam-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    // As long as the game's buffer, plus a little.
                    let keep = shared.keep_secs.load(Ordering::SeqCst).max(shared.kill_keep.load(Ordering::SeqCst)) as f64 + 3.0;
                    let mut ring = shared.cam_ring.lock().unwrap();
                    ring.feed(&buf[..n], now_hns());
                    ring.trim(keep, per_sec * (keep as usize + 2));
                }
            })
            .map_err(|e| e.to_string())?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut err = child.stderr.take().unwrap();
        let sb = stderr.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 2048];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut s = sb.lock().unwrap();
                s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if s.len() > 4096 {
                    let cut = (s.len() - 2048..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                    s.drain(..cut);
                }
            }
        });
        Ok(CamRun { key, device: hc.device.clone(), picked_mode: mode.is_some(), started: Instant::now(), child, reader: Some(reader), stderr })
    }

    /// Is the camera being recorded (and has some picture)?
    fn cam_live(&self) -> bool {
        let on = self.s.cfg.lock().unwrap().handcam.enabled;
        on && self.s.cam.lock().unwrap().is_some() && self.s.cam_ring.lock().unwrap().header().is_some()
    }

    /// A still from `device` for the settings preview: from the running camera if
    /// it's that one (a camera can only be opened once), else straight from it.
    pub fn camera_frame(&self, device: &str) -> Result<Vec<u8>, String> {
        let ffmpeg = self.s.ffmpeg.clone().or_else(|| crate::media::find_ffmpeg().map(|f| f.path)).ok_or("ffmpeg isn't installed")?;
        let running = self.s.cam.lock().unwrap().as_ref().is_some_and(|c| c.device == device);
        if running {
            let snap = self.s.cam_ring.lock().unwrap().snapshot(0.5);
            if let Some(snap) = snap {
                let mut ts = snap.header.clone();
                for c in &snap.bytes {
                    ts.extend_from_slice(c);
                }
                return handcam::frame_from_ts(&ffmpeg, ts);
            }
        }
        handcam::grab_frame(&ffmpeg, device)
    }

    /// Preview picture with the adjustments in `hc` applied. `fresh` takes a new
    /// picture from the camera; otherwise the last one is reused (instant).
    pub fn camera_preview(&self, hc: &crate::config::HandCam, fresh: bool) -> Result<Vec<u8>, String> {
        let ffmpeg = self.s.ffmpeg.clone().or_else(|| crate::media::find_ffmpeg().map(|f| f.path)).ok_or("ffmpeg isn't installed")?;
        let cached = self.s.preview.lock().unwrap().clone().filter(|(d, _)| !fresh && *d == hc.device);
        let raw = match cached {
            Some((_, jpeg)) => jpeg,
            None => {
                let jpeg = self.camera_frame(&hc.device)?;
                *self.s.preview.lock().unwrap() = Some((hc.device.clone(), jpeg.clone()));
                jpeg
            }
        };
        handcam::adjust_jpeg(&ffmpeg, &raw, hc)
    }

    /// Opens the camera's own settings window. The camera is released while it's
    /// open (a camera can only be used by one program at a time).
    pub fn camera_settings(&self, device: &str) -> Result<(), String> {
        let ffmpeg = self.s.ffmpeg.clone().or_else(|| crate::media::find_ffmpeg().map(|f| f.path)).ok_or("ffmpeg isn't installed")?;
        self.s.cam_hold.store(true, Ordering::SeqCst);
        if let Some(c) = self.s.cam.lock().unwrap().take() {
            stop_cam(c);
        }
        let res = handcam::settings_dialog(&ffmpeg, device);
        self.s.cam_hold.store(false, Ordering::SeqCst);
        *self.s.preview.lock().unwrap() = None; // the picture changed
        res
    }

    pub fn cameras(&self) -> Vec<String> {
        match self.s.ffmpeg.clone().or_else(|| crate::media::find_ffmpeg().map(|f| f.path)) {
            Some(f) => handcam::list_cameras(&f),
            None => Vec::new(),
        }
    }

    /// The camera for a clip's time range, written as MPEG-TS to `file`.
    /// Returns the QPC time of its first frame.
    fn cam_for_range(&self, start_hns: i64, end_hns: i64, file: &Path) -> Option<i64> {
        let snap = self.s.cam_ring.lock().unwrap().snapshot_range(start_hns - 10_000_000, end_hns)?;
        std::fs::create_dir_all(file.parent()?).ok()?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(file).ok()?);
        f.write_all(&snap.header).ok()?;
        for c in &snap.bytes {
            f.write_all(c).ok()?;
        }
        f.flush().ok()?;
        Some(snap.start_hns())
    }

    /// While recording, the camera is also written to a file (from its next keyframe).
    fn start_cam_recording(&self) -> Option<(PathBuf, Arc<AtomicI64>)> {
        let file = self.s.data_dir.join("handcam").join(format!("rec-{}.ts", unique_id()));
        std::fs::create_dir_all(file.parent()?).ok()?;
        let mut ring = self.s.cam_ring.lock().unwrap();
        let header = ring.header()?;
        let offset = ring.offset_hns()?;
        let mut w = std::io::BufWriter::new(std::fs::File::create(&file).ok()?);
        w.write_all(&header).ok()?;
        let start = Arc::new(AtomicI64::new(0));
        let first = start.clone();
        let mut started = false;
        ring.tee = Some(Box::new(move |p: &[u8], keyframe: bool, pts: Option<i64>| {
            if !started {
                if !keyframe {
                    return;
                }
                started = true;
                first.store(ts::pts_to_hns(pts.unwrap_or(0)) + offset, Ordering::SeqCst);
            }
            let _ = w.write_all(p);
        }));
        Some((file, start))
    }


    // ---------- supervisor ----------

    fn supervise(&self) {
        let mut backoff_until = Instant::now();
        let mut failures = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(400));
            if self.s.restart.swap(false, Ordering::SeqCst) {
                self.restart_now();
                failures = 0;
                backoff_until = Instant::now();
            }
            let cfg = self.s.cfg.lock().unwrap().clone();
            let Some(ffmpeg) = self.s.ffmpeg.clone() else {
                self.set_status("no-ffmpeg", &self.s.ffmpeg_note, None);
                continue;
            };
            if self.s.encoders.lock().unwrap().is_none() {
                self.set_status("starting", "Checking your GPU's video encoder…", None);
                let found = encoder::probe(&ffmpeg);
                *self.s.encoders.lock().unwrap() = Some(found);
            }
            self.sync_cam(&ffmpeg, &cfg);

            let recording = self.is_recording();
            let want = cfg.enabled || recording || self.s.want_record.load(Ordering::SeqCst);
            let mut run = self.s.run.lock().unwrap();

            // Did the encoder die?
            if let Some(r) = run.as_mut() {
                if let Some(msg) = r.trouble() {
                    let game_gone = matches!(r.source, Source::Window(h) if !game::alive(h));
                    let r = run.take().unwrap();
                    let was_native = r.is_native();
                    drop(run);
                    // A native recording carries on through an engine restart (same buffer, same file).
                    let gives_up = self.s.native_failures.load(Ordering::SeqCst) + 1 >= NATIVE_MAX_FAILURES;
                    if !was_native || game_gone || gives_up {
                        self.stop_recording();
                    }
                    stop_running(r);
                    self.s.native_on.store(false, Ordering::SeqCst);
                    if was_native && !game_gone {
                        // Restart right away: the buffer is kept, so a clip still has what came before.
                        self.native_failed(&msg);
                        backoff_until = Instant::now() + Duration::from_millis(300);
                        continue;
                    }
                    if !game_gone {
                        failures += 1;
                        backoff_until = Instant::now() + Duration::from_secs(if failures > 3 { 30 } else { 2 });
                        self.set_status("error", &format!("Capture stopped: {msg}"), None);
                    }
                    continue;
                }
                // A good native run clears the failure count.
                if r.is_native() && r.started.elapsed() > Duration::from_secs(60) {
                    self.s.native_failures.store(0, Ordering::SeqCst);
                }
            }

            let desired = if want { self.desired_source(&cfg, run.as_ref()) } else { None };
            let key = format!("{}|{}", pipeline_key(&cfg), if self.use_native(&cfg) { "native" } else { "classic" });
            let keep = match (run.as_ref(), desired.as_ref()) {
                (Some(r), Some((src, _, _))) => r.source == *src && r.key == key,
                _ => false,
            };
            if !keep {
                if let Some(r) = run.take() {
                    drop(run);
                    self.stop_recording();
                    stop_running(r);
                    run = self.s.run.lock().unwrap();
                }
            }
            if run.is_none() {
                match desired {
                    Some((src, label, game)) if Instant::now() >= backoff_until => {
                        let native = self.use_native(&cfg);
                        let started = if native { self.start_native(&cfg, src, label, game, key) } else { self.start_running(&ffmpeg, &cfg, src, label, game, key) };
                        match started {
                            Ok(r) => {
                                self.s.native_on.store(r.is_native(), Ordering::SeqCst);
                                *run = Some(r);
                                failures = 0;
                            }
                            Err(e) if native => {
                                // Couldn't start the new recorder: count it and try again (or fall back).
                                drop(run);
                                self.native_failed(&e);
                                backoff_until = Instant::now() + Duration::from_secs(1);
                                continue;
                            }
                            Err(e) => {
                                failures += 1;
                                backoff_until = Instant::now() + Duration::from_secs(if failures > 3 { 30 } else { 3 });
                                drop(run);
                                self.set_status("error", &e, None);
                                continue;
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        drop(run);
                        if !want {
                            self.set_status("off", "Instant replay is off.", None);
                        } else {
                            self.set_status("waiting", "Waiting for a game — capture starts when one is in focus.", None);
                        }
                        continue;
                    }
                }
            }
            if let Some(r) = run.as_ref() {
                let (secs, video_bytes) = if r.is_native() {
                    let ring = self.s.pring.lock().unwrap();
                    (ring.buffered_secs(), ring.bytes())
                } else {
                    let ring = self.s.ring.lock().unwrap();
                    (ring.buffered_secs(), ring.bytes())
                };
                let ram = video_bytes + self.s.desktop.lock().unwrap().bytes() + self.s.mic.lock().unwrap().bytes();
                let detail = format!(
                    "{} · {} fps · {} Mbps",
                    if r.plan.width > 0 { format!("{}×{}", r.plan.width, r.plan.height) } else { "native size".into() },
                    r.plan.fps,
                    r.plan.kbps / 1000
                );
                let rec = self.s.recording.lock().unwrap().as_ref().map(|rec| {
                    let start = rec.start_hns.load(Ordering::SeqCst);
                    RecordingStatus { secs: if start > 0 { (now_hns() - start) as f64 / 1e7 } else { 0.0 }, path: rec.path.to_string_lossy().into_owned() }
                });
                let st = Status {
                    state: if secs > 0.0 { "buffering" } else { "starting" }.into(),
                    message: format!("Capturing {}", r.label),
                    source: r.label.clone(),
                    encoder: match &r.backend {
                        Backend::Native { hevc, .. } => format!("Graphics card · {} · Clipit engine", if *hevc { "HEVC" } else { "H.264" }),
                        Backend::Classic { .. } => format!("{} · classic", friendly_encoder(&r.plan.encoder)),
                    },
                    detail,
                    buffered_secs: secs.min(cfg.replay_secs as f64),
                    ram_bytes: (ram + self.s.cam_ring.lock().unwrap().bytes()) as u64,
                    recording: rec,
                    handcam_on: self.s.cam.lock().unwrap().is_some(),
                    handcam_error: self.s.cam_error.lock().unwrap().0.clone(),
                };
                drop(run);
                self.publish(st);
            }
        }
    }

    fn desired_source(&self, cfg: &Capture, running: Option<&Running>) -> Option<(Source, String, Option<(String, String)>)> {
        let monitor = || {
            let mons = game::monitors();
            let m = mons.iter().find(|m| m.id == cfg.monitor).or_else(|| mons.first())?;
            Some((Source::Monitor(m.handle), m.label.clone(), None))
        };
        if cfg.target == "monitor" {
            return monitor();
        }
        // Keep capturing the current game while its window exists (even when alt-tabbed).
        if let Some(r) = running {
            if let (Source::Window(h), Some((exe, name))) = (&r.source, &r.game) {
                if game::alive(*h) && game_allowed(cfg, exe, true) {
                    return Some((r.source.clone(), name.clone(), r.game.clone()));
                }
            }
        }
        if let Some(w) = game::foreground() {
            let exe = w.exe.to_ascii_lowercase();
            let known = cfg.games.iter().any(|g| g.exe == exe);
            if game::looks_like_game(&w) && !known {
                (self.s.events)(Event::GameDetected(GameRule {
                    exe: exe.clone(),
                    name: game::display_name(&w.title, &w.exe),
                    enabled: cfg.games_mode != "selected",
                }));
            }
            if (game::looks_like_game(&w) || known) && game_allowed(cfg, &exe, game::looks_like_game(&w)) {
                let name = cfg.games.iter().find(|g| g.exe == exe).map(|g| g.name.clone()).unwrap_or_else(|| game::display_name(&w.title, &w.exe));
                return Some((Source::Window(w.hwnd), name.clone(), Some((exe, name))));
            }
        }
        // Recording asked for with no game around: record the screen instead.
        if self.s.want_record.load(Ordering::SeqCst) || self.is_recording() {
            return monitor();
        }
        None
    }

    fn start_running(&self, ffmpeg: &Path, cfg: &Capture, source: Source, label: String, game: Option<(String, String)>, key: String) -> Result<Running, String> {
        let available = self.encoders();
        let enc = encoder::pick(&available, &cfg.codec, &cfg.encoder).ok_or("No working video encoder was found (is your GPU driver up to date?)")?;
        let (w, h) = match &source {
            Source::Window(hwnd) => game::client_size(*hwnd),
            Source::Monitor(m) => game::monitors().iter().find(|x| x.handle == *m).map(|x| (x.width as u32, x.height as u32)).unwrap_or((1920, 1080)),
        };
        if w < 64 || h < 64 {
            return Err("The game window is too small (minimized?)".into());
        }
        let plan = encoder::plan(cfg, w, h, &enc);
        // VALORANT kill clips: the capture also sends the killfeed corner, 4 frames a
        // second, to a pipe we read. Nothing else is read — no screen, no game.
        let watch_kills = cfg.enabled
            && (cfg.auto_clip_kills || cfg.count_kills)
            && match (&source, &game) {
                (Source::Window(_), Some((exe, _))) => exe == killfeed::VALORANT_EXE,
                (Source::Monitor(_), _) => true,
                _ => false,
            };
        let tap_pipe = if watch_kills { mux::Pipe::new_inbound().ok() } else { None };
        let args = encoder::args(&source, &plan, cfg.cursor, tap_pipe.as_ref().map(|p| p.name.as_str()));
        #[cfg(debug_assertions)]
        println!("[capture] ffmpeg {}", args.join(" "));

        let mut cmd = Command::new(ffmpeg);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(encoder::CREATE_NO_WINDOW);
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
        self.s.job.add(&child);

        *self.s.ring.lock().unwrap() = ts::Ring::new(10_000_000 / plan.fps as i64);
        // While watching for kills the buffer must hold a whole highlight until the
        // round is known to be over: the clip (≤ the size limit) + the quiet gap.
        let kill_keep = {
            let clip = (cfg.auto_max_mb as f64 * 8_000.0 / plan.kbps.max(1000) as f64).min(cfg.kill_before_secs as f64 + killfeed::MAX_ROUND_SECS + cfg.kill_after_secs as f64);
            (clip + killfeed::ROUND_GAP_SECS + 5.0).ceil() as u32
        };
        self.s.kill_keep.store(if tap_pipe.is_some() { kill_keep } else { 0 }, Ordering::SeqCst);
        let keep = self.s.keep_secs.load(Ordering::SeqCst).max(self.s.kill_keep.load(Ordering::SeqCst)) as f64;
        *self.s.desktop.lock().unwrap() = PcmRing::new(keep + 5.0);
        *self.s.mic.lock().unwrap() = PcmRing::new(keep + 5.0);

        let mut stdout = child.stdout.take().unwrap();
        let shared = self.s.clone();
        let max_kbps = plan.max_kbps;
        let reader = std::thread::Builder::new()
            .name("capture-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; 256 * 1024];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let keep = shared.keep_secs.load(Ordering::SeqCst).max(shared.kill_keep.load(Ordering::SeqCst)) as f64;
                    // When only recording (replay off), keep just a few seconds.
                    let keep = if shared.cfg.lock().unwrap().enabled { keep } else { 3.0 };
                    let max_bytes = (encoder::buffer_bytes(max_kbps, keep as u32 + 2) as f64 * 1.2) as usize;
                    let mut ring = shared.ring.lock().unwrap();
                    ring.feed(&buf[..n], now_hns());
                    ring.trim(keep, max_bytes);
                }
            })
            .map_err(|e| e.to_string())?;
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let mut stderr = child.stderr.take().unwrap();
        let sb = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut s = sb.lock().unwrap();
                s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if s.len() > 8192 {
                    let cut = s.len() - 4096;
                    let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                    s.drain(..cut);
                }
            }
        });

        let audio_stop = Arc::new(AtomicBool::new(false));
        let mut audio_threads = Vec::new();
        if cfg.desktop_audio {
            audio_threads.push(audio::spawn(audio::Source::Desktop, cfg.desktop_device.clone(), self.s.desktop.clone(), audio_stop.clone()));
        }
        if cfg.mic {
            audio_threads.push(audio::spawn(audio::Source::Mic, cfg.mic_device.clone(), self.s.mic.clone(), audio_stop.clone()));
        }
        let tap = tap_pipe.map(|pipe| {
            let name = pipe.name.clone();
            let engine = self.clone();
            let src = source.clone();
            let t = std::thread::Builder::new()
                .name("capture-killfeed".into())
                .spawn(move || {
                    let Ok(mut tap) = pipe.connect_reader() else { return };
                    engine.watch_kills(&mut |frame: &mut Vec<u8>| tap.read_exact(frame).is_ok(), src)
                })
                .expect("killfeed thread");
            (name, t)
        });
        // The classic recorder has its own buffer; the native one's is stale after it.
        self.s.pring.lock().unwrap().clear();
        self.s.audio_keep.store(0, Ordering::SeqCst);
        let size = if plan.width > 0 { (plan.width, plan.height) } else { (w, h) };
        Ok(Running { source, game, label, plan, key, backend: Backend::Classic { child, reader: Some(reader), stderr: stderr_buf, tap }, audio_stop, audio_threads, size, started: Instant::now() })
    }

    // ---------- the native engine ----------

    /// Whether to record with Clipit's own engine now (AV1 still uses the classic recorder).
    fn use_native(&self, cfg: &Capture) -> bool {
        cfg.engine == "native" && cfg.codec != "av1" && self.s.native_failures.load(Ordering::SeqCst) < NATIVE_MAX_FAILURES
    }

    fn native_failed(&self, why: &str) {
        let n = self.s.native_failures.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("[native] failure {n}: {why}");
        if n == NATIVE_MAX_FAILURES {
            (self.s.events)(Event::Warning(format!(
                "Clipit's new recorder didn't work on this PC ({why}). It switched to the classic recorder for now; it tries the new one again next time Clipit starts."
            )));
        }
    }

    fn start_native(&self, cfg: &Capture, source: Source, label: String, game: Option<(String, String)>, key: String) -> Result<Running, String> {
        let hevc = cfg.codec == "hevc";
        // The ffmpeg encoder name is still used for the hand cam and exports.
        let enc = encoder::pick(&self.encoders(), &cfg.codec, &cfg.encoder).unwrap_or_else(|| if hevc { "hevc_nvenc".into() } else { "h264_nvenc".into() });
        let (w, h) = match &source {
            Source::Window(hwnd) => game::client_size(*hwnd),
            Source::Monitor(m) => game::monitors().iter().find(|x| x.handle == *m).map(|x| (x.width as u32, x.height as u32)).unwrap_or((1920, 1080)),
        };
        if w < 64 || h < 64 {
            return Err("The game window is too small (minimized?)".into());
        }
        let plan = encoder::plan(cfg, w, h, &enc);
        let (ow, oh) = if plan.width > 0 { (plan.width, plan.height) } else { (w & !1, h & !1) };
        let watch_kills = cfg.enabled
            && (cfg.auto_clip_kills || cfg.count_kills)
            && match (&source, &game) {
                (Source::Window(_), Some((exe, _))) => exe == killfeed::VALORANT_EXE,
                (Source::Monitor(_), _) => true,
                _ => false,
            };
        let kill_keep = {
            let clip = (cfg.auto_max_mb as f64 * 8_000.0 / plan.kbps.max(1000) as f64).min(cfg.kill_before_secs as f64 + killfeed::MAX_ROUND_SECS + cfg.kill_after_secs as f64);
            (clip + killfeed::ROUND_GAP_SECS + 5.0).ceil() as u32
        };
        self.s.kill_keep.store(if watch_kills { kill_keep } else { 0 }, Ordering::SeqCst);
        let keep = self.s.keep_secs.load(Ordering::SeqCst).max(self.s.kill_keep.load(Ordering::SeqCst));
        self.s.pkeep.store(keep + 2, Ordering::SeqCst);
        let max_bytes = (encoder::buffer_bytes(plan.max_kbps, keep + 4) as f64 * 1.2) as usize;
        {
            // Same format and recent: keep what's buffered (a restart doesn't lose the replay).
            let mut ring = self.s.pring.lock().unwrap();
            ring.set_format(packets::Format { codec: hevc as u32, width: ow, height: oh, fps: plan.fps });
            if ring.newest_hns().map_or(true, |t| now_hns() - t > 50_000_000) {
                ring.clear();
            }
        }
        // Audio buffers: kept across restarts unless their length changes.
        if self.s.audio_keep.swap(keep, Ordering::SeqCst) != keep {
            *self.s.desktop.lock().unwrap() = PcmRing::new(keep as f64 + 5.0);
            *self.s.mic.lock().unwrap() = PcmRing::new(keep as f64 + 5.0);
        }
        let (tap_tx, tap_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let (hwnd, hmon) = match &source {
            Source::Window(h) => (*h, 0),
            Source::Monitor(m) => (0, *m),
        };
        let tap_every = (plan.fps / 4).max(1);
        let engine = native::Engine::start(
            native::Options {
                hwnd,
                hmonitor: hmon,
                width: ow,
                height: oh,
                fps: plan.fps,
                kbps: plan.kbps,
                max_kbps: plan.max_kbps,
                hevc,
                cursor: cfg.cursor,
                tap: watch_kills.then(|| (encoder::TAP_W as u32, encoder::TAP_H as u32, tap_every, tap_tx)),
            },
            self.s.pring.clone(),
            self.s.pkeep.clone(),
            max_bytes,
            self.s.rec_tee.clone(),
        )?;
        #[cfg(debug_assertions)]
        println!("[native] {} {}x{} @ {} fps, {} kbps", engine.stats().encoder, ow, oh, plan.fps, plan.kbps);
        let audio_stop = Arc::new(AtomicBool::new(false));
        let mut audio_threads = Vec::new();
        if cfg.desktop_audio {
            audio_threads.push(audio::spawn(audio::Source::Desktop, cfg.desktop_device.clone(), self.s.desktop.clone(), audio_stop.clone()));
        }
        if cfg.mic {
            audio_threads.push(audio::spawn(audio::Source::Mic, cfg.mic_device.clone(), self.s.mic.clone(), audio_stop.clone()));
        }
        let tap = watch_kills.then(|| {
            let me = self.clone();
            let src = source.clone();
            std::thread::Builder::new()
                .name("capture-killfeed".into())
                .spawn(move || {
                    me.watch_kills(
                        &mut |frame: &mut Vec<u8>| match tap_rx.recv() {
                            Ok(f) if f.len() == frame.len() => {
                                *frame = f;
                                true
                            }
                            Ok(_) => true,
                            Err(_) => false,
                        },
                        src,
                    )
                })
                .expect("killfeed thread")
        });
        Ok(Running { source, game, label, plan, key, backend: Backend::Native { engine, hevc, tap }, audio_stop, audio_threads, size: (ow, oh), started: Instant::now() })
    }

    // The newest moment in whichever buffer is in use.
    fn buffer_newest(&self) -> Option<i64> {
        if self.s.native_on.load(Ordering::SeqCst) {
            self.s.pring.lock().unwrap().newest_hns()
        } else {
            self.s.ring.lock().unwrap().newest_hns()
        }
    }

    fn buffer_since(&self, secs: f64, floor: Option<i64>) -> Option<Snap> {
        if self.s.native_on.load(Ordering::SeqCst) {
            self.s.pring.lock().unwrap().snapshot_since(secs, floor).map(Snap::Native)
        } else {
            self.s.ring.lock().unwrap().snapshot_since(secs, floor).map(Snap::Ts)
        }
    }

    fn buffer_range(&self, start: i64, end: i64) -> Option<Snap> {
        if self.s.native_on.load(Ordering::SeqCst) {
            self.s.pring.lock().unwrap().snapshot_range(start, end).map(Snap::Native)
        } else {
            self.s.ring.lock().unwrap().snapshot_range(start, end).map(Snap::Ts)
        }
    }

    fn set_status(&self, state: &str, message: &str, rec: Option<RecordingStatus>) {
        let handcam_error = self.s.cam_error.lock().unwrap().0.clone();
        self.publish(Status { state: state.into(), message: message.into(), recording: rec, handcam_error, ..Default::default() });
    }

    fn publish(&self, st: Status) {
        let mut cur = self.s.status.lock().unwrap();
        let changed = cur.state != st.state || cur.message != st.message || cur.recording.is_some() != st.recording.is_some() || cur.handcam_on != st.handcam_on || cur.handcam_error != st.handcam_error;
        *cur = st.clone();
        drop(cur);
        if changed {
            (self.s.events)(Event::Status(st));
        }
    }

    fn audio_opts(cfg: &Capture) -> mux::AudioOpts {
        mux::AudioOpts {
            desktop: cfg.desktop_audio,
            mic: cfg.mic,
            desktop_volume: cfg.desktop_volume,
            mic_volume: cfg.mic_volume,
            noise_suppression: cfg.noise_suppression,
            separate_tracks: cfg.separate_tracks,
        }
    }

    /// Folder (game) name for a new clip.
    fn current_game_name(&self) -> String {
        if let Some((_, name)) = self.s.run.lock().unwrap().as_ref().and_then(|r| r.game.clone()) {
            return name;
        }
        match game::foreground() {
            Some(w) if game::looks_like_game(&w) => {
                let exe = w.exe.to_ascii_lowercase();
                let cfg = self.s.cfg.lock().unwrap();
                cfg.games.iter().find(|g| g.exe == exe).map(|g| g.name.clone()).unwrap_or_else(|| game::display_name(&w.title, &w.exe))
            }
            _ => "Desktop".into(),
        }
    }

    // ---------- saving a replay ----------

    /// Saves the last N seconds. Returns right away; the result comes as an event.
    pub fn save_replay(&self) -> Result<(), String> {
        // The moment of the key press, before anything that could wait on a lock.
        let pressed = now_hns();
        let cfg = self.s.cfg.lock().unwrap().clone();
        if self.s.run.lock().unwrap().is_none() {
            return Err(if !cfg.enabled {
                "Instant replay is off."
            } else if cfg.target == "game" {
                "No game is being captured right now."
            } else {
                "Capture hasn't started yet."
            }
            .into());
        }
        if !cfg.enabled && !self.is_recording() {
            return Err("Instant replay is off.".into());
        }
        if self.buffer_newest().is_none() {
            return Err("Nothing recorded yet — try again in a second.".into());
        }
        let me = self.clone();
        std::thread::spawn(move || {
            // The buffer can run behind the game when the PC can't encode fast
            // enough. Wait (a little) for the moment the key was pressed to arrive,
            // so the clip always ends where you pressed.
            let deadline = Instant::now() + Duration::from_millis(MAX_CATCH_UP_MS);
            let behind = loop {
                let newest = me.buffer_newest().unwrap_or(0);
                let behind = pressed - newest;
                if behind <= CAUGHT_UP_HNS || Instant::now() >= deadline {
                    break behind;
                }
                std::thread::sleep(Duration::from_millis(40));
            };
            #[cfg(debug_assertions)]
            println!("[clip] waited {:?}, buffer {:.0} ms behind the key", Duration::from_millis(MAX_CATCH_UP_MS).saturating_sub(deadline.saturating_duration_since(Instant::now())), behind as f64 / 1e4);
            if behind > CAUGHT_UP_HNS && !me.s.warned_lag.swap(true, Ordering::SeqCst) {
                (me.s.events)(Event::Warning(format!(
                    "Your PC is recording about {:.0} s behind the game, so clips end a bit early. Lower the quality, frame rate or resolution in Settings → Recording.",
                    behind as f64 / 1e7
                )));
            }
            let snap = {
                let mut last = me.s.last_clip_end.lock().unwrap();
                // Start where the previous clip ended (but keep at least MIN_CLIP_SECS).
                let floor = match (cfg.no_repeat, *last, me.buffer_newest()) {
                    (true, Some(end), Some(newest)) => Some(end.min(newest - (MIN_CLIP_SECS * 1e7) as i64)),
                    _ => None,
                };
                let snap = me.buffer_since(cfg.replay_secs as f64, floor);
                if let Some(sn) = &snap {
                    *last = Some(sn.end_hns());
                }
                snap
            };
            let res = match snap {
                Some(snap) => me.save_snapshot(snap, None),
                None => Err("Nothing recorded yet — try again in a second.".into()),
            };
            if let Err(e) = res {
                (me.s.events)(Event::ClipFailed(e));
            }
        });
        Ok(())
    }

    /// Saves a round of kills: from `start_hns` to `end_hns` on the capture clock.
    fn save_round(&self, start_hns: i64, end_hns: i64, kills: u32) -> Result<(), String> {
        let snap = self.buffer_range(start_hns, end_hns).ok_or("Nothing recorded yet.")?;
        self.save_snapshot(snap, Some(kills))
    }

    fn save_snapshot(&self, snap: Snap, kills: Option<u32>) -> Result<(), String> {
        let cfg = self.s.cfg.lock().unwrap().clone();
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        let video = self.s.run.lock().unwrap().as_ref().map(|r| VideoInfo::of(r)).ok_or("Capture isn't running.")?;
        let fps = video.fps;
        let (start, end) = (snap.start_hns() + AUDIO_SHIFT_HNS, snap.end_hns() + AUDIO_SHIFT_HNS);
        let opts = Self::audio_opts(&cfg);
        let d = opts.desktop.then(|| self.s.desktop.lock().unwrap().read(start, end));
        let m = opts.mic.then(|| self.s.mic.lock().unwrap().read(start, end));
        let name = self.current_game_name();
        let root = self.s.clips_root.lock().unwrap().clone();
        let out = clip_path(Path::new(&root), &name, ".mp4")?;
        let analyze = killfeed::is_valorant(&name) && (cfg.count_kills || kills.is_some());
        // Hand cam: the camera for the same time, kept aside until the clip is written.
        let game_start = snap.start_hns();
        let cam = if self.cam_live() {
            let file = self.s.data_dir.join("handcam").join(format!("cam-{}.ts", unique_id()));
            self.cam_for_range(snap.start_hns(), snap.end_hns(), &file).map(|t| (file, t))
        } else {
            None
        };
        let secs = (snap.end_hns() - snap.start_hns()) as f64 / 1e7;
        let kill_times = self.live_kill_times(snap.start_hns(), snap.end_hns());
        let shared = self.s.clone();
        std::thread::spawn(move || {
            // The clip is saved right away, as without a hand cam.
            #[cfg(debug_assertions)]
            let t0 = Instant::now();
            let res = match &snap {
                Snap::Ts(s) => mux::save_clip(&ffmpeg, s, mux::interleave(d.as_deref(), m.as_deref()), &opts, fps, &out, &|c| shared.job.add(c)),
                Snap::Native(s) => save_native(s, d, m, &opts, &out),
            };
            #[cfg(debug_assertions)]
            println!("[clip] wrote the clip in {:?}", t0.elapsed());
            drop(snap);
            let saved = res.is_ok();
            // Marked before "saved" goes out, so the clip never shows up as ready
            // while it's about to be replaced or read.
            let cam_busy = (saved && cam.is_some()).then(|| Busy::new(&shared, &out, "handcam"));
            let read_busy = (saved && analyze).then(|| Busy::new(&shared, &out, "reading"));
            (shared.events)(match res {
                Ok(size) => Event::ClipSaved { path: out.clone(), size, kills, kill_times: kill_times.clone() },
                Err(e) => Event::ClipFailed(e),
            });
            // Kills, map and agent are read from the clip before the camera is on it,
            // while the camera is added next to it.
            let analysis = (saved && analyze).then(|| {
                let (ffmpeg, out, shared) = (ffmpeg.clone(), out.clone(), shared.clone());
                let times = kill_times.clone();
                std::thread::spawn(move || valorant::analyze_clip(&ffmpeg, &out, &shared.data_dir.join("valorant"), kills, times, &|c| shared.job.add(c)))
            });
            let with_cam = match (&cam, saved) {
                (Some((file, cam_start)), true) => add_handcam(&shared, &ffmpeg, &cfg.handcam, &video, &out, file, cam_start - game_start, secs),
                _ => None,
            };
            if let Some(t) = analysis {
                if let Ok(mut info) = t.join() {
                    info.auto = false;
                    (shared.events)(Event::ClipInfo { path: out.clone(), info });
                }
            }
            drop(read_busy);
            if let Some(part) = with_cam {
                swap_in(&shared, &part, &out);
            }
            drop(cam_busy);
            if let Some((file, _)) = &cam {
                let _ = std::fs::remove_file(file);
            }
        });
        Ok(())
    }

    /// Your kills between two clock times, as seconds from `start` — only when the
    /// killfeed was watched the whole time (else the clip is read after saving).
    fn live_kill_times(&self, start: i64, end: i64) -> Option<Vec<f32>> {
        let since = (*self.s.kill_watch_since.lock().unwrap())?;
        // Watching starts with the capture: allow its first second.
        if since > start + 10_000_000 {
            return None;
        }
        let log = self.s.kill_log.lock().unwrap();
        Some(log.iter().filter(|&&t| t >= start && t <= end).map(|&t| (t - start) as f32 / 1e7).collect())
    }

    // ---------- watching for kills (VALORANT) ----------

    /// Reads the killfeed tap (4 frames a second of the killfeed corner, from our own
    /// capture) and saves one clip per round: from `kill_before_secs` before your
    /// first kill to `kill_after_secs` after your last. A round is over once no kill
    /// has happened for ROUND_GAP_SECS — longer than a round's end plus buy phase can
    /// be, so an ace stays in one clip, and two rounds never merge.
    /// `next_frame` fills in the next tap frame; false = the capture stopped.
    fn watch_kills(&self, next_frame: &mut dyn FnMut(&mut Vec<u8>) -> bool, source: Source) {
        let mut frame = vec![0u8; encoder::TAP_W * encoder::TAP_H * 3];
        let mut tracker = killfeed::Tracker::new();
        let mut round = killfeed::Round::default();
        // Foreground window and whether it's VALORANT (looked up once per window).
        let mut front: (isize, bool) = (0, false);
        while next_frame(&mut frame) {
            let (on, auto, rules) = {
                let c = self.s.cfg.lock().unwrap();
                (c.enabled && (c.auto_clip_kills || c.count_kills), c.enabled && c.auto_clip_kills, Self::kill_rules(&c))
            };
            let now = Instant::now();
            let now_hns = now_hns();
            if let Some(end) = round.finished(now, &rules) {
                self.save_highlights(end);
            }
            if !on {
                tracker.reset();
                *self.s.kill_watch_since.lock().unwrap() = None;
                continue;
            }
            // Whole-monitor capture: only while VALORANT is in front on that monitor.
            if let Source::Monitor(m) = source {
                let hwnd = game::foreground_hwnd();
                if hwnd != front.0 {
                    let exe = game::describe(hwnd).map(|w| w.exe.to_ascii_lowercase()).unwrap_or_default();
                    front = (hwnd, exe == killfeed::VALORANT_EXE);
                }
                if !front.1 || game::monitor_of(hwnd) != m {
                    tracker.reset();
                    *self.s.kill_watch_since.lock().unwrap() = None;
                    continue;
                }
            }
            self.s.kill_watch_since.lock().unwrap().get_or_insert(now_hns);
            let img = killfeed::Image { w: encoder::TAP_W, h: encoder::TAP_H, stride: encoder::TAP_W * 3, bgr: &frame };
            let rows = killfeed::own_kills(&img, 1.0);
            let new = tracker.feed(&rows, 32, now) as u32;
            if new > 0 {
                #[cfg(debug_assertions)]
                println!("[killfeed] +{new} kill(s), yours in the feed at {rows:?}");
                // An entry is counted about 0.8 s after the kill (it fades in).
                let mut log = self.s.kill_log.lock().unwrap();
                log.extend(std::iter::repeat(now_hns - KILL_SEEN_LATE_HNS).take(new as usize));
                log.retain(|t| now_hns - t < KILL_LOG_KEEP_HNS);
                drop(log);
                if auto {
                    round.kill(new, now, now_hns);
                }
            }
        }
        *self.s.kill_watch_since.lock().unwrap() = None;
        // Capture stopped (game closed, settings changed): keep a round in progress.
        let rules = Self::kill_rules(&self.s.cfg.lock().unwrap());
        if let Some(end) = round.take(&rules) {
            self.save_highlights(end);
        }
    }

    fn kill_rules(c: &Capture) -> killfeed::Rules {
        killfeed::Rules {
            min_kills: c.auto_min_kills.clamp(1, 5),
            max_gap_secs: c.auto_max_gap_secs as f64,
            whole_round_from: c.auto_whole_round_from,
            skip_deathmatch: c.auto_skip_deathmatch,
        }
    }

    /// Longest auto clip in seconds: the size limit at the current bitrate.
    fn auto_max_secs(&self, c: &Capture) -> f64 {
        let kbps = self.s.run.lock().unwrap().as_ref().map(|r| r.plan.kbps).unwrap_or(20_000).max(1000);
        (c.auto_max_mb as f64 * 8_000.0 / kbps as f64).max(10.0)
    }

    fn save_highlights(&self, end: killfeed::RoundEnd) {
        let killfeed::RoundEnd::Clips(clips) = end else {
            #[cfg(debug_assertions)]
            println!("[killfeed] Deathmatch-style play: not clipping");
            return;
        };
        let c = self.s.cfg.lock().unwrap().clone();
        let max = (self.auto_max_secs(&c) * 1e7) as i64;
        for (first, last, kills) in clips {
            let end = last + c.kill_after_secs as i64 * 10_000_000;
            // Over the size limit: keep the end (the last kills) and drop the start.
            let start = (first - c.kill_before_secs as i64 * 10_000_000).max(end - max);
            if let Err(e) = self.save_round(start, end, kills) {
                (self.s.events)(Event::ClipFailed(e));
            }
        }
    }

    // ---------- counting kills ----------

    /// Kills, map and agent in any VALORANT clip (reads the file, not the screen).
    /// Clips still being worked on after saving: (path, "handcam" | "reading").
    pub fn busy_clips(&self) -> Vec<(String, &'static str)> {
        let list = self.s.busy.lock().unwrap();
        let mut out: Vec<(String, &'static str)> = Vec::new();
        for (p, _) in list.iter() {
            let key = p.to_string_lossy().into_owned();
            if !out.iter().any(|(k, _)| *k == key) {
                out.push((key, busy_task(&list, p).unwrap_or("reading")));
            }
        }
        out
    }

    pub fn analyze(&self, clip: &Path) -> Result<valorant::ClipInfo, String> {
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        let info = valorant::analyze_clip(&ffmpeg, clip, &self.s.data_dir.join("valorant"), None, None, &|c| self.s.job.add(c));
        if info.kill_times.is_none() {
            return Err("Couldn't read the clip".into());
        }
        Ok(info)
    }

    /// Only when your kills happen (for the timeline): faster than `analyze`.
    pub fn kill_times(&self, clip: &Path) -> Result<Vec<f32>, String> {
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        killfeed::kill_times_in_clip(&ffmpeg, clip, &|c| self.s.job.add(c))
    }

    // ---------- recording ----------

    /// Starts or stops a recording. Returns true when a recording was started.
    pub fn toggle_recording(&self) -> Result<bool, String> {
        if self.is_recording() {
            self.stop_recording();
            Ok(false)
        } else {
            self.start_recording()?;
            Ok(true)
        }
    }

    pub fn start_recording(&self) -> Result<(), String> {
        let cfg = self.s.cfg.lock().unwrap().clone();
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        let root = self.s.clips_root.lock().unwrap().clone();
        if let Some(free) = game::free_space(Path::new(&root)) {
            if free < 2_000_000_000 {
                return Err("Less than 2 GB free on the clips drive.".into());
            }
        }
        // Make sure the capture is running (it starts on the screen if no game is open).
        self.s.want_record.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        let ready = loop {
            let ok = match self.s.run.lock().unwrap().as_ref() {
                Some(r) if r.is_native() => self.s.pring.lock().unwrap().newest_hns().is_some(),
                Some(_) => self.s.ring.lock().unwrap().header().is_some(),
                None => false,
            };
            if ok {
                break true;
            }
            if t0.elapsed() > Duration::from_secs(8) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if !ready {
            self.s.want_record.store(false, Ordering::SeqCst);
            return Err("Capture didn't start, so recording couldn't begin.".into());
        }
        let name = self.current_game_name();
        let out = clip_path(Path::new(&root), &name, ".mp4")?;
        let opts = Self::audio_opts(&cfg);
        let shared = self.s.clone();
        let cam = if self.cam_live() { self.start_cam_recording() } else { None };
        if self.s.native_on.load(Ordering::SeqCst) {
            return self.start_native_recording(&cfg, out, cam);
        }
        let mut live = mux::start_live(&ffmpeg, &opts, &out, &|c| shared.job.add(c))?;
        let header = self.s.ring.lock().unwrap().header().unwrap_or_default();

        let start_hns = Arc::new(AtomicI64::new(0));
        let end_hns = Arc::new(AtomicI64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));

        // Video: tee packets from the capture into the muxer, starting at a keyframe.
        let (tx, rx) = sync_channel::<Vec<u8>>(64);
        {
            let mut ring = self.s.ring.lock().unwrap();
            let offset = ring.offset_hns().unwrap_or(0);
            let frame = ring.frame_hns;
            let mut tee = Tee { tx, batch: Vec::with_capacity(64 * ts::PACKET), failed: failed.clone() };
            let (start_hns, end_hns) = (start_hns.clone(), end_hns.clone());
            let mut started = false;
            ring.tee = Some(Box::new(move |p: &[u8], keyframe: bool, pts: Option<i64>| {
                if !started {
                    if !keyframe {
                        return;
                    }
                    started = true;
                    start_hns.store(ts::pts_to_hns(pts.unwrap_or(0)) + offset, Ordering::SeqCst);
                }
                if let Some(pts) = pts {
                    end_hns.store(ts::pts_to_hns(pts) + offset + frame, Ordering::SeqCst);
                }
                tee.push(p);
            }));
        }
        let mut stdin = live.stdin.take().unwrap();
        let video = std::thread::spawn(move || {
            let _ = stdin.write_all(&header);
            for batch in rx {
                if stdin.write_all(&batch).is_err() {
                    break;
                }
            }
        });

        // Audio: stream PCM in step with the clock, ~300 ms behind real time.
        let pipe_name = live.pipe.as_ref().map(|p| p.name.clone());
        let audio = live.pipe.take().map(|pipe| {
            let (desktop, mic) = (self.s.desktop.clone(), self.s.mic.clone());
            let (start_hns, end_hns, stop) = (start_hns.clone(), end_hns.clone(), stop.clone());
            let opts = opts.clone();
            std::thread::spawn(move || {
                let Ok(mut w) = pipe.connect() else { return };
                let mut cursor: Option<i64> = None;
                loop {
                    let stopping = stop.load(Ordering::SeqCst);
                    let start = start_hns.load(Ordering::SeqCst);
                    if start > 0 {
                        let from = *cursor.get_or_insert(audio::frame_of(start + AUDIO_SHIFT_HNS));
                        let to = if stopping {
                            audio::frame_of(end_hns.load(Ordering::SeqCst) + AUDIO_SHIFT_HNS)
                        } else {
                            audio::frame_of(now_hns() - 3_000_000)
                        };
                        if to > from {
                            let d = opts.desktop.then(|| desktop.lock().unwrap().read_frames(from, to));
                            let m = opts.mic.then(|| mic.lock().unwrap().read_frames(from, to));
                            if w.write_all(&mux::interleave(d.as_deref(), m.as_deref())).is_err() {
                                return;
                            }
                            cursor = Some(to);
                        }
                    }
                    if stopping {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
        });

        *self.s.recording.lock().unwrap() = Some(Recording { path: out.clone(), start_hns, stop, video: Some(video), audio, pipe_name, mux: Some(live), failed, cam, native: None });
        if cfg.sounds {
            sound::play(sound::Cue::RecordStart);
        }
        (self.s.events)(Event::RecordingStarted(out));
        Ok(())
    }

    /// A recording on the native engine: it continues the replay buffer from its
    /// newest keyframe (so it starts at once) and goes straight into the MP4.
    fn start_native_recording(&self, cfg: &Capture, out: PathBuf, cam: Option<(PathBuf, Arc<AtomicI64>)>) -> Result<(), String> {
        let opts = Self::audio_opts(cfg);
        let titles = native_track_titles(&opts);
        let asc = native::AacStream::open(2, audio::RATE as u32, 192)?.asc;
        let metas = titles.iter().map(|t| mp4::AudioTrack { title: t.to_string(), rate: audio::RATE as u32, channels: 2, asc, frames: Vec::new(), kbps: 192 }).collect();
        let part = mux::part_path(&out);
        let (tx, rx) = std::sync::mpsc::channel::<packets::RecMsg>();
        let start_hns = Arc::new(AtomicI64::new(0));
        let mut writer = {
            let ring = self.s.pring.lock().unwrap();
            let w = mp4::LiveWriter::create(&part, ring.format, metas)?;
            let seed = ring.from_last_key();
            start_hns.store(seed.first().map(|p| p.t).unwrap_or_else(now_hns), Ordering::SeqCst);
            for p in seed {
                let _ = tx.send(packets::RecMsg::Video(p));
            }
            // From here on every new packet also goes to the recording.
            *self.s.rec_tee.lock().unwrap() = Some(tx.clone());
            w
        };
        let writer_thread = std::thread::Builder::new()
            .name("recording-writer".into())
            .spawn(move || {
                let mut err: Option<String> = None;
                for msg in rx {
                    let r = match msg {
                        packets::RecMsg::Video(p) => writer.video(&p),
                        packets::RecMsg::Audio(i, f) => writer.audio(i, &f),
                        packets::RecMsg::Stop => break,
                    };
                    if let Err(e) = r {
                        err.get_or_insert(e); // keep draining; the clip is reported as failed
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None => writer.finish(),
                }
            })
            .map_err(|e| e.to_string())?;
        // Audio: mixed and encoded in pieces, about 300 ms behind real time.
        let stop = Arc::new(AtomicBool::new(false));
        let end_hns = Arc::new(AtomicI64::new(0));
        let audio_thread = {
            let (desktop, mic) = (self.s.desktop.clone(), self.s.mic.clone());
            let (stop, end_hns, start_hns, tx) = (stop.clone(), end_hns.clone(), start_hns.clone(), tx.clone());
            std::thread::Builder::new()
                .name("recording-audio".into())
                .spawn(move || {
                    let mut enc: Vec<native::AacStream> = titles.iter().filter_map(|_| native::AacStream::open(2, audio::RATE as u32, 192).ok()).collect();
                    let mut clean = opts.noise_suppression.then(denoise::Stream::new);
                    let mut cursor = audio::frame_of(start_hns.load(Ordering::SeqCst) + AUDIO_SHIFT_HNS);
                    loop {
                        let stopping = stop.load(Ordering::SeqCst);
                        let to = if stopping {
                            audio::frame_of(end_hns.load(Ordering::SeqCst) + AUDIO_SHIFT_HNS)
                        } else {
                            // Whole 480-frame blocks (the noise filter's size).
                            let t = audio::frame_of(now_hns() - 3_000_000);
                            cursor + (t - cursor).max(0) / 480 * 480
                        };
                        if to > cursor {
                            let d = opts.desktop.then(|| desktop.lock().unwrap().read_frames(cursor, to));
                            let mut m = opts.mic.then(|| mic.lock().unwrap().read_frames(cursor, to));
                            if let (Some(m), Some(c)) = (m.as_mut(), clean.as_mut()) {
                                c.process(m);
                            }
                            let no_denoise = mux::AudioOpts { noise_suppression: false, ..opts.clone() };
                            for (i, (_, pcm)) in native_tracks(d, m, &no_denoise).into_iter().enumerate() {
                                if let Some(e) = enc.get_mut(i) {
                                    for f in e.push(&pcm).unwrap_or_default() {
                                        let _ = tx.send(packets::RecMsg::Audio(i, f));
                                    }
                                }
                            }
                            cursor = to;
                        }
                        if stopping {
                            for (i, e) in enc.iter_mut().enumerate() {
                                for f in e.finish().unwrap_or_default() {
                                    let _ = tx.send(packets::RecMsg::Audio(i, f));
                                }
                            }
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .map_err(|e| e.to_string())?
        };
        let native = NativeRec { part, tx, writer: writer_thread, audio: audio_thread, end_hns };
        *self.s.recording.lock().unwrap() = Some(Recording {
            path: out.clone(),
            start_hns,
            stop,
            video: None,
            audio: None,
            pipe_name: None,
            mux: None,
            failed: Arc::new(AtomicBool::new(false)),
            cam,
            native: Some(native),
        });
        if cfg.sounds {
            sound::play(sound::Cue::RecordStart);
        }
        (self.s.events)(Event::RecordingStarted(out));
        Ok(())
    }

    fn finish_native_recording(&self, n: NativeRec, out: &Path, stop: &AtomicBool) -> Result<u64, String> {
        // No more video; the audio ends where the video does.
        let end = {
            let ring = self.s.pring.lock().unwrap();
            *self.s.rec_tee.lock().unwrap() = None;
            ring.newest_hns().unwrap_or_else(now_hns)
        };
        n.end_hns.store(end, Ordering::SeqCst);
        stop.store(true, Ordering::SeqCst);
        let _ = n.audio.join();
        let _ = n.tx.send(packets::RecMsg::Stop);
        let size = n.writer.join().map_err(|_| "The recording writer crashed".to_string())??;
        mux::rename_retry(&n.part, out, Duration::from_secs(15)).map_err(|e| format!("Couldn't finish the recording: {e}"))?;
        Ok(size)
    }

    pub fn stop_recording(&self) {
        let Some(mut rec) = self.s.recording.lock().unwrap().take() else {
            self.s.want_record.store(false, Ordering::SeqCst);
            return;
        };
        self.s.want_record.store(false, Ordering::SeqCst);
        self.s.cam_ring.lock().unwrap().tee = None; // closes the hand cam file
        if let Some(n) = rec.native.take() {
            let result = self.finish_native_recording(n, &rec.path, &rec.stop);
            return self.recording_done(rec, result, false);
        }
        // Stop feeding video (dropping the tee closes the channel → writer exits).
        self.s.ring.lock().unwrap().tee = None;
        if let Some(v) = rec.video.take() {
            let _ = v.join();
        }
        rec.stop.store(true, Ordering::SeqCst);
        if let Some(a) = rec.audio.take() {
            // If ffmpeg died before opening the audio pipe, unblock the writer.
            let t = Instant::now();
            while !a.is_finished() && t.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !a.is_finished() {
                if let Some(name) = &rec.pipe_name {
                    mux::Pipe::poke(name);
                }
            }
            let _ = a.join();
        }
        let failed = rec.failed.load(Ordering::SeqCst);
        let result = rec.mux.take().map(|m| m.finish()).unwrap_or(Err("no muxer".into()));
        self.recording_done(rec, result, failed)
    }

    /// Hand cam, sound and events once a recording's file is finished.
    fn recording_done(&self, mut rec: Recording, result: Result<u64, String>, failed: bool) {
        let sounds = self.s.cfg.lock().unwrap().sounds;
        let path = rec.path.clone();
        // Hand cam: added in the background; the recording is usable right away.
        if let Some((file, cam_start)) = rec.cam.take() {
            let (cam_start, rec_start) = (cam_start.load(Ordering::SeqCst), rec.start_hns.load(Ordering::SeqCst));
            let video = self.s.run.lock().unwrap().as_ref().map(VideoInfo::of);
            match (&result, self.s.ffmpeg.clone(), video) {
                (Ok(_), Some(ffmpeg), Some(video)) if cam_start > 0 && rec_start > 0 => {
                    let hc = self.s.cfg.lock().unwrap().handcam.clone();
                    let (shared, path) = (self.s.clone(), path.clone());
                    let secs = (now_hns() - rec_start) as f64 / 1e7;
                    let busy = Busy::new(&shared, &path, "handcam");
                    std::thread::spawn(move || {
                        if let Some(part) = add_handcam(&shared, &ffmpeg, &hc, &video, &path, &file, cam_start - rec_start, secs) {
                            swap_in(&shared, &part, &path);
                        }
                        drop(busy);
                        let _ = std::fs::remove_file(&file);
                    });
                }
                _ => {
                    let _ = std::fs::remove_file(&file);
                }
            }
        }
        if sounds {
            sound::play(if result.is_ok() { sound::Cue::RecordStop } else { sound::Cue::Error });
        }
        (self.s.events)(match result {
            Ok(size) if !failed => Event::RecordingStopped { path, size },
            Ok(size) => {
                let _ = size;
                Event::RecordingFailed("The recording fell behind and may have gaps.".into())
            }
            Err(e) => Event::RecordingFailed(e),
        });
    }
}

/// Batches recording packets for the muxer; sends the remainder when dropped.
struct Tee {
    tx: SyncSender<Vec<u8>>,
    batch: Vec<u8>,
    failed: Arc<AtomicBool>,
}

impl Tee {
    fn push(&mut self, p: &[u8]) {
        self.batch.extend_from_slice(p);
        if self.batch.len() >= 64 * ts::PACKET {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let b = std::mem::replace(&mut self.batch, Vec::with_capacity(64 * ts::PACKET));
        if self.tx.try_send(b).is_err() {
            self.failed.store(true, Ordering::SeqCst); // the muxer fell behind
        }
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        if !self.batch.is_empty() {
            let b = std::mem::take(&mut self.batch);
            let _ = self.tx.send(b);
        }
    }
}

/// What the saved video looks like (for re-encoding it with the hand cam).
#[derive(Clone)]
struct VideoInfo {
    size: (u32, u32),
    encoder: String,
    kbps: u32,
    max_kbps: u32,
    fps: u32,
}

impl VideoInfo {
    fn of(r: &Running) -> Self {
        Self { size: r.size, encoder: r.plan.encoder.clone(), kbps: r.plan.kbps, max_kbps: r.plan.max_kbps, fps: r.plan.fps }
    }
}

fn unique_id() -> String {
    let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{ns}", std::process::id())
}

/// The clip before the hand cam is added: next to it, with an extension the
/// library ignores.
fn raw_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".clipit-raw");
    PathBuf::from(s)
}

/// The saved clip `clip` + camera -> a new file next to it (returned), at the
/// clip's own bitrate so it stays the same size. On failure the clip is simply
/// kept without the camera and a warning is shown. `cam_offset_hns` = camera
/// start minus clip start.
fn add_handcam(shared: &Shared, ffmpeg: &Path, hc: &crate::config::HandCam, v: &VideoInfo, clip: &Path, cam_file: &Path, cam_offset_hns: i64, secs: f64) -> Option<PathBuf> {
    let work = shared.data_dir.join("handcam");
    let part = raw_path(clip);
    // Same bitrate as the clip itself (the capture's target is a ceiling, and a
    // quiet clip would otherwise come out much bigger).
    let size = std::fs::metadata(clip).map(|m| m.len()).unwrap_or(0);
    let actual = if secs > 1.0 { (size as f64 * 8.0 / 1000.0 / secs * 0.97) as u32 } else { v.kbps };
    let kbps = actual.clamp(1_000, v.kbps.max(1_000));
    let c = handcam::Compose {
        ffmpeg,
        game: clip,
        camera: cam_file,
        camera_offset: cam_offset_hns as f64 / 1e7 + hc.delay_ms as f64 / 1000.0,
        cam: hc,
        size: v.size,
        encoder: &v.encoder,
        kbps,
        max_kbps: (kbps * 3 / 2).min(v.max_kbps.max(kbps)),
        fps: v.fps,
        work: &work,
        out: &part,
    };
    match handcam::compose(&c, &|ch| shared.job.add(ch)) {
        Ok(_) => Some(part),
        Err(e) => {
            (shared.events)(Event::Warning(format!("Saved without the hand cam. {e}")));
            None
        }
    }
}

/// Marks a clip as being worked on until dropped (also on failure).
struct Busy {
    shared: Arc<Shared>,
    path: PathBuf,
    task: &'static str,
}

impl Busy {
    fn new(shared: &Arc<Shared>, path: &Path, task: &'static str) -> Self {
        shared.busy.lock().unwrap().push((path.to_path_buf(), task));
        announce_busy(shared, path);
        Busy { shared: shared.clone(), path: path.to_path_buf(), task }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut b = self.shared.busy.lock().unwrap();
        if let Some(i) = b.iter().position(|(p, t)| *p == self.path && *t == self.task) {
            b.remove(i);
        }
        drop(b);
        announce_busy(&self.shared, &self.path);
    }
}

/// What a clip is waiting for: the hand cam matters most (the file will change).
fn busy_task(list: &[(PathBuf, &'static str)], path: &Path) -> Option<&'static str> {
    let tasks: Vec<_> = list.iter().filter(|(p, _)| p == path).map(|(_, t)| *t).collect();
    tasks.iter().find(|t| **t == "handcam").or(tasks.first()).copied()
}

fn announce_busy(shared: &Shared, path: &Path) {
    let task = busy_task(&shared.busy.lock().unwrap(), path);
    (shared.events)(Event::ClipBusy { path: path.to_path_buf(), task });
}

/// Puts the clip with the camera in place of the plain one. The player may have
/// the file open for a moment, so this retries for a few seconds.
fn swap_in(shared: &Shared, part: &Path, clip: &Path) {
    for _ in 0..40 {
        if std::fs::rename(part, clip).is_ok() {
            let size = std::fs::metadata(clip).map(|m| m.len()).unwrap_or(0);
            (shared.events)(Event::ClipUpdated { path: clip.to_path_buf(), size });
            return;
        }
        if !clip.exists() {
            break; // deleted or renamed meanwhile
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_file(part);
    (shared.events)(Event::Warning("Saved without the hand cam: the clip was in use.".into()));
}

fn stop_cam(mut c: CamRun) {
    let _ = c.child.kill();
    let _ = c.child.wait();
    if let Some(t) = c.reader.take() {
        let _ = t.join();
    }
}

fn stop_running(r: Running) {
    let Running { backend, audio_stop, audio_threads, .. } = r;
    audio_stop.store(true, Ordering::SeqCst);
    match backend {
        Backend::Classic { mut child, reader, tap, .. } => {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(t) = reader {
                let _ = t.join();
            }
            if let Some((name, t)) = tap {
                mux::Pipe::poke_writer(&name); // in case ffmpeg never opened it
                let _ = t.join();
            }
        }
        Backend::Native { engine, tap, .. } => {
            drop(engine); // stops capture; the tap channel closes with it
            if let Some(t) = tap {
                let _ = t.join();
            }
        }
    }
    for t in audio_threads {
        let _ = t.join();
    }
}

/// Writes a native-engine clip: MP4 straight from the buffer, audio encoded to
/// AAC alongside (one thread per track). No ffmpeg, no new process.
fn save_native(snap: &packets::Snapshot, desktop: Option<Vec<i16>>, mic: Option<Vec<i16>>, opts: &mux::AudioOpts, out: &Path) -> Result<u64, String> {
    let tracks = native_tracks(desktop, mic, opts);
    let jobs: Vec<_> = tracks
        .into_iter()
        .map(|(title, pcm)| {
            std::thread::spawn(move || {
                native::aac(&pcm, 2, audio::RATE as u32, 192).map(|a| mp4::AudioTrack { title: title.to_string(), rate: audio::RATE as u32, channels: 2, asc: a.asc, frames: a.frames, kbps: 192 })
            })
        })
        .collect();
    let mut audio_tracks = Vec::new();
    for j in jobs {
        audio_tracks.push(j.join().map_err(|_| "The audio encoder crashed".to_string())??);
    }
    let part = mux::part_path(out);
    let size = mp4::write(&part, snap, &audio_tracks).inspect_err(|_| {
        let _ = std::fs::remove_file(&part);
    })?;
    // Antivirus may hold a brand-new file for a moment.
    mux::rename_retry(&part, out, Duration::from_secs(15)).map_err(|e| format!("Couldn't finish the clip: {e}"))?;
    Ok(size)
}

fn native_track_titles(a: &mux::AudioOpts) -> Vec<&'static str> {
    match (a.desktop, a.mic) {
        (true, true) if a.separate_tracks => vec!["Game + mic", "Desktop", "Microphone"],
        (true, true) => vec!["Game + mic"],
        (true, false) => vec!["Desktop"],
        (false, true) => vec!["Microphone"],
        (false, false) => Vec::new(),
    }
}

/// The same tracks the classic recorder makes: "Game + mic" (mixed, limited),
/// plus "Desktop" and "Microphone" on their own when asked for.
fn native_tracks(desktop: Option<Vec<i16>>, mic: Option<Vec<i16>>, a: &mux::AudioOpts) -> Vec<(&'static str, Vec<i16>)> {
    let scale = |v: &mut Vec<i16>, vol: u32| {
        if vol != 100 {
            let g = vol as f32 / 100.0;
            v.iter_mut().for_each(|s| *s = (*s as f32 * g).clamp(-32768.0, 32767.0) as i16);
        }
    };
    let d = desktop.map(|mut v| {
        scale(&mut v, a.desktop_volume);
        v
    });
    let m = mic.map(|mut v| {
        if a.noise_suppression {
            denoise::stereo(&mut v);
        }
        scale(&mut v, a.mic_volume);
        v
    });
    match (d, m) {
        (Some(d), Some(m)) => {
            let n = d.len().min(m.len());
            let mix: Vec<i16> = (0..n).map(|i| limit(d[i] as f32 + m[i] as f32)).collect();
            if a.separate_tracks {
                vec![("Game + mic", mix), ("Desktop", d), ("Microphone", m)]
            } else {
                vec![("Game + mic", mix)]
            }
        }
        (Some(d), None) => vec![("Desktop", d)],
        (None, Some(m)) => vec![("Microphone", m)],
        (None, None) => Vec::new(),
    }
}

/// Soft limiter for the mix: untouched below 80 % of full scale, then eased
/// towards 97 % (like the classic recorder's alimiter, without the lookahead).
fn limit(x: f32) -> i16 {
    const FS: f32 = 32767.0;
    let a = x.abs() / FS;
    let y = if a <= 0.8 { a } else { 0.8 + 0.17 * ((a - 0.8) / 0.17).tanh() };
    (y * FS * x.signum()) as i16
}

fn game_allowed(cfg: &Capture, exe: &str, looks_like_game: bool) -> bool {
    match cfg.games.iter().find(|g| g.exe == exe) {
        Some(rule) => rule.enabled,
        None => cfg.games_mode != "selected" && looks_like_game,
    }
}

fn friendly_encoder(e: &str) -> String {
    let gpu = if e.ends_with("_nvenc") || e.ends_with("_amf") || e.ends_with("_qsv") { "Graphics card" } else { "CPU (software)" };
    let codec = if e.starts_with("hevc") || e == "libx265" { "HEVC" } else if e.starts_with("av1") { "AV1" } else { "H.264" };
    format!("{gpu} · {codec}")
}

/// `<root>/<Game>/<Game> 2026.11.11 - 14.03.22.07<suffix>`, reusing an existing
/// folder whose name matches regardless of case (e.g. an existing "Valorant").
pub fn clip_path(root: &Path, game_name: &str, suffix: &str) -> Result<PathBuf, String> {
    if root.as_os_str().is_empty() {
        return Err("Set a clips folder in Settings first.".into());
    }
    let wanted = game::sanitize(game_name);
    let wanted = if wanted.is_empty() { "Desktop".to_string() } else { wanted };
    let existing = std::fs::read_dir(root)
        .ok()
        .and_then(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.eq_ignore_ascii_case(&wanted)));
    let folder = existing.unwrap_or(wanted);
    let dir = root.join(&folder);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    #[cfg(windows)]
    let stamp = game::now_timestamp();
    #[cfg(not(windows))]
    let stamp = String::from("2026.01.01 - 00.00.00.00");
    let mut path = dir.join(format!("{folder} {stamp}{suffix}"));
    let mut n = 2;
    while path.exists() || mux::part_path(&path).exists() {
        path = dir.join(format!("{folder} {stamp} ({n}){suffix}"));
        n += 1;
    }
    Ok(path)
}

/// Renames recordings left as ".mp4.part" by a crash (they're playable fragmented MP4s).
pub fn recover_parts(root: &Path) {
    let Ok(games) = std::fs::read_dir(root) else { return };
    for g in games.flatten() {
        let Ok(files) = std::fs::read_dir(g.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            let name = p.to_string_lossy();
            if !name.ends_with(".mp4.part") {
                continue;
            }
            // A recording (fragmented MP4) plays up to where it stopped; a replay
            // that was fully written but couldn't be renamed (the file was busy)
            // is a whole clip. Only a half-written replay is useless.
            if is_fragmented(&p) || is_complete_mp4(&p) {
                let fixed = PathBuf::from(name.trim_end_matches(".part"));
                if !fixed.exists() {
                    let _ = std::fs::rename(&p, fixed);
                }
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// A finished (non-fragmented) MP4: its top-level boxes run exactly to the end
/// of the file and include the index (moov) and the media (mdat). ffmpeg writes
/// the moov last, so a half-written replay has none.
fn is_complete_mp4(p: &Path) -> bool {
    use std::io::Seek;
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
    let (mut pos, mut moov, mut mdat) = (0u64, false, false);
    while pos < len {
        let mut h = [0u8; 16];
        if f.seek(std::io::SeekFrom::Start(pos)).is_err() || f.read_exact(&mut h[..8]).is_err() {
            return false;
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        if size == 1 {
            if f.read_exact(&mut h[8..16]).is_err() {
                return false;
            }
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
        }
        if size < 8 {
            return false; // 0 = "to the end of the file": an unfinished mdat
        }
        match &h[4..8] {
            b"moov" => moov = true,
            b"mdat" => mdat = true,
            _ => {}
        }
        pos += size;
    }
    pos == len && moov && mdat
}

/// Recordings are fragmented MP4s (a "moof" box near the start); saved replays
/// aren't, and a half-written one can't be played.
fn is_fragmented(p: &Path) -> bool {
    let mut buf = vec![0u8; 256 * 1024];
    let n = std::fs::File::open(p).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    buf[..n].windows(4).any(|w| w == b"moof")
}

// ---------- job object: ffmpeg never outlives Clipit ----------

struct Job(isize);

impl Job {
    fn new() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::System::JobObjects::*;
            if let Ok(h) = CreateJobObjectW(None, None) {
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let _ = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                return Job(h.0 as isize);
            }
        }
        Job(0)
    }

    fn add(&self, child: &Child) {
        #[cfg(windows)]
        if self.0 != 0 {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Foundation::HANDLE;
            unsafe {
                let _ = windows::Win32::System::JobObjects::AssignProcessToJobObject(HANDLE(self.0 as *mut _), HANDLE(child.as_raw_handle() as *mut _));
            }
        }
        #[cfg(not(windows))]
        let _ = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_paths_reuse_existing_folders() {
        let root = std::env::temp_dir().join(format!("clipit-paths-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Valorant")).unwrap();
        let p = clip_path(&root, "VALORANT", ".mp4").unwrap();
        assert_eq!(p.parent().unwrap(), root.join("Valorant"));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Valorant 20") && name.ends_with(".mp4"), "{name}");
        assert!(crate::scanner::parse_recorded(&name).is_some());
        std::fs::write(&p, b"x").unwrap();
        let p2 = clip_path(&root, "valorant", ".mp4").unwrap();
        assert_ne!(p, p2);
        let q = clip_path(&root, "Cool: Game?", ".mp4").unwrap();
        assert_eq!(q.parent().unwrap(), root.join("Cool Game"));
        assert!(clip_path(Path::new(""), "x", ".mp4").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rules() {
        let mut cfg = Capture::default();
        assert!(game_allowed(&cfg, "game.exe", true));
        assert!(!game_allowed(&cfg, "game.exe", false));
        cfg.games.push(GameRule { exe: "game.exe".into(), name: "Game".into(), enabled: false });
        assert!(!game_allowed(&cfg, "game.exe", true));
        cfg.games_mode = "selected".into();
        assert!(!game_allowed(&cfg, "other.exe", true));
        cfg.games.push(GameRule { exe: "windowed.exe".into(), name: "W".into(), enabled: true });
        assert!(game_allowed(&cfg, "windowed.exe", false));
    }

    /// The real capture with the killfeed tap: ffmpeg → named pipe → frames.
    /// Run with: cargo test tap_pipeline_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn tap_pipeline_real() {
        let enc = "h264_nvenc";
        let mon = game::monitors().into_iter().next().expect("a monitor");
        let plan = encoder::plan(&Capture::default(), mon.width as u32, mon.height as u32, enc);
        let pipe = mux::Pipe::new_inbound().unwrap();
        let mut args = encoder::args(&Source::Monitor(mon.handle), &plan, false, Some(&pipe.name));
        args.splice(0..0, ["-t".to_string(), "4".to_string()]);
        let mut child = Command::new("ffmpeg").args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut out = child.stdout.take().unwrap();
        let ts = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = out.read_to_end(&mut v);
            v.len()
        });
        let mut tap = pipe.connect_reader().unwrap();
        let mut frame = vec![0u8; encoder::TAP_W * encoder::TAP_H * 3];
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed() < Duration::from_secs(6) && tap.read_exact(&mut frame).is_ok() {
            n += 1;
        }
        let _ = child.kill();
        let _ = child.wait();
        let ts_bytes = ts.join().unwrap();
        println!("tap frames: {n} in {:?}, video bytes: {ts_bytes}", t.elapsed());
        assert!(n >= 8, "about 4 tap frames a second");
        assert!(ts_bytes > 100_000, "video still encoded");
    }

    /// The whole hand cam path: screen capture + camera → save a replay and a
    /// recording → both get the camera. Needs a camera:
    /// Two clips a few seconds apart: the second starts where the first ended.
    /// cargo test replay_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn replay_real() {
        // One engine per run (the recorder's supervisor lives until the process ends):
        // CLIPIT_ENGINE=classic cargo test replay_real -- --ignored --nocapture
        replay_real_with(&std::env::var("CLIPIT_ENGINE").unwrap_or_else(|_| "native".into()));
    }

    fn replay_real_with(engine_name: &str) {
        let dir = std::env::temp_dir().join(format!("clipit-rp-{}-{engine_name}", std::process::id()));
        let mut cfg = Capture::default();
        cfg.engine = engine_name.into();
        cfg.target = "monitor".into();
        cfg.desktop_audio = true;
        cfg.mic = true;
        cfg.sounds = false;
        cfg.auto_clip_kills = false;
        cfg.count_kills = false;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let ffmpeg = crate::media::find_ffmpeg().map(|f| f.path).unwrap();
        let engine = Engine::start(Some(ffmpeg.clone()), String::new(), cfg, dir.join("clips").to_string_lossy().into_owned(), dir.join("data"), Box::new(move |ev| {
            let msg = match ev {
                Event::ClipSaved { path, .. } => format!("saved {}", path.display()),
                Event::Warning(w) | Event::ClipFailed(w) => format!("problem {w}"),
                _ => return,
            };
            let _ = tx.lock().unwrap().send(msg);
        }));
        let t = Instant::now();
        while engine.status().buffered_secs < 20.0 {
            assert!(t.elapsed() < Duration::from_secs(60), "capture didn't start: {:?}", engine.status());
            std::thread::sleep(Duration::from_millis(200));
        }
        let st = engine.status();
        let mut meter = crate::usage::Meter::default();
        meter.read();
        std::thread::sleep(Duration::from_secs(5));
        let use_ = meter.read();
        let probe = |p: &str| -> (f64, String) {
            let o = Command::new(&ffmpeg).args(["-hide_banner", "-i", p]).output().unwrap();
            let e = String::from_utf8_lossy(&o.stderr).to_string();
            let d = e.split("Duration: ").nth(1).unwrap().split(',').next().unwrap().to_string();
            let v: Vec<f64> = d.split(':').map(|x| x.parse().unwrap()).collect();
            let streams = e.lines().filter(|l| l.contains("Stream #")).map(|l| l.trim().split(": ").nth(1).unwrap_or("").split(' ').next().unwrap_or("").to_string()).collect::<Vec<_>>().join("+");
            (v[0] * 3600.0 + v[1] * 60.0 + v[2], streams)
        };
        let pressed = Instant::now();
        engine.save_replay().unwrap();
        let first = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let took = pressed.elapsed();
        let first = first.strip_prefix("saved ").unwrap_or_else(|| panic!("{first}")).to_string();
        let (d1, streams) = probe(&first);
        let o = Command::new(&ffmpeg).args(["-hide_banner", "-v", "error", "-i", &first, "-f", "null", "-"]).output().unwrap();
        let errors = String::from_utf8_lossy(&o.stderr).lines().count();
        let size = std::fs::metadata(&first).map(|m| m.len()).unwrap_or(0);
        println!("[{engine_name}] {} | buffer {:.1}s | saved in {:?}: {:.2}s, {} MB, {streams}, decode errors {errors} | Clipit+children CPU {:.1}% RAM {} MB",
            st.encoder, st.buffered_secs, took, d1, size / 1_000_000, use_.cpu, use_.ram / 1_048_576);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(d1 > 19.0, "{d1}");
        assert_eq!(errors, 0);
    }

    /// A 6 s recording on the native engine: starts at once, plays cleanly.
    /// cargo test recording_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn recording_real() {
        let dir = std::env::temp_dir().join(format!("clipit-rec-{}", std::process::id()));
        let mut cfg = Capture::default();
        cfg.target = "monitor".into();
        cfg.sounds = false;
        cfg.auto_clip_kills = false;
        cfg.count_kills = false;
        cfg.noise_suppression = true;
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let ffmpeg = crate::media::find_ffmpeg().map(|f| f.path).unwrap();
        let engine = Engine::start(Some(ffmpeg.clone()), String::new(), cfg, dir.join("clips").to_string_lossy().into_owned(), dir.join("data"), Box::new(move |ev| {
            let msg = match ev {
                Event::RecordingStopped { path, size } => format!("saved {}|{size}", path.display()),
                Event::RecordingFailed(w) | Event::Warning(w) => format!("problem {w}"),
                _ => return,
            };
            let _ = tx.lock().unwrap().send(msg);
        }));
        let t = Instant::now();
        while engine.status().buffered_secs < 3.0 {
            assert!(t.elapsed() < Duration::from_secs(40), "capture didn't start: {:?}", engine.status());
            std::thread::sleep(Duration::from_millis(200));
        }
        let pressed = Instant::now();
        engine.toggle_recording().unwrap();
        println!("recording started in {:?} ({})", pressed.elapsed(), engine.status().encoder);
        std::thread::sleep(Duration::from_secs(6));
        let stopped = Instant::now();
        engine.toggle_recording().unwrap();
        let msg = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        println!("stopped in {:?}: {msg}", stopped.elapsed());
        let path = msg.strip_prefix("saved ").expect("saved").split('|').next().unwrap().to_string();
        let o = Command::new(&ffmpeg).args(["-hide_banner", "-v", "error", "-i", &path, "-f", "null", "-"]).output().unwrap();
        let errors = String::from_utf8_lossy(&o.stderr).to_string();
        let info = Command::new(&ffmpeg).args(["-hide_banner", "-i", &path]).output().unwrap();
        let text = String::from_utf8_lossy(&info.stderr).to_string();
        for l in text.lines().filter(|l| l.contains("Duration") || l.contains("Stream #")) {
            println!("{l}");
        }
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.trim().is_empty(), "{errors}");
        assert!(text.contains("Duration: 00:00:0"), "{text}");
    }

    /// CLIPIT_CAMERA="Iriun Webcam" cargo test handcam_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn handcam_real() {
        let device = std::env::var("CLIPIT_CAMERA").expect("CLIPIT_CAMERA");
        let dir = std::env::temp_dir().join(format!("clipit-hc-{}", std::process::id()));
        let mut cfg = Capture::default();
        cfg.target = "monitor".into();
        cfg.desktop_audio = false;
        cfg.mic = false;
        cfg.sounds = false;
        cfg.auto_clip_kills = false;
        cfg.handcam = crate::config::HandCam { enabled: true, device, shape: "circle".into(), border: "white".into(), gamma: 130, temperature: 4500, sharpness: 40, brightness: 10, ..Default::default() };
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let ffmpeg = crate::media::find_ffmpeg().map(|f| f.path);
        let engine = Engine::start(ffmpeg, String::new(), cfg, dir.join("clips").to_string_lossy().into_owned(), dir.join("data"), Box::new(move |ev| {
            let msg = match ev {
                Event::ClipSaved { path, size, .. } => format!("saved {} {size}", path.display()),
                Event::RecordingStopped { path, size } => format!("saved {} {size}", path.display()),
                Event::ClipUpdated { path, size } => format!("updated {} {size}", path.display()),
                Event::Warning(w) | Event::ClipFailed(w) | Event::RecordingFailed(w) => format!("problem {w}"),
                _ => return,
            };
            let _ = tx.lock().unwrap().send(msg);
        }));
        let t = Instant::now();
        while !(engine.status().handcam_on && engine.status().buffered_secs > 6.0) {
            assert!(t.elapsed() < Duration::from_secs(30), "capture + camera didn't start: {:?}", engine.status());
            std::thread::sleep(Duration::from_millis(200));
        }
        engine.save_replay().unwrap();
        let saved = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        println!("replay: {saved} ({:?})", t.elapsed());
        assert!(saved.starts_with("saved"), "{saved}");
        let updated = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        println!("with camera: {updated} ({:?})", t.elapsed());
        assert!(updated.starts_with("updated"), "{updated}");
        engine.toggle_recording().unwrap();
        std::thread::sleep(Duration::from_secs(4));
        engine.toggle_recording().unwrap();
        let rec = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        println!("recording: {rec}");
        assert!(rec.starts_with("saved"), "{rec}");
        let updated = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        println!("recording with camera: {updated}");
        assert!(updated.starts_with("updated"), "{updated}");
        engine.shutdown();
        let left: Vec<_> = std::fs::read_dir(dir.join("data").join("handcam")).map(|r| r.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "temporary files left: {left:?}");
        println!("clips in {}", dir.display());
    }

    #[test]
    fn busy_clips_report_the_hand_cam_first() {
        let (a, b) = (PathBuf::from("clips/a.mp4"), PathBuf::from("clips/b.mp4"));
        let list = vec![(a.clone(), "reading"), (a.clone(), "handcam"), (b.clone(), "reading")];
        assert_eq!(busy_task(&list, &a), Some("handcam"), "the file will change: that's what to show");
        assert_eq!(busy_task(&list, &b), Some("reading"));
        assert_eq!(busy_task(&list[2..], &a), None);
    }

    #[test]
    fn recovers_crashed_recordings() {
        let root = std::env::temp_dir().join(format!("clipit-parts-{}", std::process::id()));
        let g = root.join("Game");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.00.mp4.part"), b"\x00\x00\x00\x18ftypiso5....moof rec").unwrap();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.01.mp4.part"), b"half").unwrap();
        // A whole replay whose rename failed: ftyp, moov, mdat, ending exactly at the end.
        let mp4box = |t: &[u8], body: usize| { let mut b = ((8 + body) as u32).to_be_bytes().to_vec(); b.extend(t); b.extend(vec![0u8; body]); b };
        let whole = [mp4box(b"ftyp", 8), mp4box(b"moov", 20), mp4box(b"free", 4), mp4box(b"mdat", 100)].concat();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.02.mp4.part"), &whole).unwrap();
        // Half-written replay: no moov yet, mdat cut short.
        let cut = [mp4box(b"ftyp", 8), mp4box(b"free", 20), mp4box(b"mdat", 100)].concat();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.03.mp4.part"), &cut[..cut.len() - 30]).unwrap();
        recover_parts(&root);
        assert!(g.join("Game 2026.01.01 - 00.00.00.00.mp4").exists());
        assert!(!g.join("Game 2026.01.01 - 00.00.00.01.mp4.part").exists());
        assert!(g.join("Game 2026.01.01 - 00.00.00.02.mp4").exists(), "a whole clip is kept");
        assert!(!g.join("Game 2026.01.01 - 00.00.00.03.mp4.part").exists());
        assert!(!g.join("Game 2026.01.01 - 00.00.00.03.mp4").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
