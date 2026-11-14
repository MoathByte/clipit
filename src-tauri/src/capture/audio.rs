//! Audio capture: desktop sound (WASAPI loopback) and microphone, kept as raw
//! 16-bit PCM in RAM (≈11 MB per minute per source). It is only encoded when a
//! clip is saved, so buffering costs no CPU for audio encoding.
//!
//! Every sample has a place on the system's QPC clock (the same clock the video
//! is mapped to), so desktop audio, mic and video line up exactly. Gaps (Windows
//! sends nothing while the desktop is silent) are filled with silence and clock
//! drift is corrected by skipping/padding.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const RATE: i64 = 48_000;
pub const CHANNELS: usize = 2;
const HNS: i64 = 10_000_000;

/// Absolute frame number (since the ring's origin) of a QPC time.
fn frame_at(origin_hns: i64, t_hns: i64) -> i64 {
    ((t_hns - origin_hns) as i128 * RATE as i128).div_euclid(HNS as i128) as i64
}

/// Frame number on the absolute QPC clock (shared by all rings and readers).
pub fn frame_of(t_hns: i64) -> i64 {
    frame_at(0, t_hns)
}

#[derive(Default)]
pub struct PcmRing {
    /// Set once audio has arrived.
    origin_hns: Option<i64>,
    /// Absolute frame index of `data[0]`.
    base: i64,
    data: VecDeque<i16>,
    max_frames: usize,
}

impl PcmRing {
    pub fn new(max_secs: f64) -> Self {
        Self { max_frames: (max_secs * RATE as f64) as usize, ..Default::default() }
    }

    fn end(&self) -> i64 {
        self.base + (self.data.len() / CHANNELS) as i64
    }

    /// Adds interleaved stereo samples whose first frame was captured at `t_hns`.
    pub fn push(&mut self, t_hns: i64, samples: &[f32]) {
        self.origin_hns.get_or_insert(t_hns);
        let target = frame_of(t_hns);
        if self.data.is_empty() {
            self.base = target;
        }
        let end = self.end();
        let mut skip = 0usize;
        if target > end + RATE / 100 {
            // A gap of more than 10 ms (silence or a hiccup): fill it.
            let gap = (target - end) as usize;
            self.data.extend(std::iter::repeat(0).take(gap * CHANNELS));
        } else if target < end - RATE / 20 {
            // More than 50 ms ahead of the clock (device clock drift): skip overlap.
            skip = ((end - target) as usize * CHANNELS).min(samples.len());
        }
        self.data.extend(samples[skip..].iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
        self.trim();
    }

    fn trim(&mut self) {
        let frames = self.data.len() / CHANNELS;
        if frames > self.max_frames {
            let drop = frames - self.max_frames;
            self.data.drain(..drop * CHANNELS);
            self.base += drop as i64;
        }
    }

    /// Interleaved samples for [from, to) on the QPC clock; silence where we have nothing.
    pub fn read(&self, from_hns: i64, to_hns: i64) -> Vec<i16> {
        let a = frame_of(from_hns);
        let b = frame_of(to_hns).max(a);
        self.read_frames(a, b)
    }

    /// Frames [a, b) by absolute frame number (see `frame_of`).
    pub fn read_frames(&self, a: i64, b: i64) -> Vec<i16> {
        let mut out = Vec::with_capacity(((b - a).max(0) as usize) * CHANNELS);
        let have = if self.origin_hns.is_some() { self.data.len() / CHANNELS } else { 0 };
        for f in a..b {
            let i = f - self.base;
            if i >= 0 && (i as usize) < have {
                let k = i as usize * CHANNELS;
                out.push(self.data[k]);
                out.push(self.data[k + 1]);
            } else {
                out.push(0);
                out.push(0);
            }
        }
        out
    }

    pub fn bytes(&self) -> usize {
        self.data.len() * 2
    }
}

// ---------- devices ----------

#[derive(Serialize, Clone, Debug)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub default: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct AudioDevices {
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

#[cfg(windows)]
pub fn list_devices() -> AudioDevices {
    use wasapi::{DeviceEnumerator, Direction};
    let _ = wasapi::initialize_mta();
    let mut out = AudioDevices::default();
    let Ok(en) = DeviceEnumerator::new() else { return out };
    for (dir, list) in [(Direction::Render, &mut out.outputs), (Direction::Capture, &mut out.inputs)] {
        let default_id = en.get_default_device(dir).ok().and_then(|d| d.get_id().ok());
        if let Ok(coll) = en.get_device_collection(dir) {
            for dev in &coll {
                let Ok(dev) = dev else { continue };
                let (Ok(id), Ok(name)) = (dev.get_id(), dev.get_friendlyname()) else { continue };
                list.push(AudioDevice { default: Some(&id) == default_id.as_ref(), id, name });
            }
        }
    }
    out
}

#[cfg(not(windows))]
pub fn list_devices() -> AudioDevices {
    AudioDevices::default()
}

// ---------- capture thread ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// What you hear (loopback of an output device).
    Desktop,
    Mic,
}

