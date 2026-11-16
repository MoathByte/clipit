//! Live hand cam: a camera process turns the camera into finished frames (turned,
//! cropped, sized, adjusted, shaped, bordered), and a steady writer feeds them to
//! the capture, which overlays them on the game as it records — like a camera
//! source in OBS. Saving a clip stays an instant copy.
//!
//! The writer sends a frame on every tick even when the camera stalls or is gone
//! (the last picture, or a transparent one), so the game capture never waits.

use super::handcam;
use crate::config::HandCam;
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

type Frame = Arc<Vec<u8>>;

/// The camera's latest frames (RGBA, `size`), shared by the camera reader, the
/// capture writer and the settings preview.
pub struct Feed {
    frames: Mutex<VecDeque<(Instant, Frame)>>,
    size: Mutex<(u32, u32)>,
    /// Camera delay (ms) the writer applies.
    pub delay_ms: AtomicI32,
}

impl Feed {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { frames: Mutex::new(VecDeque::new()), size: Mutex::new((0, 0)), delay_ms: AtomicI32::new(0) })
    }

    fn push(&self, size: (u32, u32), f: Frame) {
        let now = Instant::now();
        let keep = Duration::from_millis(self.delay_ms.load(Ordering::Relaxed).clamp(0, 2000) as u64 + 500);
        let mut q = self.frames.lock().unwrap();
        if *self.size.lock().unwrap() != size {
            q.clear();
            *self.size.lock().unwrap() = size;
        }
        q.push_back((now, f));
        while q.len() > 2 && q.front().is_some_and(|(t, _)| now.duration_since(*t) > keep) {
            q.pop_front();
        }
    }

    pub fn clear(&self) {
        self.frames.lock().unwrap().clear();
    }

    /// The frame to show now: the newest one at least `delay` old.
    fn current(&self, size: (u32, u32)) -> Option<Frame> {
        if *self.size.lock().unwrap() != size {
            return None;
        }
        let delay = Duration::from_millis(self.delay_ms.load(Ordering::Relaxed).max(0) as u64);
        let now = Instant::now();
        let q = self.frames.lock().unwrap();
        q.iter().rev().find(|(t, _)| now.duration_since(*t) >= delay).or(q.front()).map(|(_, f)| f.clone())
    }

    /// A picture arrived in the last two seconds.
    pub fn live(&self) -> bool {
        self.frames.lock().unwrap().back().is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(2))
    }

    /// The newest frame as a PNG (with its shape), at most `max_w` wide.
    pub fn preview_png(&self, max_w: u32) -> Option<Vec<u8>> {
        let (w, h) = *self.size.lock().unwrap();
        let f = self.frames.lock().unwrap().back().map(|(_, f)| f.clone())?;
        let img = image::RgbaImage::from_raw(w, h, f.to_vec())?;
        let img = if w > max_w {
            image::imageops::resize(&img, max_w, (h * max_w / w).max(1), image::imageops::FilterType::Triangle)
        } else {
            img
        };
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).ok()?;
        Some(out.into_inner())
    }
}

/// Alpha mask + border for the camera's shape, applied to each RGBA frame.
pub struct Shape {
    alpha: Vec<u8>,
    /// Border colour and coverage per pixel (empty = no border).
    border: Vec<u8>,
    rgb: [u8; 3],
}

impl Shape {
    pub fn new(h: &HandCam, w: u32, hgt: u32) -> Self {
        let (wf, hf) = (w as f64, hgt as f64);
        let shaped = h.shape == "rounded" || h.shape == "circle";
        let t = handcam::border_width(w) as f64;
        let rgb = match h.border.as_str() {
            "white" => Some([255, 255, 255]),
            "black" => Some([0, 0, 0]),
            _ => None,
        };
        let mut alpha = Vec::with_capacity((w * hgt) as usize);
        let mut border = Vec::new();
        for y in 0..hgt {
            for x in 0..w {
                let d = handcam::shape_sdf(&h.shape, wf, hf, x as f64 + 0.5, y as f64 + 0.5);
                alpha.push(if shaped { (handcam::coverage(d) * 255.0).round() as u8 } else { 255 });
                if rgb.is_some() {
                    border.push(((handcam::coverage(d) - handcam::coverage(d + t)).clamp(0.0, 1.0) * 255.0).round() as u8);
                }
            }
        }
        Self { alpha, border, rgb: rgb.unwrap_or([0, 0, 0]) }
    }

    pub fn apply(&self, px: &mut [u8]) {
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let a = self.alpha[i];
            if let Some(&b) = self.border.get(i) {
                if b > 0 {
                    let (bf, inv) = (b as u32, 255 - b as u32);
                    for c in 0..3 {
                        p[c] = ((self.rgb[c] as u32 * bf + p[c] as u32 * inv) / 255) as u8;
                    }
                }
                p[3] = a.max(b);
            } else {
                p[3] = a;
            }
        }
    }
}

