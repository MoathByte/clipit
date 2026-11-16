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
pub mod mux;
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
    ClipSaved { path: PathBuf, size: u64, kills: Option<u32> },
    /// What a saved VALORANT clip shows: your kills, the map and your agent.
    ClipInfo { path: PathBuf, info: valorant::ClipInfo },
    ClipFailed(String),
    /// Saved, but something extra didn't work (e.g. the hand cam).
    Warning(String),
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
    child: Child,
    reader: Option<JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    audio_stop: Arc<AtomicBool>,
    audio_threads: Vec<JoinHandle<()>>,
    /// Killfeed tap pipe + the thread reading it (VALORANT kill clips).
    tap: Option<(String, JoinHandle<()>)>,
    /// Size of the recorded frames.
    size: (u32, u32),
}

/// The hand cam's ffmpeg (camera -> its own RAM buffer).
struct CamRun {
    key: String,
    device: String,
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
        let want = hc.enabled && !hc.device.trim().is_empty() && self.s.run.lock().unwrap().is_some();
        let key = handcam::key(hc);
        let mut cam = self.s.cam.lock().unwrap();
        // Died (unplugged, used by another app)?
        let died = cam.as_mut().is_some_and(|c| matches!(c.child.try_wait(), Ok(Some(_))));
        if died {
            let c = cam.take().unwrap();
            let err = c.stderr.lock().unwrap().trim().lines().last().unwrap_or("").to_string();
            stop_cam(c);
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
        let mut cmd = Command::new(ffmpeg);
        cmd.args(handcam::camera_args(hc, &enc)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
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
        Ok(CamRun { key, device: hc.device.clone(), child, reader: Some(reader), stderr })
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
                if let Ok(Some(_)) = r.child.try_wait() {
                    let err = r.stderr.lock().unwrap().trim().to_string();
                    let game_gone = matches!(r.source, Source::Window(h) if !game::alive(h));
                    let r = run.take().unwrap();
                    drop(run);
                    self.stop_recording();
                    stop_running(r);
                    if !game_gone {
                        failures += 1;
                        backoff_until = Instant::now() + Duration::from_secs(if failures > 3 { 30 } else { 2 });
                        let msg = err.lines().last().unwrap_or("The capture stopped unexpectedly").to_string();
                        self.set_status("error", &format!("Capture stopped: {msg}"), None);
                    }
                    continue;
                }
            }

            let desired = if want { self.desired_source(&cfg, run.as_ref()) } else { None };
            let key = pipeline_key(&cfg);
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
                        match self.start_running(&ffmpeg, &cfg, src, label, game, key) {
                            Ok(r) => {
                                *run = Some(r);
                                failures = 0;
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
                let ring = self.s.ring.lock().unwrap();
                let secs = ring.buffered_secs();
                let ram = ring.bytes() + self.s.desktop.lock().unwrap().bytes() + self.s.mic.lock().unwrap().bytes();
                drop(ring);
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
                    encoder: friendly_encoder(&r.plan.encoder),
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
            && cfg.auto_clip_kills
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
                .spawn(move || engine.watch_kills(pipe, src))
                .expect("killfeed thread");
            (name, t)
        });
        let size = if plan.width > 0 { (plan.width, plan.height) } else { (w, h) };
        Ok(Running { source, game, label, plan, key, child, reader: Some(reader), stderr: stderr_buf, audio_stop, audio_threads, tap, size })
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
        let snap = self.s.ring.lock().unwrap().snapshot(cfg.replay_secs as f64).ok_or("Nothing recorded yet — try again in a second.")?;
        self.save_snapshot(snap, None)
    }

    /// Saves a round of kills: from `start_hns` to `end_hns` on the capture clock.
    fn save_round(&self, start_hns: i64, end_hns: i64, kills: u32) -> Result<(), String> {
        let snap = self.s.ring.lock().unwrap().snapshot_range(start_hns, end_hns).ok_or("Nothing recorded yet.")?;
        self.save_snapshot(snap, Some(kills))
    }