/// Captures `source` into `ring` until `stop` is set. `device` empty = the Windows
/// default (and follows it when you switch headsets).
pub fn spawn(source: Source, device: String, ring: Arc<Mutex<PcmRing>>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("audio-{source:?}"))
        .spawn(move || {
            #[cfg(windows)]
            {
                let _ = wasapi::initialize_mta();
                while !stop.load(Ordering::SeqCst) {
                    if let Err(_e) = capture(source, &device, &ring, &stop) {
                        #[cfg(debug_assertions)]
                        println!("[audio {source:?}] {_e}; retrying");
                        for _ in 0..10 {
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
            #[cfg(not(windows))]
            let _ = (source, device, ring, stop);
        })
        .expect("audio thread")
}

#[cfg(windows)]
fn capture(source: Source, device: &str, ring: &Arc<Mutex<PcmRing>>, stop: &AtomicBool) -> Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
    let e = |x: wasapi::WasapiError| x.to_string();
    let en = DeviceEnumerator::new().map_err(e)?;
    let dir = match source {
        Source::Desktop => Direction::Render,
        Source::Mic => Direction::Capture,
    };
    let follow_default = device.is_empty();
    let dev = if follow_default {
        en.get_default_device(dir).map_err(e)?
    } else {
        // A chosen device that's unplugged: fall back to the default one.
        en.get_device(device).or_else(|_| en.get_default_device(dir)).map_err(e)?
    };
    let dev_id = dev.get_id().map_err(e)?;
    let mut client = dev.get_iaudioclient().map_err(e)?;
    let format = WaveFormat::new(32, 32, SampleType::Float, RATE as usize, CHANNELS, None);
    let mode = StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 2_000_000 };
    // Desktop audio = loopback: a render device opened for capture.
    client.initialize_client(&format, Direction::Capture, mode, None).map_err(e)?;
    let cap = client.get_audiocaptureclient().map_err(e)?;
    client.start_stream().map_err(e)?;

    let mut buf = vec![0u8; RATE as usize * CHANNELS * 4]; // 1 s
    let mut floats: Vec<f32> = Vec::new();
    let mut checks = 0u32;
    let result = loop {
        if stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
        loop {
            match cap.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {}
                Ok(_) => break,
                Err(x) => return Err(e(x)),
            }
            let (frames, info) = cap.read_from_device(&mut buf).map_err(e)?;
            if frames == 0 {
                break;
            }
            let bytes = &buf[..frames as usize * CHANNELS * 4];
            floats.clear();
            floats.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            let t = if info.timestamp > 0 { info.timestamp as i64 } else { super::now_hns() };
            ring.lock().unwrap().push(t, &floats);
        }
        // Follow the Windows default device (e.g. switching to a headset).
        checks += 1;
        if follow_default && checks % 200 == 0 {
            let now_default = en.get_default_device(dir).ok().and_then(|d| d.get_id().ok());
            if now_default.is_some() && now_default.as_deref() != Some(dev_id.as_str()) {
                break Ok(());
            }
        }
    };
    let _ = client.stop_stream();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 10_000; // 1 ms in 100 ns units

    fn tone(frames: usize, v: f32) -> Vec<f32> {
        vec![v; frames * CHANNELS]
    }

    #[test]
    fn continuous_audio_lines_up() {
        let mut r = PcmRing::new(10.0);
        let t0 = 5_000_000_000;
        for i in 0..100 {
            r.push(t0 + i * 10 * MS, &tone(480, 0.5)); // 10 ms packets
        }
        let s = r.read(t0, t0 + 1000 * MS);
        assert_eq!(s.len(), 48_000 * CHANNELS);
        assert!(s.iter().all(|&x| x == 16383));
    }

    #[test]
    fn silence_gaps_are_filled_at_the_right_place() {
        let mut r = PcmRing::new(10.0);
        let t0 = 1_000_000;
        r.push(t0, &tone(4800, 0.25)); // 0–100 ms
        r.push(t0 + 500 * MS, &tone(4800, 0.75)); // 500–600 ms, nothing in between
        let s = r.read(t0, t0 + 600 * MS);
        assert_eq!(s.len(), 28_800 * CHANNELS);
        let at = |ms: i64| s[(ms * 48) as usize * CHANNELS];
        assert_eq!(at(50), 8191);
        assert_eq!(at(300), 0);
        assert_eq!(at(550), 24575);
        // Reading past the end gives silence, not a shorter buffer.
        assert_eq!(r.read(t0 + 550 * MS, t0 + 800 * MS).len(), 12_000 * CHANNELS);
    }

    #[test]
    fn drift_is_corrected() {
        let mut r = PcmRing::new(10.0);
        let t0 = 0;
        r.push(t0, &tone(4800, 0.5));
        // Device says the next packet starts at 20 ms, but we already have 100 ms:
        // the 80 ms overlap is dropped so later audio stays on the clock.
        r.push(t0 + 20 * MS, &tone(9600, 0.9));
        assert_eq!(r.end(), 10_560); // the packet covers 20–220 ms
        // Small jitter (< 50 ms) is simply appended.
        let mut r2 = PcmRing::new(10.0);
        r2.push(0, &tone(4800, 0.5));
        r2.push(80 * MS, &tone(480, 0.5));
        assert_eq!(r2.end(), 5280);
    }

    #[test]
    fn keeps_only_the_window() {
        let mut r = PcmRing::new(1.0);
        for i in 0..300 {
            r.push(i * 10 * MS, &tone(480, 0.1));
        }
        assert_eq!(r.data.len(), 48_000 * CHANNELS);
        assert_eq!(r.base, 300 * 480 - 48_000);
        // Old audio is gone: reading it gives silence.
        assert!(r.read(0, 100 * MS).iter().all(|&x| x == 0));
    }

    #[test]
    fn reading_before_any_audio_is_silent() {
        let r = PcmRing::new(5.0);
        assert_eq!(r.read(0, 500 * MS).len(), 24_000 * CHANNELS);
    }
}
