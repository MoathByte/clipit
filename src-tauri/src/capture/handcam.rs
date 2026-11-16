//! Hand cam helpers: cameras, placement, the picture chain and shapes.
//! The live part (camera process + feed into the capture) is in camfeed.rs.

use crate::config::HandCam;
use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

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
pub fn shape_sdf(shape: &str, w: f64, h: f64, px: f64, py: f64) -> f64 {
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

pub fn coverage(d: f64) -> f64 {
    (0.5 - d).clamp(0.0, 1.0)
}

pub fn border_width(cw: u32) -> u32 {
    ((cw as f64 * 0.012).round() as u32).max(2)
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
