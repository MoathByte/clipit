//! Builds the always-running ffmpeg capture pipeline:
//! Windows Graphics Capture → (GPU scale) → hardware encoder → MPEG-TS on stdout.
//! With NVENC/AMF the frames never leave the GPU, so it costs ~1% of one CPU core.

use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use crate::config::Capture;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Window(isize),
    Monitor(isize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// 0 = keep the source size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub max_kbps: u32,
    pub encoder: String,
}

const EVEN: fn(u32) -> u32 = |v| v - v % 2;

/// Output size for a source of `w`×`h`: "1080" = at most 1080 tall, never upscaled.
pub fn output_size(resolution: &str, w: u32, h: u32) -> (u32, u32) {
    let limit: u32 = match resolution {
        "1440" => 1440,
        "1080" => 1080,
        "720" => 720,
        _ => return (EVEN(w), EVEN(h)),
    };
    if h <= limit || h == 0 {
        return (EVEN(w), EVEN(h));
    }
    (EVEN(((w as u64 * limit as u64 + h as u64 / 2) / h as u64) as u32), EVEN(limit))
}

/// Target bitrate: 1080p60 H.264 = 20 / 14 / 9 Mbps (high / balanced / small),
/// scaled for size, frame rate and codec efficiency.
pub fn bitrate_kbps(w: u32, h: u32, fps: u32, quality: &str, codec: &str) -> u32 {
    let base = match quality {
        "small" => 9000.0,
        "balanced" => 14000.0,
        _ => 20000.0,
    };
    let pixels = (w.max(2) * h.max(2)) as f64 / (1920.0 * 1080.0);
    let codec_f = match codec {
        "hevc" => 0.7,
        "av1" => 0.6,
        _ => 1.0,
    };
    let k = base * pixels.powf(0.75) * (fps.max(1) as f64 / 60.0).powf(0.6) * codec_f;
    ((k / 500.0).round() * 500.0).clamp(1500.0, 120_000.0) as u32
}

/// Approximate RAM for the buffer, in bytes.
pub fn buffer_bytes(kbps: u32, secs: u32) -> u64 {
    kbps as u64 * 1000 / 8 * secs as u64
}

pub fn candidates(codec: &str) -> &'static [&'static str] {
    match codec {
        "hevc" => &["hevc_nvenc", "hevc_amf", "hevc_qsv", "libx265"],
        "av1" => &["av1_nvenc", "av1_amf", "av1_qsv"],
        _ => &["h264_nvenc", "h264_amf", "h264_qsv", "libx264"],
    }
}

