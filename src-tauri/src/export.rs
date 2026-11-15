//! Trim / crop / compress clips with ffmpeg (NVENC when available).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Crop rectangle, normalized to 0..1 of the source frame.
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ExportRequest {
    pub path: String,
    pub start: f64,
    pub end: f64,
    pub crop: Option<Crop>,
    pub src_width: u32,
    pub src_height: u32,
    /// Max output size in MB (1 MB = 1,000,000 bytes). None = high quality, no limit.
    pub target_mb: Option<f64>,
    /// Output short-side resolution (e.g. 720). None = automatic.
    pub resolution: Option<u32>,
    /// Output frame rate. None = keep source.
    pub fps: Option<u32>,
    pub audio: bool,
    /// Length of the whole source clip in seconds (lets us avoid exceeding its bitrate).
    #[serde(default)]
    pub src_duration: Option<f64>,
}

#[derive(Serialize, Clone)]
pub struct ExportResult {
    pub output: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub encoder: String,
}

#[derive(Serialize, Clone)]
pub struct Progress {
    pub job: String,
    pub percent: f64,
    pub pass: u32,
}

/// Running ffmpeg processes by job id, so they can be cancelled.
#[derive(Default)]
pub struct Jobs {
    running: Mutex<HashMap<String, (Arc<Mutex<Child>>, bool)>>,
}

impl Jobs {
    pub fn busy(&self) -> bool {
        !self.running.lock().unwrap().is_empty()
    }

    pub fn cancel(&self, job: &str) {
        if let Some((child, cancelled)) = self.running.lock().unwrap().get_mut(job) {
            *cancelled = true;
            let _ = child.lock().unwrap().kill();
        }
    }
    fn was_cancelled(&self, job: &str) -> bool {
        self.running
            .lock()
            .unwrap()
            .get(job)
            .map(|(_, c)| *c)
            .unwrap_or(false)
    }
}

fn nvenc_works(ffmpeg: &Path) -> bool {
    static NVENC: OnceLock<bool> = OnceLock::new();
    *NVENC.get_or_init(|| {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
            "color=c=black:s=256x256:d=0.1", "-c:v", "h264_nvenc", "-f", "null", "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        matches!(cmd.status(), Ok(s) if s.success())
    })
}

fn even(v: f64) -> u32 {
    let v = v.round().max(2.0) as u32;
    v - (v % 2)
}

/// Picks a resolution that looks good at the given video bitrate.
fn auto_short_side(video_kbps: f64) -> u32 {
    if video_kbps >= 5000.0 {
        1080
    } else if video_kbps >= 1200.0 {
        720
    } else if video_kbps >= 600.0 {
        480
    } else {
        360
    }
}

pub struct Plan {
    pub duration: f64,
    pub out_w: u32,
    pub out_h: u32,
    pub filters: Vec<String>,
    pub audio_kbps: u32,
    pub video_kbps: Option<u32>,
}