/// What makes the camera process restart.
pub fn key(h: &HandCam, size: (u32, u32), fps: u32) -> String {
    format!(
        "{}|{}x{}|{fps}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        h.device, size.0, size.1, h.height, h.aspect, h.shape, h.border, h.flip_h, h.flip_v, h.rotate, h.zoom, h.brightness, h.contrast, h.saturation, h.gamma, h.temperature * 1000 + h.sharpness
    )
}

/// ffmpeg arguments: camera → finished RGBA frames of `size` on stdout.
pub fn camera_args(h: &HandCam, size: (u32, u32), fps: u32) -> Vec<String> {
    let height = h.height.clamp(240, 2160);
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "dshow", "-rtbufsize", "64M"].iter().map(|s| s.to_string()).collect();
    a.extend(["-i".to_string(), format!("video={}", h.device)]);
    // Shrink big camera pictures first (cheap), then the hand cam chain.
    let vf = format!("scale=-2:'min(ih,{height})',fps={fps},{},format=rgba", handcam::camera_filter(h, size.0, size.1));
    a.extend(["-vf".to_string(), vf, "-an".to_string(), "-f".to_string(), "rawvideo".to_string(), "-pix_fmt".to_string(), "rgba".to_string(), "pipe:1".to_string()]);
    a
}

pub struct CamProc {
    pub key: String,
    child: Child,
    reader: Option<JoinHandle<()>>,
    pub stderr: Arc<Mutex<String>>,
}

impl CamProc {
    pub fn start(ffmpeg: &Path, h: &HandCam, size: (u32, u32), fps: u32, feed: Arc<Feed>, register: &dyn Fn(&Child)) -> Result<Self, String> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(camera_args(h, size, fps)).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let mut child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
        register(&child);
        let mut out = child.stdout.take().unwrap();
        let shape = Shape::new(h, size.0, size.1);
        feed.clear();
        let reader = std::thread::Builder::new()
            .name("handcam-reader".into())
            .spawn(move || {
                let len = (size.0 * size.1 * 4) as usize;
                loop {
                    let mut buf = vec![0u8; len];
                    if out.read_exact(&mut buf).is_err() {
                        break;
                    }
                    shape.apply(&mut buf);
                    feed.push(size, Arc::new(buf));
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
        Ok(Self { key: key(h, size, fps), child, reader: Some(reader), stderr })
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.reader.take() {
            let _ = t.join();
        }
    }
}

/// Where the capture expects the camera: frame size, position, rate and the pipe.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    pub pipe: String,
    pub size: (u32, u32),
    pub at: (u32, u32),
    pub fps: u32,
}

/// Feeds the capture's camera input at `fps`, steadily, until `stop` or the
/// capture goes away. A transparent frame stands in until the camera has one.
pub fn write_loop(feed: Arc<Feed>, pipe: super::mux::Pipe, size: (u32, u32), fps: u32, stop: Arc<AtomicBool>) {
    let Ok(mut w) = pipe.connect() else { return };
    let blank = vec![0u8; (size.0 * size.1 * 4) as usize];
    let tick = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let f = feed.current(size);
        let data: &[u8] = f.as_deref().map(|v| v.as_slice()).unwrap_or(&blank);
        if w.write_all(data).is_err() {
            return;
        }
        next += tick;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind (machine busy): don't burst
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_masks_and_borders() {
        let h = HandCam { shape: "circle".into(), border: "white".into(), ..Default::default() };
        let s = Shape::new(&h, 100, 100);
        let mut px = vec![10u8; 100 * 100 * 4];
        s.apply(&mut px);
        let at = |x: usize, y: usize| &px[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
        assert_eq!(at(1, 1)[3], 0, "corner is outside the circle");
        assert_eq!(at(50, 50), &[10, 10, 10, 255], "middle is the camera");
        assert!(at(50, 1)[0] > 240, "top edge is the white border");
    }

    #[test]
    fn delay_picks_an_older_frame() {
        let feed = Feed::new();
        feed.push((1, 1), Arc::new(vec![1, 0, 0, 0]));
        std::thread::sleep(Duration::from_millis(60));
        feed.push((1, 1), Arc::new(vec![2, 0, 0, 0]));
        assert_eq!(feed.current((1, 1)).unwrap()[0], 2);
        feed.delay_ms.store(40, Ordering::SeqCst);
        assert_eq!(feed.current((1, 1)).unwrap()[0], 1);
        assert!(feed.current((2, 2)).is_none(), "wrong size: transparent instead");
    }
}
