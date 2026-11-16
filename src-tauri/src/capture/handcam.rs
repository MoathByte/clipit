//! Hand cam: a camera recorded next to the game into its own small RAM buffer,
//! then added onto each clip when it's saved. The game capture is never touched,
//! and the camera is only on while Clipit is capturing a game.

use crate::config::HandCam;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// What makes the camera ffmpeg restart when it changes.
pub fn key(h: &HandCam) -> String {
    format!("{}|{}|{}", h.device, h.height, h.fps)
}

pub fn kbps(h: &HandCam) -> u32 {
    let base = match h.height {
        0..=480 => 2_000,
        481..=720 => 4_000,
        _ => 8_000,
    };
    if h.fps > 30 { base * 3 / 2 } else { base }
}

/// ffmpeg arguments: camera → small H.264 MPEG-TS on stdout (a keyframe every second).
pub fn camera_args(h: &HandCam, encoder: &str) -> Vec<String> {
    let fps = if h.fps >= 60 { 60 } else { 30 };
    let height = h.height.clamp(240, 1080);
    let k = kbps(h);
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "dshow", "-rtbufsize", "256M"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.extend(["-i".to_string(), format!("video={}", h.device)]);
    // Never upscale; constant frame rate; 4:2:0 for every encoder.
    a.extend(["-vf".to_string(), format!("scale=-2:'min(ih,{height})',fps={fps},format=yuv420p")]);
    a.extend(super::encoder::encode_args(encoder, k, k * 3 / 2, fps));
    a.extend(
        ["-an", "-f", "mpegts", "-muxdelay", "0", "-muxpreload", "0", "-flush_packets", "1", "pipe:1"]
            .iter()
            .map(|s| s.to_string()),
    );
    a
}

/// Camera names Windows offers (DirectShow video devices).
pub fn list_cameras(ffmpeg: &Path) -> Vec<String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let Ok(out) = cmd.output() else { return Vec::new() };
    parse_devices(&String::from_utf8_lossy(&out.stderr))
}

fn parse_devices(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.trim_end().ends_with("(video)"))
        .filter_map(|l| {
            let a = l.find('"')?;
            let b = l[a + 1..].find('"')? + a + 1;
            Some(l[a + 1..b].to_string())
        })
        .collect()
}

/// Where the camera goes in a `w`×`h` frame: (x, y, width, height), all even.
pub fn layout(h: &HandCam, w: u32, hgt: u32) -> (u32, u32, u32, u32) {
    let even = |v: f64| ((v / 2.0).round() as u32) * 2;
    let a = aspect(h);
    let mut cw = even(h.size.clamp(0.05, 1.0) * w as f64).max(16);
    let mut ch = even(cw as f64 / a).max(16);
    if ch > hgt {
        ch = even(hgt as f64);
        cw = even(ch as f64 * a);
    }
    let x = even(h.x.clamp(0.0, 1.0) * w as f64).min(w.saturating_sub(cw));
    let y = even(h.y.clamp(0.0, 1.0) * hgt as f64).min(hgt.saturating_sub(ch));
    (x, y, cw, ch)
}

/// Width / height of the camera box. A circle is always round.
pub fn aspect(h: &HandCam) -> f64 {
    if h.shape == "circle" {
        return 1.0;
    }
    match h.aspect.as_str() {
        "4:3" => 4.0 / 3.0,
        "1:1" => 1.0,
        "3:4" => 3.0 / 4.0,
        "9:16" => 9.0 / 16.0,
        _ => 16.0 / 9.0,
    }
}