    fn save_snapshot(&self, snap: ts::Snapshot, kills: Option<u32>) -> Result<(), String> {
        let cfg = self.s.cfg.lock().unwrap().clone();
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        let video = self.s.run.lock().unwrap().as_ref().map(|r| VideoInfo::of(r)).ok_or("Capture isn't running.")?;
        let fps = video.fps;
        let (start, end) = (snap.start_hns() + AUDIO_SHIFT_HNS, snap.end_hns() + AUDIO_SHIFT_HNS);
        let opts = Self::audio_opts(&cfg);
        let d = opts.desktop.then(|| self.s.desktop.lock().unwrap().read(start, end));
        let m = opts.mic.then(|| self.s.mic.lock().unwrap().read(start, end));
        let pcm = mux::interleave(d.as_deref(), m.as_deref());
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
        let shared = self.s.clone();
        std::thread::spawn(move || {
            // With a hand cam the plain clip is written first, then the camera added.
            let raw = if cam.is_some() { raw_path(&out) } else { out.clone() };
            let res = mux::save_clip(&ffmpeg, &snap, pcm, &opts, fps, &raw, &|c| shared.job.add(c));
            drop(snap);
            let res = match (res, &cam) {
                (Ok(size), Some((file, cam_start))) => Ok(add_handcam(&shared, &ffmpeg, &cfg.handcam, &video, &raw, &out, file, cam_start - game_start).unwrap_or(size)),
                (r, _) => r,
            };
            if let Some((file, _)) = &cam {
                let _ = std::fs::remove_file(file);
            }
            let saved = res.is_ok();
            (shared.events)(match res {
                Ok(size) => Event::ClipSaved { path: out.clone(), size, kills },
                Err(e) => Event::ClipFailed(e),
            });
            // Kills, map and agent are read from the clip without the camera on it.
            let read_from = if raw.exists() { raw.clone() } else { out.clone() };
            if saved && analyze {
                let mut info = valorant::analyze_clip(&ffmpeg, &read_from, &shared.data_dir.join("valorant"), kills, &|c| shared.job.add(c));
                info.auto = false;
                (shared.events)(Event::ClipInfo { path: out.clone(), info });
            }
            if raw != out {
                let _ = std::fs::remove_file(&raw);
            }
        });
        Ok(())
    }

    // ---------- watching for kills (VALORANT) ----------

    /// Reads the killfeed tap (4 frames a second of the killfeed corner, from our own
    /// capture) and saves one clip per round: from `kill_before_secs` before your
    /// first kill to `kill_after_secs` after your last. A round is over once no kill
    /// has happened for ROUND_GAP_SECS — longer than a round's end plus buy phase can
    /// be, so an ace stays in one clip, and two rounds never merge.
    fn watch_kills(&self, pipe: mux::Pipe, source: Source) {
        let Ok(mut tap) = pipe.connect_reader() else { return };
        let mut frame = vec![0u8; encoder::TAP_W * encoder::TAP_H * 3];
        let mut tracker = killfeed::Tracker::new();
        let mut round = killfeed::Round::default();
        // Foreground window and whether it's VALORANT (looked up once per window).
        let mut front: (isize, bool) = (0, false);
        while tap.read_exact(&mut frame).is_ok() {
            let (on, rules) = {
                let c = self.s.cfg.lock().unwrap();
                (c.enabled && c.auto_clip_kills, Self::kill_rules(&c))
            };
            let now = Instant::now();
            let now_hns = now_hns();
            if let Some(end) = round.finished(now, &rules) {
                self.save_highlights(end);
            }
            if !on {
                tracker.reset();
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
                    continue;
                }
            }
            let img = killfeed::Image { w: encoder::TAP_W, h: encoder::TAP_H, stride: encoder::TAP_W * 3, bgr: &frame };
            let rows = killfeed::own_kills(&img, 1.0);
            let new = tracker.feed(&rows, 32, now) as u32;
            if new > 0 {
                #[cfg(debug_assertions)]
                println!("[killfeed] +{new} kill(s), yours in the feed at {rows:?}");
                round.kill(new, now, now_hns);
            }
        }
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
    pub fn analyze(&self, clip: &Path) -> Result<valorant::ClipInfo, String> {
        let ffmpeg = self.s.ffmpeg.clone().ok_or("ffmpeg isn't installed")?;
        let info = valorant::analyze_clip(&ffmpeg, clip, &self.s.data_dir.join("valorant"), None, &|c| self.s.job.add(c));
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
            let ok = self.s.run.lock().unwrap().is_some() && self.s.ring.lock().unwrap().header().is_some();
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

        *self.s.recording.lock().unwrap() = Some(Recording { path: out.clone(), start_hns, stop, video: Some(video), audio, pipe_name, mux: Some(live), failed, cam });
        if cfg.sounds {
            sound::play(sound::Cue::RecordStart);
        }
        (self.s.events)(Event::RecordingStarted(out));
        Ok(())
    }