/// Works out output size, filters and bitrates for a request.
pub fn plan(req: &ExportRequest) -> Result<Plan, String> {
    let duration = req.end - req.start;
    if !(duration > 0.2) {
        return Err("The selection is too short".into());
    }
    if req.src_width < 16 || req.src_height < 16 {
        return Err("Unknown video size".into());
    }
    let (sw, sh) = (req.src_width as f64, req.src_height as f64);

    let mut filters = Vec::new();
    let (mut cw, mut ch) = (sw, sh);
    if let Some(c) = req.crop {
        let x = c.x.clamp(0.0, 1.0);
        let y = c.y.clamp(0.0, 1.0);
        let w = c.w.clamp(0.01, 1.0 - x);
        let h = c.h.clamp(0.01, 1.0 - y);
        let full = x < 0.001 && y < 0.001 && w > 0.999 && h > 0.999;
        if !full {
            cw = even(w * sw) as f64;
            ch = even(h * sh) as f64;
            let px = ((x * sw).round() as u32).min(req.src_width - cw as u32);
            let py = ((y * sh).round() as u32).min(req.src_height - ch as u32);
            filters.push(format!("crop={}:{}:{}:{}", cw, ch, px, py));
        }
    }

    let audio_kbps;
    let mut video_kbps = None;
    let mut short = cw.min(ch);
    match req.target_mb {
        Some(mb) => {
            if !(mb >= 1.0) {
                return Err("Target size must be at least 1 MB".into());
            }
            // Keep ~8% headroom for the container and encoder overshoot.
            let total_kbps = mb * 8000.0 * 0.92 / duration;
            audio_kbps = if !req.audio { 0 } else if total_kbps < 1000.0 { 64 } else { 128 };
            let mut v = (total_kbps - audio_kbps as f64).max(100.0);
            // Never spend more bits than the source had — a 2s clip shouldn't become 10 MB.
            let src_kbps = req
                .src_duration
                .filter(|d| *d > 0.5)
                .and_then(|d| std::fs::metadata(&req.path).ok().map(|m| m.len() as f64 * 8.0 / 1000.0 / d));
            if let Some(src) = src_kbps {
                v = v.min(src.max(1000.0));
            }
            video_kbps = Some(v as u32);
            let wanted = req.resolution.unwrap_or_else(|| auto_short_side(v));
            short = short.min(wanted as f64);
        }
        None => {
            audio_kbps = if req.audio { 192 } else { 0 };
            if let Some(r) = req.resolution {
                short = short.min(r as f64);
            }
        }
    }

    let (mut out_w, mut out_h) = (even(cw), even(ch));
    if short < cw.min(ch) {
        if cw <= ch {
            out_w = even(short);
            out_h = even(ch * short / cw);
        } else {
            out_h = even(short);
            out_w = even(cw * short / ch);
        }
        filters.push(format!("scale={}:{}:flags=lanczos", out_w, out_h));
    }
    if let Some(f) = req.fps {
        filters.push(format!("fps={}", f));
    }

    Ok(Plan { duration, out_w, out_h, filters, audio_kbps, video_kbps })
}