/// Filter for the camera picture: turn, flip, zoom, crop to the box, scale.
pub fn camera_filter(h: &HandCam, cw: u32, ch: u32) -> String {
    let mut f: Vec<String> = Vec::new();
    match h.rotate {
        90 => f.push("transpose=1".into()),
        180 => f.push("hflip,vflip".into()),
        270 => f.push("transpose=2".into()),
        _ => {}
    }
    if h.flip_h {
        f.push("hflip".into());
    }
    if h.flip_v {
        f.push("vflip".into());
    }
    let z = h.zoom.clamp(1.0, 4.0);
    if z > 1.001 {
        f.push(format!("crop=iw/{z:.3}:ih/{z:.3}"));
    }
    let a = cw as f64 / ch as f64;
    f.push(format!("crop='min(iw,ih*{a:.5})':'min(ih,iw/{a:.5})'"));
    f.push(format!("scale={cw}:{ch}"));
    if let Some(adj) = adjust_filter(h) {
        f.push(adj);
    }
    f.join(",")
}

/// Brightness, contrast, colour… as ffmpeg filters, or None when nothing changes.
pub fn adjust_filter(h: &HandCam) -> Option<String> {
    let mut f: Vec<String> = Vec::new();
    let b = h.brightness.clamp(-100, 100) as f64 * 0.003;
    let c = h.contrast.clamp(25, 300) as f64 / 100.0;
    let s = h.saturation.min(300) as f64 / 100.0;
    let g = h.gamma.clamp(25, 400) as f64 / 100.0;
    if b != 0.0 || c != 1.0 || s != 1.0 || g != 1.0 {
        f.push(format!("eq=brightness={b:.3}:contrast={c:.2}:saturation={s:.2}:gamma={g:.2}"));
    }
    let k = h.temperature.clamp(1000, 40000);
    if k != 6500 {
        f.push(format!("colortemperature=temperature={k}:pl=1"));
    }
    if h.sharpness > 0 {
        f.push(format!("unsharp=5:5:{:.2}", h.sharpness.min(100) as f64 / 100.0 * 1.5));
    }
    (!f.is_empty()).then(|| f.join(","))
}

/// A preview picture with the adjustments applied (JPEG in, JPEG out).
pub fn adjust_jpeg(ffmpeg: &Path, jpeg: &[u8], h: &HandCam) -> Result<Vec<u8>, String> {
    let Some(adj) = adjust_filter(h) else { return Ok(jpeg.to_vec()) };
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "mjpeg", "-i", "pipe:0", "-frames:v", "1", "-vf", &adj, "-q:v", "3", "-f", "mjpeg", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().unwrap();
    let data = jpeg.to_vec();
    let w = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = w.join();
    if out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("Couldn't adjust the picture").to_string());
    }
    Ok(out.stdout)
}

