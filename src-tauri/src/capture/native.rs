//! Rust side of clipcore, the native C++ recording engine (native/clipcore.h).
//! The engine captures and encodes on the GPU and hands over encoded packets,
//! which go straight into the packet buffer; killfeed tap frames go to a channel.

use super::packets::{Packet, Ring};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

#[repr(C)]
struct CcOptions {
    hwnd: isize,
    hmonitor: isize,
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    max_kbps: u32,
    codec: u32,
    cursor: u32,
    tap_every: u32,
    tap_w: u32,
    tap_h: u32,
    on_packet: Option<extern "C" fn(*mut c_void, *const u8, usize, i64, i32)>,
    on_tap: Option<extern "C" fn(*mut c_void, *const u8, u32, u32, i64)>,
    user: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CcStats {
    width: u32,
    height: u32,
    src_width: u32,
    src_height: u32,
    captured: u64,
    encoded: u64,
    repeated: u64,
    dropped: u64,
    last_capture_hns: i64,
    last_packet_hns: i64,
    failed: i32,
    encoder: [c_char; 128],
    error: [c_char; 256],
}

#[repr(C)]
struct CcEngine {
    _private: [u8; 0],
}

#[cfg(windows)]
extern "C" {
    fn cc_start(o: *const CcOptions, err: *mut c_char, errlen: usize) -> *mut CcEngine;
    fn cc_get_stats(e: *mut CcEngine, s: *mut CcStats);
    fn cc_stop(e: *mut CcEngine);
    fn cc_aac_encode(
        pcm: *const i16,
        frames: usize,
        channels: u32,
        rate: u32,
        kbps: u32,
        on_frame: extern "C" fn(*mut c_void, *const u8, usize, i64, i32),
        user: *mut c_void,
        asc: *mut u8,
        err: *mut c_char,
        errlen: usize,
    ) -> i32;
}

fn text(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

/// What the engine gets from the app while it runs.
struct Ctx {
    ring: Arc<Mutex<Ring>>,
    /// Seconds to keep and the byte limit (changed live).
    keep_secs: Arc<AtomicU32>,
    max_bytes: usize,
    tap: Option<SyncSender<Vec<u8>>>,
}

extern "C" fn on_packet(user: *mut c_void, data: *const u8, size: usize, t: i64, key: i32) {
    let ctx = unsafe { &*(user as *const Ctx) };
    let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
    let mut ring = ctx.ring.lock().unwrap();
    ring.push(Packet { t, key: key != 0, data: Arc::new(bytes) });
    ring.trim(ctx.keep_secs.load(Ordering::Relaxed) as f64, ctx.max_bytes);
}

extern "C" fn on_tap(user: *mut c_void, bgr: *const u8, w: u32, h: u32, _t: i64) {
    let ctx = unsafe { &*(user as *const Ctx) };
    if let Some(tx) = &ctx.tap {
        let frame = unsafe { std::slice::from_raw_parts(bgr, w as usize * h as usize * 3) }.to_vec();
        let _ = tx.try_send(frame); // the reader is behind: skip a tap frame, never wait
    }
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // all of it is useful when debugging
pub struct Stats {
    pub width: u32,
    pub height: u32,
    pub captured: u64,
    pub encoded: u64,
    pub repeated: u64,
    pub dropped: u64,
    pub last_capture_hns: i64,
    pub last_packet_hns: i64,
    pub failed: bool,
    pub encoder: String,
    pub error: String,
}

pub struct Options {
    pub hwnd: isize,
    pub hmonitor: isize,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub max_kbps: u32,
    pub hevc: bool,
    pub cursor: bool,
    /// Killfeed tap frames (w, h, every N frames) and where they go.
    pub tap: Option<(u32, u32, u32, SyncSender<Vec<u8>>)>,
}

pub struct Engine {
    ptr: *mut CcEngine,
    ctx: *mut Ctx,
}

// The engine is used from the supervisor thread only; the C++ side has its own locking.
unsafe impl Send for Engine {}

impl Engine {
    pub fn start(o: Options, ring: Arc<Mutex<Ring>>, keep_secs: Arc<AtomicU32>, max_bytes: usize) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let (tap_w, tap_h, tap_every, tap_tx) = match o.tap {
                Some((w, h, n, tx)) => (w, h, n, Some(tx)),
                None => (0, 0, 0, None),
            };
            let ctx = Box::into_raw(Box::new(Ctx { ring, keep_secs, max_bytes, tap: tap_tx }));
            let opts = CcOptions {
                hwnd: o.hwnd,
                hmonitor: o.hmonitor,
                width: o.width,
                height: o.height,
                fps: o.fps,
                kbps: o.kbps,
                max_kbps: o.max_kbps,
                codec: o.hevc as u32,
                cursor: o.cursor as u32,
                tap_every,
                tap_w,
                tap_h,
                on_packet: Some(on_packet),
                on_tap: if tap_every > 0 { Some(on_tap) } else { None },
                user: ctx as *mut c_void,
            };
            let mut err = [0 as c_char; 512];
            let ptr = unsafe { cc_start(&opts, err.as_mut_ptr(), err.len()) };
            if ptr.is_null() {
                drop(unsafe { Box::from_raw(ctx) });
                return Err(text(&err));
            }
            Ok(Self { ptr, ctx })
        }
        #[cfg(not(windows))]
        {
            let _ = (o, ring, keep_secs, max_bytes);
            Err("Only on Windows".into())
        }
    }

    pub fn stats(&self) -> Stats {
        #[cfg(windows)]
        {
            let mut s: CcStats = unsafe { std::mem::zeroed() };
            unsafe { cc_get_stats(self.ptr, &mut s) };
            Stats {
                width: s.width,
                height: s.height,
                captured: s.captured,
                encoded: s.encoded,
                repeated: s.repeated,
                dropped: s.dropped,
                last_capture_hns: s.last_capture_hns,
                last_packet_hns: s.last_packet_hns,
                failed: s.failed != 0,
                encoder: text(&s.encoder),
                error: text(&s.error),
            }
        }
        #[cfg(not(windows))]
        Stats::default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            cc_stop(self.ptr); // no callbacks after this
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// AAC-LC frames (1024 samples each) and the AudioSpecificConfig.
pub struct Aac {
    pub frames: Vec<Vec<u8>>,
    pub asc: [u8; 2],
}

extern "C" fn on_aac(user: *mut c_void, data: *const u8, size: usize, _t: i64, _key: i32) {
    let out = unsafe { &mut *(user as *mut Vec<Vec<u8>>) };
    out.push(unsafe { std::slice::from_raw_parts(data, size) }.to_vec());
}

/// Interleaved 16-bit PCM to AAC (Windows' own encoder).
pub fn aac(pcm: &[i16], channels: u32, rate: u32, kbps: u32) -> Result<Aac, String> {
    #[cfg(windows)]
    {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut asc = [0u8; 2];
        let mut err = [0 as c_char; 256];
        let n = pcm.len() / channels.max(1) as usize;
        let r = unsafe {
            cc_aac_encode(pcm.as_ptr(), n, channels, rate, kbps, on_aac, &mut frames as *mut _ as *mut c_void, asc.as_mut_ptr(), err.as_mut_ptr(), err.len())
        };
        if r != 0 {
            return Err(text(&err));
        }
        Ok(Aac { frames, asc })
    }
    #[cfg(not(windows))]
    {
        let _ = (pcm, channels, rate, kbps);
        Err("Only on Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::packets::Format;

    #[test]
    fn aac_of_a_second_of_tone() {
        let rate = 48_000;
        let pcm: Vec<i16> = (0..rate).flat_map(|i| {
            let v = ((i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 8000.0) as i16;
            [v, v]
        }).collect();
        let a = aac(&pcm, 2, rate, 192).unwrap();
        assert_eq!(a.asc, [0x11, 0x90]);
        // ~47 frames for a second (+ the encoder's priming/drain).
        assert!((46..=50).contains(&a.frames.len()), "{}", a.frames.len());
        let bytes: usize = a.frames.iter().map(|f| f.len()).sum();
        assert!(bytes > 15_000 && bytes < 40_000, "{bytes}");
    }

    /// The real engine on the main monitor for 3 s.
    /// cargo test native_capture_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn native_capture_real() {
        let mon = crate::capture::game::monitors().first().map(|m| m.handle).expect("a monitor");
        let ring = Arc::new(Mutex::new(Ring::default()));
        ring.lock().unwrap().set_format(Format { codec: 0, width: 1920, height: 1080, fps: 60 });
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let e = Engine::start(
            Options { hwnd: 0, hmonitor: mon, width: 1920, height: 1080, fps: 60, kbps: 20000, max_kbps: 30000, hevc: false, cursor: true, tap: Some((770, 290, 15, tx)) },
            ring.clone(),
            Arc::new(AtomicU32::new(10)),
            usize::MAX,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let s = e.stats();
        drop(e);
        let taps = rx.try_iter().count();
        let r = ring.lock().unwrap();
        println!("{s:?} buffered {:.2}s {} KB, {taps} tap frames", r.buffered_secs(), r.bytes() / 1024);
        assert!(!s.failed, "{}", s.error);
        assert!(s.encoded > 150, "{s:?}");
        assert!(r.buffered_secs() > 2.0);
        assert!(taps >= 8, "{taps}");
    }
}