/// Encoders that really work on this PC (a tiny test encode each).
pub fn probe(ffmpeg: &Path) -> Vec<String> {
    let mut ok = Vec::new();
    for codec in ["h264", "hevc", "av1"] {
        for enc in candidates(codec) {
            let mut cmd = Command::new(ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=black:s=640x360:r=30", "-frames:v", "3", "-c:v", enc, "-f", "null", "-"]);
            #[cfg(windows)]
            cmd.creation_flags(CREATE_NO_WINDOW);
            if matches!(cmd.output(), Ok(o) if o.status.success()) {
                ok.push(enc.to_string());
            }
        }
    }
    ok
}

/// The encoder to use: the user's choice if it works, else the best available for the codec.
pub fn pick(available: &[String], codec: &str, wanted: &str) -> Option<String> {
    if wanted != "auto" && available.iter().any(|a| a == wanted) {
        return Some(wanted.to_string());
    }
    candidates(codec)
        .iter()
        .chain(candidates("h264").iter())
        .find(|c| available.iter().any(|a| a == *c))
        .map(|c| c.to_string())
}

pub fn plan(cfg: &Capture, src_w: u32, src_h: u32, encoder: &str) -> Plan {
    let (w, h) = output_size(&cfg.resolution, src_w, src_h);
    let fps = cfg.fps.clamp(15, 240);
    let codec = if encoder.starts_with("hevc") || encoder == "libx265" {
        "hevc"
    } else if encoder.starts_with("av1") {
        "av1"
    } else {
        "h264"
    };
    let kbps = bitrate_kbps(w, h, fps, &cfg.quality, codec);
    Plan {
        width: if (w, h) == (EVEN(src_w), EVEN(src_h)) { 0 } else { w },
        height: if (w, h) == (EVEN(src_w), EVEN(src_h)) { 0 } else { h },
        fps,
        kbps,
        max_kbps: kbps * 3 / 2,
        encoder: encoder.to_string(),
    }
}

/// Encoders that take GPU (D3D11) frames directly; the rest need a copy to RAM.
fn zero_copy(encoder: &str) -> bool {
    encoder.ends_with("_nvenc") || encoder.ends_with("_amf")
}

/// Size of the killfeed tap frames (the killfeed corner, scaled to 1080p size).
pub const TAP_W: usize = 770;
pub const TAP_H: usize = 290;

/// ffmpeg args for the capture. With `tap`, the same captured frames also go,
/// four a second and cropped to the killfeed corner, as raw BGR to that pipe.
pub fn args(source: &Source, plan: &Plan, cursor: bool, tap: Option<&str>) -> Vec<String> {
    let mut src = match source {
        Source::Window(h) => format!("gfxcapture=hwnd={}", *h as u64),
        Source::Monitor(m) => format!("gfxcapture=hmonitor={}", *m as u64),
    };
    src.push_str(&format!(":max_framerate={}:capture_cursor={}", plan.fps, cursor as u8));
    if plan.width > 0 {
        src.push_str(&format!(":width={}:height={}:resize_mode=scale_aspect", plan.width, plan.height));
    } else {
        src.push_str(":resize_mode=scale_aspect");
    }
    // Constant frame rate (WGC only delivers frames when something changes).
    src.push_str(&format!(",fps={}", plan.fps));
    let e = plan.encoder.as_str();
    if tap.is_some() {
        // Branch off before the encoder; the tap downloads only 4 frames a second.
        src.push_str(&format!(
            ",split=2[cap][tap];[tap]fps=4,hwdownload,format=bgra,crop=w='min(iw,{w}*ih/1080)':h='{h}*ih/1080':x='iw-min(iw,{w}*ih/1080)':y='40*ih/1080',scale={w}:{h},format=bgr24[kf];[cap]null",
            w = TAP_W,
            h = TAP_H
        ));
    }
    if !zero_copy(e) {
        src.push_str(if e == "libx264" || e == "libx265" { ",hwdownload,format=bgra,format=yuv420p" } else { ",hwdownload,format=bgra,format=nv12" });
    }
    if tap.is_some() {
        src.push_str("[vid]");
    }

    let (b, m, buf) = (format!("{}k", plan.kbps), format!("{}k", plan.max_kbps), format!("{}k", plan.max_kbps * 2));
    let g = plan.fps.to_string(); // a keyframe every second: clips start within 1 s of where you asked
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-filter_complex", &src].iter().map(|s| s.to_string()).collect();
    if tap.is_some() {
        a.extend(["-map".to_string(), "[vid]".to_string()]);
    }
    let enc: Vec<&str> = if e.ends_with("_nvenc") {
        vec!["-c:v", e, "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf, "-g", &g, "-bf", "0", "-spatial-aq", "1"]
    } else if e.ends_with("_amf") {
        vec!["-c:v", e, "-usage", "lowlatency", "-quality", "balanced", "-rc", "vbr_peak", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf, "-g", &g, "-bf", "0"]
    } else if e.ends_with("_qsv") {
        vec!["-c:v", e, "-preset", "faster", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf, "-g", &g, "-bf", "0"]
    } else {
        vec!["-c:v", e, "-preset", "veryfast", "-b:v", &b, "-maxrate", &m, "-bufsize", &buf, "-g", &g, "-bf", "0"]
    };
    a.extend(enc.iter().map(|s| s.to_string()));
    a.extend(
        ["-an", "-fps_mode", "passthrough", "-f", "mpegts", "-muxdelay", "0", "-muxpreload", "0", "-flush_packets", "1", "pipe:1"]
            .iter()
            .map(|s| s.to_string()),
    );
    if let Some(pipe) = tap {
        a.extend(["-map", "[kf]", "-f", "rawvideo", "-pix_fmt", "bgr24", "-y", pipe].iter().map(|s| s.to_string()));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(output_size("1080", 2560, 1440), (1920, 1080));
        assert_eq!(output_size("1080", 1920, 1080), (1920, 1080));
        assert_eq!(output_size("1440", 1920, 1080), (1920, 1080)); // never upscales
        assert_eq!(output_size("1080", 3440, 1440), (2580, 1080));
        assert_eq!(output_size("720", 1366, 768), (1280, 720));
        assert_eq!(output_size("native", 1365, 767), (1364, 766));
    }

    #[test]
    fn bitrates() {
        assert_eq!(bitrate_kbps(1920, 1080, 60, "high", "h264"), 20000);
        assert_eq!(bitrate_kbps(1920, 1080, 60, "balanced", "h264"), 14000);
        assert!(bitrate_kbps(2560, 1440, 60, "high", "h264") > 25000);
        assert!(bitrate_kbps(1920, 1080, 30, "high", "h264") < 15000);
        assert_eq!(bitrate_kbps(1920, 1080, 60, "high", "hevc"), 14000);
        // 1 minute at 1080p60 high ≈ 150 MB, as promised in setup.
        assert_eq!(buffer_bytes(20000, 60), 150_000_000);
    }

    #[test]
    fn picks_working_encoder() {
        let avail: Vec<String> = ["h264_nvenc", "hevc_nvenc", "libx264"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick(&avail, "h264", "auto").as_deref(), Some("h264_nvenc"));
        assert_eq!(pick(&avail, "hevc", "auto").as_deref(), Some("hevc_nvenc"));
        assert_eq!(pick(&avail, "av1", "auto").as_deref(), Some("h264_nvenc")); // falls back to H.264
        assert_eq!(pick(&avail, "h264", "libx264").as_deref(), Some("libx264"));
        assert_eq!(pick(&avail, "h264", "h264_amf").as_deref(), Some("h264_nvenc"));
        assert_eq!(pick(&[], "h264", "auto"), None);
    }

    #[test]
    fn pipeline_args() {
        let cfg = Capture::default();
        let p = plan(&cfg, 2560, 1440, "h264_nvenc");
        assert_eq!((p.width, p.height, p.fps, p.kbps), (1920, 1080, 60, 20000));
        let a = args(&Source::Monitor(65537), &p, false, None).join(" ");
        assert!(a.contains("gfxcapture=hmonitor=65537:max_framerate=60:capture_cursor=0:width=1920:height=1080:resize_mode=scale_aspect,fps=60"), "{a}");
        assert!(a.contains("-c:v h264_nvenc") && a.contains("-g 60") && a.contains("-f mpegts") && a.ends_with("pipe:1"));
        assert!(!a.contains("hwdownload"));
        let native = plan(&Capture { resolution: "native".into(), ..cfg.clone() }, 1920, 1080, "libx264");
        assert_eq!(native.width, 0);
        let a = args(&Source::Window(42), &native, true, None).join(" ");
        assert!(a.contains("gfxcapture=hwnd=42:max_framerate=60:capture_cursor=1:resize_mode=scale_aspect,fps=60,hwdownload"), "{a}");
        // With the killfeed tap: the capture splits, video is mapped explicitly,
        // and the tap goes to the pipe as raw frames.
        let a = args(&Source::Monitor(7), &p, false, Some(r"\\.\pipe\x")).join(" ");
        assert!(a.contains(",fps=60,split=2[cap][tap];[tap]fps=4,hwdownload,format=bgra,crop="), "{a}");
        assert!(a.contains("[kf];[cap]null[vid]") && a.contains("-map [vid]"), "{a}");
        assert!(a.ends_with(r"-map [kf] -f rawvideo -pix_fmt bgr24 -y \\.\pipe\x"), "{a}");
        let a = args(&Source::Monitor(7), &native, false, Some("p")).join(" ");
        assert!(a.contains("[kf];[cap]null,hwdownload,format=bgra,format=yuv420p[vid]"), "{a}");
    }
}