/// Opens the camera's own settings window (exposure, white balance, focus… —
/// whatever the camera offers), like OBS's "Configure Video". Returns when it's closed.
pub fn settings_dialog(ffmpeg: &Path, device: &str) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "dshow", "-show_video_device_dialog", "true", "-i"])
        .arg(format!("video={device}"))
        .args(["-frames:v", "1", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = cmd.spawn().and_then(|c| c.wait_with_output()).map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr);
    // Virtual and phone cameras (Iriun, OBS…) often have no settings window.
    if err.contains("property pages") {
        return Err("This camera has no settings window of its own. Phone cameras keep exposure, focus and so on in the app on your phone. The picture adjustments below work with any camera.".into());
    }
    if !out.status.success() {
        return Err(if err.contains("Could not") || err.contains("I/O error") {
            "The camera isn't connected or is in use by another app.".into()
        } else {
            err.lines().last().unwrap_or("Couldn't open the camera's settings").to_string()
        });
    }
    Ok(())
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------- shapes ----------

/// Signed distance to the shape's edge (negative inside), in pixels.
fn shape_sdf(shape: &str, w: f64, h: f64, px: f64, py: f64) -> f64 {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (dx, dy) = ((px - cx).abs(), (py - cy).abs());
    let r = match shape {
        "circle" => w.min(h) / 2.0,
        "rounded" => (w.min(h) * 0.12).max(4.0),
        _ => 0.0,
    };
    let (qx, qy) = (dx - (cx - r), dy - (cy - r));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn coverage(d: f64) -> f64 {
    (0.5 - d).clamp(0.0, 1.0)
}

pub fn border_width(cw: u32) -> u32 {
    ((cw as f64 * 0.012).round() as u32).max(2)
}

/// Grey mask (white = camera shows). None for a plain rectangle.
pub fn mask_png(h: &HandCam, cw: u32, ch: u32, out: &Path) -> Result<Option<PathBuf>, String> {
    if h.shape != "rounded" && h.shape != "circle" {
        return Ok(None);
    }
    let img = image::GrayImage::from_fn(cw, ch, |x, y| {
        let d = shape_sdf(&h.shape, cw as f64, ch as f64, x as f64 + 0.5, y as f64 + 0.5);
        image::Luma([(coverage(d) * 255.0).round() as u8])
    });
    img.save(out).map_err(|e| e.to_string())?;
    Ok(Some(out.to_path_buf()))
}

/// The border drawn over the camera's edge. None without a border.
pub fn border_png(h: &HandCam, cw: u32, ch: u32, out: &Path) -> Result<Option<PathBuf>, String> {
    let rgb = match h.border.as_str() {
        "white" => [255u8, 255, 255],
        "black" => [0u8, 0, 0],
        _ => return Ok(None),
    };
    let t = border_width(cw) as f64;
    let img = image::RgbaImage::from_fn(cw, ch, |x, y| {
        let d = shape_sdf(&h.shape, cw as f64, ch as f64, x as f64 + 0.5, y as f64 + 0.5);
        let a = (coverage(d) - coverage(d + t)).clamp(0.0, 1.0);
        image::Rgba([rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8])
    });
    img.save(out).map_err(|e| e.to_string())?;
    Ok(Some(out.to_path_buf()))
}

// ---------- adding the camera to a clip ----------

pub struct Compose<'a> {
    pub ffmpeg: &'a Path,
    /// The clip as recorded (game + audio).
    pub game: &'a Path,
    /// The camera for the same time, as MPEG-TS.
    pub camera: &'a Path,
    /// Seconds from the start of the clip to the camera's first frame.
    pub camera_offset: f64,
    pub cam: &'a HandCam,
    /// Clip frame size.
    pub size: (u32, u32),
    pub encoder: &'a str,
    pub kbps: u32,
    pub max_kbps: u32,
    pub fps: u32,
    /// Folder for the shape images (removed afterwards).
    pub work: &'a Path,
    pub out: &'a Path,
}

/// ffmpeg arguments that put the camera onto the clip. Audio is copied as is.
pub fn compose_args(c: &Compose, mask: Option<&Path>, border: Option<&Path>, part: &Path) -> Vec<String> {
    let (w, h) = c.size;
    let (x, y, cw, ch) = layout(c.cam, w, h);
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-hwaccel", "auto"].iter().map(|s| s.to_string()).collect();
    a.extend(["-i".into(), c.game.to_string_lossy().into_owned()]);
    a.extend(["-itsoffset".into(), format!("{:.3}", c.camera_offset), "-i".into(), c.camera.to_string_lossy().into_owned()]);
    let mut next = 2;
    let mut graph = format!("[1:v]{},format=yuva420p[c0];", camera_filter(c.cam, cw, ch));
    let mut cam = "c0".to_string();
    if let Some(m) = mask {
        a.extend(["-loop".into(), "1".into(), "-i".into(), m.to_string_lossy().into_owned()]);
        graph.push_str(&format!("[{next}:v]format=gray[m];[c0][m]alphamerge[c1];"));
        cam = "c1".into();
        next += 1;
    }
    graph.push_str(&format!("[0:v][{cam}]overlay={x}:{y}:eof_action=pass:format=auto[v0]"));
    let mut v = "v0".to_string();
    if let Some(b) = border {
        a.extend(["-loop".into(), "1".into(), "-i".into(), b.to_string_lossy().into_owned()]);
        graph.push_str(&format!(";[v0][{next}:v]overlay={x}:{y}:eof_action=pass:shortest=1[v1]"));
        v = "v1".into();
    }
    graph.push_str(&format!(";[{v}]format=yuv420p[v]"));
    a.extend(["-filter_complex".into(), graph, "-map".into(), "[v]".into(), "-map".into(), "0:a?".into(), "-c:a".into(), "copy".into()]);
    a.extend(super::encoder::encode_args(c.encoder, c.kbps, c.max_kbps, c.fps * 2));
    a.extend(["-map_metadata".into(), "0".into(), "-movflags".into(), "+faststart".into(), "-f".into(), "mp4".into()]);
    a.push(part.to_string_lossy().into_owned());
    a
}

/// Writes `c.out` = the clip with the camera on it.
pub fn compose(c: &Compose, register: &dyn Fn(&Child)) -> Result<u64, String> {
    std::fs::create_dir_all(c.work).map_err(|e| e.to_string())?;
    let (_, _, cw, ch) = layout(c.cam, c.size.0, c.size.1);
    let id = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let mask_file = c.work.join(format!("mask-{id}.png"));
    let border_file = c.work.join(format!("border-{id}.png"));
    let mask = mask_png(c.cam, cw, ch, &mask_file)?;
    let border = border_png(c.cam, cw, ch, &border_file)?;
    let part = super::mux::part_path(c.out);
    let args = compose_args(c, mask.as_deref(), border.as_deref(), &part);
    let mut cmd = Command::new(c.ffmpeg);
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000 | 0x0000_4000 /* below normal priority */);
    let result = (|| {
        let child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
        register(&child);
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("Couldn't add the hand cam: {}", err.lines().last().unwrap_or("ffmpeg failed")));
        }
        std::fs::rename(&part, c.out).map_err(|e| e.to_string())?;
        Ok(std::fs::metadata(c.out).map(|m| m.len()).unwrap_or(0))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    if let Some(m) = mask {
        let _ = std::fs::remove_file(m);
    }
    if let Some(b) = border {
        let _ = std::fs::remove_file(b);
    }
    result
}