    pub fn stop_recording(&self) {
        let Some(mut rec) = self.s.recording.lock().unwrap().take() else {
            self.s.want_record.store(false, Ordering::SeqCst);
            return;
        };
        self.s.want_record.store(false, Ordering::SeqCst);
        // Stop feeding video (dropping the tee closes the channel → writer exits).
        self.s.ring.lock().unwrap().tee = None;
        self.s.cam_ring.lock().unwrap().tee = None; // closes the hand cam file
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
        let sounds = self.s.cfg.lock().unwrap().sounds;
        let path = rec.path.clone();
        let failed = rec.failed.load(Ordering::SeqCst);
        let mut result = rec.mux.take().map(|m| m.finish()).unwrap_or(Err("no muxer".into()));
        // Hand cam: add the camera now that the recording is complete.
        if let (Ok(size), Some((file, cam_start))) = (&result, &rec.cam) {
            let (cam_start, rec_start) = (cam_start.load(Ordering::SeqCst), rec.start_hns.load(Ordering::SeqCst));
            let video = self.s.run.lock().unwrap().as_ref().map(VideoInfo::of);
            if let (Some(ffmpeg), Some(video), true) = (self.s.ffmpeg.clone(), video, cam_start > 0 && rec_start > 0) {
                let raw = raw_path(&path);
                if std::fs::rename(&path, &raw).is_ok() {
                    let hc = self.s.cfg.lock().unwrap().handcam.clone();
                    result = Ok(add_handcam(&self.s, &ffmpeg, &hc, &video, &raw, &path, file, cam_start - rec_start).unwrap_or(*size));
                    let _ = std::fs::remove_file(&raw);
                }
            }
        }
        if let Some((file, _)) = &rec.cam {
            let _ = std::fs::remove_file(file);
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

/// `raw` + camera -> `out`. On failure `raw` becomes `out` (the clip is kept
/// without the camera) and a warning is shown. `cam_offset_hns` = camera start
/// minus clip start.
fn add_handcam(shared: &Shared, ffmpeg: &Path, hc: &crate::config::HandCam, v: &VideoInfo, raw: &Path, out: &Path, cam_file: &Path, cam_offset_hns: i64) -> Result<u64, String> {
    let work = shared.data_dir.join("handcam");
    let c = handcam::Compose {
        ffmpeg,
        game: raw,
        camera: cam_file,
        camera_offset: cam_offset_hns as f64 / 1e7 + hc.delay_ms as f64 / 1000.0,
        cam: hc,
        size: v.size,
        encoder: &v.encoder,
        kbps: v.kbps,
        max_kbps: v.max_kbps,
        fps: v.fps,
        work: &work,
        out,
    };
    match handcam::compose(&c, &|ch| shared.job.add(ch)) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = std::fs::rename(raw, out);
            (shared.events)(Event::Warning(format!("Saved without the hand cam. {e}")));
            Err(e)
        }
    }
}

fn stop_cam(mut c: CamRun) {
    let _ = c.child.kill();
    let _ = c.child.wait();
    if let Some(t) = c.reader.take() {
        let _ = t.join();
    }
}

fn stop_running(mut r: Running) {
    r.audio_stop.store(true, Ordering::SeqCst);
    let _ = r.child.kill();
    let _ = r.child.wait();
    if let Some(t) = r.reader.take() {
        let _ = t.join();
    }
    if let Some((name, t)) = r.tap.take() {
        mux::Pipe::poke_writer(&name); // in case ffmpeg never opened it
        let _ = t.join();
    }
    for t in r.audio_threads.drain(..) {
        let _ = t.join();
    }
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
            if is_fragmented(&p) {
                // A recording (fragmented MP4): everything written so far plays.
                let fixed = PathBuf::from(name.trim_end_matches(".part"));
                if !fixed.exists() {
                    let _ = std::fs::rename(&p, fixed);
                }
            } else {
                let _ = std::fs::remove_file(&p); // half-written replay: useless
            }
        }
    }
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
        cfg.handcam = crate::config::HandCam { enabled: true, device, shape: "circle".into(), border: "white".into(), ..Default::default() };
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tx = Mutex::new(tx);
        let ffmpeg = crate::media::find_ffmpeg().map(|f| f.path);
        let engine = Engine::start(ffmpeg, String::new(), cfg, dir.join("clips").to_string_lossy().into_owned(), dir.join("data"), Box::new(move |ev| {
            let msg = match ev {
                Event::ClipSaved { path, size, .. } => format!("saved {} {size}", path.display()),
                Event::RecordingStopped { path, size } => format!("saved {} {size}", path.display()),
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
        engine.toggle_recording().unwrap();
        std::thread::sleep(Duration::from_secs(4));
        engine.toggle_recording().unwrap();
        let rec = rx.recv_timeout(Duration::from_secs(60)).unwrap();
        println!("recording: {rec}");
        assert!(rec.starts_with("saved"), "{rec}");
        engine.shutdown();
        let left: Vec<_> = std::fs::read_dir(dir.join("data").join("handcam")).map(|r| r.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "temporary files left: {left:?}");
        println!("clips in {}", dir.display());
    }

    #[test]
    fn recovers_crashed_recordings() {
        let root = std::env::temp_dir().join(format!("clipit-parts-{}", std::process::id()));
        let g = root.join("Game");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.00.mp4.part"), b"\x00\x00\x00\x18ftypiso5....moof rec").unwrap();
        std::fs::write(g.join("Game 2026.01.01 - 00.00.00.01.mp4.part"), b"half").unwrap();
        recover_parts(&root);
        assert!(g.join("Game 2026.01.01 - 00.00.00.00.mp4").exists());
        assert!(!g.join("Game 2026.01.01 - 00.00.00.01.mp4.part").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