/// Picks a file name in `dir` that doesn't exist yet.
pub fn unique_output(dir: &Path, src: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".into());
    let stem = stem.trim_end_matches(".DVR").to_string();
    // Re-editing an edit shouldn't give "x (edited) (edited)".
    let stem = match stem.rfind(" (") {
        Some(i) if crate::scanner::is_edited(&format!("{stem}.mp4")) => stem[..i].to_string(),
        _ => stem,
    };
    let first = dir.join(format!("{stem} (edited).mp4"));
    if !first.exists() {
        return first;
    }
    (2..10_000)
        .map(|n| dir.join(format!("{stem} (edited {n}).mp4")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Runs the export. `on_progress` receives 0..100. Blocking — call from a worker thread.
pub fn run(
    ffmpeg: &Path,
    jobs: &Jobs,
    job: &str,
    req: &ExportRequest,
    out: &Path,
    on_progress: &dyn Fn(f64, u32),
) -> Result<ExportResult, String> {
    let plan = plan(req)?;
    let nvenc = nvenc_works(ffmpeg);
    let part = out.with_extension("part.mp4");
    let limit_bytes = req.target_mb.map(|mb| (mb * 1_000_000.0) as u64);

    let mut video_kbps = plan.video_kbps;
    let mut pass = 1;
    let result = loop {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-nostats", "-loglevel", "error", "-progress", "pipe:1", "-y"]);
        cmd.args(["-ss", &format!("{:.3}", req.start.max(0.0))]);
        cmd.arg("-i").arg(&req.path);
        cmd.args(["-t", &format!("{:.3}", plan.duration)]);
        cmd.args(["-map", "0:v:0"]);
        if plan.audio_kbps > 0 {
            cmd.args(["-map", "0:a:0?"]);
        }
        if !plan.filters.is_empty() {
            cmd.args(["-vf", &plan.filters.join(",")]);
        }
        match (nvenc, video_kbps) {
            (true, Some(v)) => {
                cmd.args(["-c:v", "h264_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-multipass", "fullres"]);
                cmd.args(["-b:v", &format!("{v}k"), "-maxrate", &format!("{v}k"), "-bufsize", &format!("{}k", v * 2)]);
            }
            (true, None) => {
                cmd.args(["-c:v", "h264_nvenc", "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", "21", "-b:v", "0"]);
            }
            (false, Some(v)) => {
                cmd.args(["-c:v", "libx264", "-preset", "veryfast"]);
                cmd.args(["-b:v", &format!("{v}k"), "-maxrate", &format!("{v}k"), "-bufsize", &format!("{}k", v * 2)]);
            }
            (false, None) => {
                cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
            }
        }
        cmd.args(["-pix_fmt", "yuv420p"]);
        if plan.audio_kbps > 0 {
            cmd.args(["-c:a", "aac", "-b:a", &format!("{}k", plan.audio_kbps)]);
        } else {
            cmd.arg("-an");
        }
        cmd.args(["-movflags", "+faststart", "-f", "mp4"]).arg(&part);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let child = Arc::new(Mutex::new(child));
        jobs.running
            .lock()
            .unwrap()
            .insert(job.to_string(), (child.clone(), false));

        // Read stderr on its own thread so a full pipe can never block ffmpeg.
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = us.trim().parse::<f64>() {
                    let pct = (us / 1_000_000.0 / plan.duration * 100.0).clamp(0.0, 99.0);
                    on_progress(pct, pass);
                }
            }
        }
        let status = child.lock().unwrap().wait();
        let errors = err_thread.join().unwrap_or_default();
        let cancelled = jobs.was_cancelled(job);
        jobs.running.lock().unwrap().remove(job);

        if cancelled {
            let _ = std::fs::remove_file(&part);
            break Err("Cancelled".to_string());
        }
        match status {
            Ok(s) if s.success() => {}
            _ => {
                let _ = std::fs::remove_file(&part);
                let msg = errors.lines().last().unwrap_or("unknown error").to_string();
                break Err(format!("ffmpeg failed: {msg}"));
            }
        }

        let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        // If the encoder overshot the size limit, try again with a lower bitrate.
        if let (Some(limit), Some(v)) = (limit_bytes, video_kbps) {
            if size > limit && pass < 3 {
                let scale = limit as f64 / size as f64 * 0.95;
                video_kbps = Some(((v as f64) * scale).max(80.0) as u32);
                pass += 1;
                continue;
            }
            if size > limit {
                let _ = std::fs::remove_file(&part);
                break Err("Couldn't fit the clip under the size limit — try trimming it shorter".into());
            }
        }

        if out.exists() {
            let _ = std::fs::remove_file(out);
        }
        std::fs::rename(&part, out).map_err(|e| e.to_string())?;
        on_progress(100.0, pass);
        break Ok(ExportResult {
            output: out.to_string_lossy().into_owned(),
            size,
            width: plan.out_w,
            height: plan.out_h,
            encoder: if nvenc { "NVENC".into() } else { "x264".into() },
        });
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ExportRequest {
        ExportRequest {
            path: String::new(),
            start: 0.0,
            end: 45.0,
            crop: None,
            src_width: 1920,
            src_height: 1080,
            target_mb: Some(10.0),
            resolution: None,
            fps: None,
            audio: true,
            src_duration: None,
        }
    }

    #[test]
    fn short_clips_dont_exceed_source_bitrate() {
        let dir = std::env::temp_dir().join(format!("clipit-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("src.mp4");
        std::fs::write(&f, vec![0u8; 5_000_000]).unwrap(); // 5 MB over 40 s ≈ 1000 kbps
        let mut r = req();
        r.path = f.to_string_lossy().into_owned();
        r.start = 0.0;
        r.end = 2.0;
        r.src_duration = Some(40.0);
        let capped = plan(&r).unwrap().video_kbps.unwrap();
        assert!(capped <= 1000, "{capped}");
        r.src_duration = None;
        let uncapped = plan(&r).unwrap().video_kbps.unwrap();
        assert!(uncapped > 10_000, "{uncapped}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discord_10mb_picks_720p() {
        let p = plan(&req()).unwrap();
        assert_eq!((p.out_w, p.out_h), (1280, 720));
        let total = p.video_kbps.unwrap() + p.audio_kbps;
        assert!((total as f64) * 45.0 / 8000.0 < 10.0);
    }

    #[test]
    fn vertical_crop_scales_width() {
        let mut r = req();
        r.crop = Some(Crop { x: 0.341, y: 0.0, w: 0.3164, h: 1.0 }); // ~9:16
        r.resolution = Some(480);
        let p = plan(&r).unwrap();
        assert_eq!(p.out_w, 480);
        assert!(p.out_h > p.out_w);
        assert!(p.filters[0].starts_with("crop=606:1080:"), "{}", p.filters[0]);
    }

    #[test]
    fn never_upscales() {
        let mut r = req();
        r.target_mb = None;
        r.resolution = Some(1440);
        let p = plan(&r).unwrap();
        assert_eq!((p.out_w, p.out_h), (1920, 1080));
        assert!(p.filters.is_empty());
    }

    /// Real encode. Run with: CLIPIT_TEST_CLIP=<mp4> cargo test -- --ignored
    #[test]
    #[ignore]
    fn real_export_trim_crop_10mb() {
        let clip = std::env::var("CLIPIT_TEST_CLIP").expect("set CLIPIT_TEST_CLIP");
        let ffmpeg = crate::media::find_ffmpeg().expect("ffmpeg").path;
        let out_dir = std::env::temp_dir().join("clipit-test");
        std::fs::create_dir_all(&out_dir).unwrap();
        let jobs = Jobs::default();
        let cases = [
            ("trim+10MB", Some(10.0), None, 5.0, 35.0),
            ("vertical 9:16 + 10MB", Some(10.0), Some(Crop { x: 0.341, y: 0.0, w: 0.3164, h: 1.0 }), 0.0, 45.0),
            ("no limit, short", None, None, 10.0, 15.0),
        ];
        for (name, mb, crop, start, end) in cases {
            let mut r = req();
            r.path = clip.clone();
            r.target_mb = mb;
            r.crop = crop;
            r.start = start;
            r.end = end;
            let out = unique_output(&out_dir, Path::new(&clip));
            let res = run(&ffmpeg, &jobs, "t", &r, &out, &|_, _| {}).unwrap();
            println!("{name}: {} bytes, {}x{}, {} -> {}", res.size, res.width, res.height, res.encoder, res.output);
            if let Some(mb) = mb {
                assert!(res.size <= (mb * 1_000_000.0) as u64, "{name} over limit");
            }
            assert!(Path::new(&res.output).is_file());
        }
    }

    #[test]
    fn output_names() {
        let dir = std::env::temp_dir().join(format!("clipit-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("Valorant 2026.07.06 - 06.43.28.03.DVR.mp4");
        let a = unique_output(&dir, &src);
        assert_eq!(a.file_name().unwrap(), "Valorant 2026.07.06 - 06.43.28.03 (edited).mp4");
        std::fs::write(&a, b"x").unwrap();
        let b = unique_output(&dir, &src);
        assert_eq!(b.file_name().unwrap(), "Valorant 2026.07.06 - 06.43.28.03 (edited 2).mp4");
        // Editing an edit:
        let c = unique_output(&dir, &a);
        assert_eq!(c.file_name().unwrap(), "Valorant 2026.07.06 - 06.43.28.03 (edited 2).mp4");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_empty_selection() {
        let mut r = req();
        r.end = 0.1;
        assert!(plan(&r).is_err());
    }
}