/// One JPEG frame from a camera (for the settings preview).
pub fn grab_frame(ffmpeg: &Path, device: &str) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "dshow", "-i"])
        .arg(format!("video={device}"))
        .args(["-frames:v", "1", "-vf", "scale=-2:'min(ih,480)'", "-q:v", "4", "-f", "mjpeg", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    run_for_jpeg(cmd)
}

/// One JPEG frame from the camera's buffer (when it's already running).
pub fn frame_from_ts(ffmpeg: &Path, ts: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "mpegts", "-i", "pipe:0", "-frames:v", "1", "-vf", "scale=-2:'min(ih,480)'", "-q:v", "4", "-f", "mjpeg", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().unwrap();
    let w = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(&ts);
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = w.join();
    if out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("No picture from the camera").to_string());
    }
    Ok(out.stdout)
}

fn run_for_jpeg(mut cmd: Command) -> Result<Vec<u8>, String> {
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(if err.contains("Could not find") || err.contains("I/O error") {
            "The camera isn't connected or is in use by another app.".into()
        } else {
            err.lines().last().unwrap_or("No picture from the camera").to_string()
        });
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> HandCam {
        HandCam { enabled: true, device: "Iriun Webcam".into(), ..Default::default() }
    }

    #[test]
    fn finds_video_devices() {
        let text = r#"[in#0 @ 0] "Iriun Webcam" (video)
[in#0 @ 0]   Alternative name "@device_pnp_x"
[in#0 @ 0] "OBS Virtual Camera" (video)
[in#0 @ 0] "Microphone (USB)" (audio)"#;
        assert_eq!(parse_devices(text), vec!["Iriun Webcam", "OBS Virtual Camera"]);
    }

    #[test]
    fn layout_stays_inside_the_frame() {
        let mut c = cam();
        c.x = 0.95;
        c.y = 0.95;
        let (x, y, w, h) = layout(&c, 1920, 1080);
        assert_eq!((w, h), (480, 270));
        assert_eq!((x + w, y + h), (1920, 1080));
        c.shape = "circle".into();
        let (_, _, w, h) = layout(&c, 1920, 1080);
        assert_eq!(w, h);
        for v in [x, y, w, h] {
            assert_eq!(v % 2, 0);
        }
    }

    #[test]
    fn camera_filter_turns_flips_and_crops() {
        let mut c = cam();
        c.rotate = 90;
        c.flip_h = true;
        c.zoom = 1.5;
        let f = camera_filter(&c, 480, 270);
        assert!(f.starts_with("transpose=1,hflip,crop=iw/1.500:ih/1.500,"), "{f}");
        assert!(f.ends_with("scale=480:270"), "{f}");
    }

    #[test]
    fn compose_graph_uses_mask_and_border() {
        let mut c = cam();
        c.border = "white".into();
        let comp = Compose {
            ffmpeg: Path::new("ffmpeg"),
            game: Path::new("g.mp4"),
            camera: Path::new("c.ts"),
            camera_offset: 0.25,
            cam: &c,
            size: (1920, 1080),
            encoder: "h264_nvenc",
            kbps: 20000,
            max_kbps: 30000,
            fps: 60,
            work: Path::new("."),
            out: Path::new("o.mp4"),
        };
        let a = compose_args(&comp, Some(Path::new("m.png")), Some(Path::new("b.png")), Path::new("o.mp4.part"));
        let g = &a[a.iter().position(|s| s == "-filter_complex").unwrap() + 1];
        assert!(g.contains("[2:v]format=gray[m];[c0][m]alphamerge[c1]"), "{g}");
        assert!(g.contains("[0:v][c1]overlay=1402:734"), "{g}");
        assert!(g.contains("[v0][3:v]overlay="), "{g}");
        assert!(a.windows(2).any(|w| w[0] == "-itsoffset" && w[1] == "0.250"));
        assert!(a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a?"));
    }

    /// Puts a camera file onto a real clip:
    /// CLIPIT_HC_GAME=<mp4> CLIPIT_HC_CAM=<ts> CLIPIT_HC_OUT=<mp4> cargo test compose_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn compose_real() {
        let game = std::env::var("CLIPIT_HC_GAME").unwrap();
        let camf = std::env::var("CLIPIT_HC_CAM").unwrap();
        let out = std::env::var("CLIPIT_HC_OUT").unwrap();
        let mut c = cam();
        c.shape = std::env::var("CLIPIT_HC_SHAPE").unwrap_or("circle".into());
        c.border = "white".into();
        c.x = 0.02;
        c.y = 0.6;
        let work = std::env::temp_dir().join("clipit-hc-test");
        let comp = Compose {
            ffmpeg: Path::new("ffmpeg"),
            game: Path::new(&game),
            camera: Path::new(&camf),
            camera_offset: 1.0,
            cam: &c,
            size: (1920, 1080),
            encoder: "h264_nvenc",
            kbps: 20000,
            max_kbps: 30000,
            fps: 60,
            work: &work,
            out: Path::new(&out),
        };
        let t = std::time::Instant::now();
        let n = compose(&comp, &|_| {}).unwrap();
        println!("{n} bytes in {:?}", t.elapsed());
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn adjustments_only_when_changed() {
        let mut c = cam();
        assert_eq!(adjust_filter(&c), None);
        c.gamma = 130;
        c.temperature = 5000;
        c.sharpness = 40;
        let f = adjust_filter(&c).unwrap();
        assert_eq!(f, "eq=brightness=0.000:contrast=1.00:saturation=1.00:gamma=1.30,colortemperature=temperature=5000:pl=1,unsharp=5:5:0.60");
        assert!(camera_filter(&c, 480, 270).ends_with(&f));
    }

    #[test]
    fn shapes() {
        // Middle is inside, corners of a circle are outside.
        assert!(shape_sdf("circle", 100.0, 100.0, 50.0, 50.0) < 0.0);
        assert!(shape_sdf("circle", 100.0, 100.0, 2.0, 2.0) > 0.0);
        assert!(shape_sdf("rect", 100.0, 60.0, 1.0, 1.0) < 0.0);
        assert!(shape_sdf("rounded", 100.0, 60.0, 0.5, 0.5) > 0.0);
    }
}
