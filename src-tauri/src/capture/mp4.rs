//! A small MP4 writer for the native engine's clips: H.264/HEVC video from the
//! packet buffer plus AAC audio tracks. Everything is in memory already, so the
//! index ("moov") is written first and the file plays and streams right away.

use super::packets::Snapshot;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct AudioTrack {
    pub title: String,
    pub rate: u32,
    pub channels: u16,
    pub asc: [u8; 2],
    pub frames: Vec<Vec<u8>>,
    pub kbps: u32,
}

const VIDEO_SCALE: u32 = 90_000;

// ---------- boxes ----------

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
    b.extend_from_slice(body);
    bx(kind, &b)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

struct W(Vec<u8>);
impl W {
    fn new() -> Self {
        W(Vec::new())
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.0.extend(std::iter::repeat(0).take(n));
        self
    }
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

// ---------- NAL units ----------

/// NAL units of an Annex B access unit (without start codes).
pub fn nals(au: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && au[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&au[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < au.len() {
            out.push(&au[s..]);
        }
    }
    out
}

fn nal_type(n: &[u8], hevc: bool) -> u8 {
    if hevc { (n[0] >> 1) & 0x3f } else { n[0] & 0x1f }
}

/// Parameter sets, as `(vps, sps, pps)` (vps empty for H.264).
fn params(au: &[u8], hevc: bool) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let (mut vps, mut sps, mut pps) = (Vec::new(), Vec::new(), Vec::new());
    for n in nals(au) {
        if n.is_empty() {
            continue;
        }
        match (hevc, nal_type(n, hevc)) {
            (false, 7) | (true, 33) if sps.is_empty() => sps = n.to_vec(),
            (false, 8) | (true, 34) if pps.is_empty() => pps = n.to_vec(),
            (true, 32) if vps.is_empty() => vps = n.to_vec(),
            _ => {}
        }
    }
    (!sps.is_empty() && !pps.is_empty() && (!hevc || !vps.is_empty())).then_some((vps, sps, pps))
}

/// An access unit as MP4 wants it: 4-byte lengths, without parameter sets or AUDs.
fn sample(au: &[u8], hevc: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for n in nals(au) {
        if n.is_empty() {
            continue;
        }
        let skip = if hevc { matches!(nal_type(n, true), 32 | 33 | 34 | 35) } else { matches!(nal_type(n, false), 7 | 8 | 9) };
        if !skip {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    out
}

fn unescape(n: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(n.len());
    let mut zeros = 0;
    for &b in n {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut w = W::new();
    w.u8(1).u8(sps[1]).u8(sps[2]).u8(sps[3]).u8(0xff).u8(0xe1);
    w.u16(sps.len() as u16).bytes(sps).u8(1).u16(pps.len() as u16).bytes(pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        w.u8(0xfd).u8(0xf8).u8(0xf8).u8(0); // 4:2:0, 8-bit, no SPS extensions
    }
    bx(b"avcC", &w.0)
}

fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let rbsp = unescape(&sps[2..]);
    let b0 = rbsp.first().copied().unwrap_or(0);
    let ptl: Vec<u8> = rbsp.get(1..13).map(|s| s.to_vec()).unwrap_or_else(|| vec![0; 12]);
    let sub_layers = ((b0 >> 1) & 7) + 1;
    let nested = b0 & 1;
    let mut w = W::new();
    w.u8(1).bytes(&ptl[0..12]);
    w.u16(0xf000).u8(0xfc).u8(0xfd).u8(0xf8).u8(0xf8).u16(0);
    w.u8((sub_layers << 3) | (nested << 2) | 3);
    w.u8(3);
    for (t, n) in [(32u8, vps), (33, sps), (34, pps)] {
        w.u8(0x80 | t).u16(1).u16(n.len() as u16).bytes(n);
    }
    bx(b"hvcC", &w.0)
}

fn esds(t: &AudioTrack) -> Vec<u8> {
    let desc = |tag: u8, body: &[u8]| {
        let mut v = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
        v.extend_from_slice(body);
        v
    };
    let dsi = desc(0x05, &t.asc);
    let mut dcd = W::new();
    let bitrate = t.kbps * 1000;
    dcd.u8(0x40).u8(0x15).u8(0).u16(0x1800).u32(bitrate).u32(bitrate).bytes(&dsi);
    let dcd = desc(0x04, &dcd.0);
    let sl = desc(0x06, &[2]);
    let mut es = W::new();
    es.u16(1).u8(0).bytes(&dcd).bytes(&sl);
    full(b"esds", 0, 0, &desc(0x03, &es.0))
}

// ---------- tracks ----------

struct Track {
    handler: [u8; 4],
    title: String,
    scale: u32,
    durations: Vec<u32>,
    sizes: Vec<u32>,
    sync: Option<Vec<u32>>, // 1-based sample numbers; None = all
    stsd_entry: Vec<u8>,
    width: u32,
    height: u32,
    /// (chunk start sample, sample count) once laid out.
    chunks: Vec<(usize, usize)>,
    offsets: Vec<u64>,
}

impl Track {
    fn duration(&self) -> u64 {
        self.durations.iter().map(|&d| d as u64).sum()
    }
}

fn stbl(t: &Track) -> Vec<u8> {
    // stts: runs of equal durations
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in &t.durations {
        match runs.last_mut() {
            Some((n, v)) if *v == d => *n += 1,
            _ => runs.push((1, d)),
        }
    }
    let mut stts = W::new();
    stts.u32(runs.len() as u32);
    for (n, d) in &runs {
        stts.u32(*n).u32(*d);
    }
    let mut stsc = W::new();
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, n)) in t.chunks.iter().enumerate() {
        if entries.last().map(|e| e.1) != Some(n as u32) {
            entries.push((i as u32 + 1, n as u32));
        }
    }
    stsc.u32(entries.len() as u32);
    for (first, n) in entries {
        stsc.u32(first).u32(n).u32(1);
    }
    let mut stsz = W::new();
    stsz.u32(0).u32(t.sizes.len() as u32);
    for s in &t.sizes {
        stsz.u32(*s);
    }
    let mut co64 = W::new();
    co64.u32(t.offsets.len() as u32);
    for o in &t.offsets {
        co64.u64(*o);
    }
    let mut parts = vec![
        full(b"stsd", 0, 0, &[&1u32.to_be_bytes()[..], &t.stsd_entry].concat()),
        full(b"stts", 0, 0, &stts.0),
    ];
    if let Some(sync) = &t.sync {
        let mut s = W::new();
        s.u32(sync.len() as u32);
        for n in sync {
            s.u32(*n);
        }
        parts.push(full(b"stss", 0, 0, &s.0));
    }
    parts.push(full(b"stsc", 0, 0, &stsc.0));
    parts.push(full(b"stsz", 0, 0, &stsz.0));
    parts.push(full(b"co64", 0, 0, &co64.0));
    bx(b"stbl", &cat(&parts))
}

fn trak(t: &Track, id: u32, movie_dur_ms: u64, enabled: bool, alt_group: u16) -> Vec<u8> {
    let video = &t.handler == b"vide";
    let mut tkhd = W::new();
    tkhd.u32(0).u32(0).u32(id).u32(0).u32(movie_dur_ms as u32).zeros(8).u16(0).u16(alt_group).u16(if video { 0 } else { 0x0100 }).u16(0);
    for m in MATRIX {
        tkhd.u32(m);
    }
    tkhd.u32(t.width << 16).u32(t.height << 16);
    let mut mdhd = W::new();
    mdhd.u32(0).u32(0).u32(t.scale).u32(t.duration() as u32).u16(0x55c4).u16(0); // language "und"
    let mut hdlr = W::new();
    hdlr.u32(0).bytes(&t.handler).zeros(12).bytes(t.title.as_bytes()).u8(0);
    let media_header = if video { full(b"vmhd", 0, 1, &[0; 8]) } else { full(b"smhd", 0, 0, &[0; 4]) };
    let dref = full(b"dref", 0, 0, &[&1u32.to_be_bytes()[..], &full(b"url ", 0, 1, &[])].concat());
    let minf = bx(b"minf", &cat(&[media_header, bx(b"dinf", &dref), stbl(t)]));
    let mdia = bx(b"mdia", &cat(&[full(b"mdhd", 0, 0, &mdhd.0), full(b"hdlr", 0, 0, &hdlr.0), minf]));
    let mut parts = vec![full(b"tkhd", 0, if enabled { 3 } else { 2 }, &tkhd.0), mdia];
    if !t.title.is_empty() && !video {
        // Track title, as ffmpeg writes it ("Game + mic", "Desktop", "Microphone").
        let mut name = W::new();
        name.bytes(t.title.as_bytes());
        parts.push(bx(b"udta", &bx(b"name", &name.0)));
    }
    bx(b"trak", &cat(&parts))
}

/// Writes the clip. Returns the file size.
pub fn write(path: &Path, video: &Snapshot, audio: &[AudioTrack]) -> Result<u64, String> {
    let hevc = video.format.codec == 1;
    let first = video.packets.first().ok_or("No video to save")?;
    let (vps, sps, pps) = params(&first.data, hevc).ok_or("The first frame has no stream header")?;
    let samples: Vec<Vec<u8>> = video.packets.iter().map(|p| sample(&p.data, hevc)).collect();
    let fd = super::packets::frame_hns(video.format.fps);
    let to_v = |hns: i64| (hns.max(0) as u128 * VIDEO_SCALE as u128 / 10_000_000) as u32;
    let durations: Vec<u32> = video
        .packets
        .windows(2)
        .map(|w| to_v(w[1].t - w[0].t).max(1))
        .chain(std::iter::once(to_v(fd).max(1)))
        .collect();
    let sync: Vec<u32> = video.packets.iter().enumerate().filter(|(_, p)| p.key).map(|(i, _)| i as u32 + 1).collect();
    let (w, h) = (video.format.width, video.format.height);

    let mut entry = W::new();
    entry.zeros(6).u16(1).zeros(16).u16(w as u16).u16(h as u16).u32(0x0048_0000).u32(0x0048_0000).u32(0).u16(1).zeros(32).u16(0x18).u16(0xffff);
    entry.bytes(&if hevc { hvcc(&vps, &sps, &pps) } else { avcc(&sps, &pps) });
    let mut tracks = vec![Track {
        handler: *b"vide",
        title: "Video".into(),
        scale: VIDEO_SCALE,
        durations,
        sizes: samples.iter().map(|s| s.len() as u32).collect(),
        sync: Some(sync),
        stsd_entry: bx(if hevc { b"hvc1" } else { b"avc1" }, &entry.0),
        width: w,
        height: h,
        chunks: Vec::new(),
        offsets: Vec::new(),
    }];
    for a in audio {
        let mut e = W::new();
        e.zeros(6).u16(1).zeros(8).u16(a.channels).u16(16).u16(0).u16(0).u32(a.rate << 16);
        e.bytes(&esds(a));
        tracks.push(Track {
            handler: *b"soun",
            title: a.title.clone(),
            scale: a.rate,
            durations: vec![1024; a.frames.len()],
            sizes: a.frames.iter().map(|f| f.len() as u32).collect(),
            sync: None,
            stsd_entry: bx(b"mp4a", &e.0),
            width: 0,
            height: 0,
            chunks: Vec::new(),
            offsets: Vec::new(),
        });
    }

    // Interleave: half a second of each track at a time.
    let mut order: Vec<(usize, usize, usize)> = Vec::new(); // (track, first sample, count)
    let mut next = vec![0usize; tracks.len()];
    let mut elapsed = vec![0u64; tracks.len()];
    let mut window = 0.5f64;
    loop {
        let mut any = false;
        for (ti, t) in tracks.iter().enumerate() {
            let start = next[ti];
            let mut n = 0;
            while start + n < t.durations.len() && (elapsed[ti] as f64 / t.scale as f64) < window {
                elapsed[ti] += t.durations[start + n] as u64;
                n += 1;
            }
            if n > 0 {
                order.push((ti, start, n));
                next[ti] += n;
                any = true;
            }
        }
        if !any && next.iter().zip(&tracks).all(|(n, t)| *n >= t.durations.len()) {
            break;
        }
        window += 0.5;
    }
    for &(ti, start, n) in &order {
        tracks[ti].chunks.push((start, n));
        tracks[ti].offsets.push(0);
    }

    let movie_ms = tracks.iter().map(|t| t.duration() * 1000 / t.scale as u64).max().unwrap_or(0);
    let ftyp = bx(b"ftyp", &cat(&[b"isom".to_vec(), 512u32.to_be_bytes().to_vec(), b"isomiso2avc1mp41".to_vec()]));
    let build_moov = |tracks: &[Track]| {
        let mut mvhd = W::new();
        mvhd.u32(0).u32(0).u32(1000).u32(movie_ms as u32).u32(0x0001_0000).u16(0x0100).zeros(10);
        for m in MATRIX {
            mvhd.u32(m);
        }
        mvhd.zeros(24).u32(tracks.len() as u32 + 1);
        let mut parts = vec![full(b"mvhd", 0, 0, &mvhd.0)];
        let mut first_audio = true;
        for (i, t) in tracks.iter().enumerate() {
            let audio = &t.handler == b"soun";
            parts.push(trak(t, i as u32 + 1, movie_ms, !audio || first_audio, if audio { 1 } else { 0 }));
            if audio {
                first_audio = false;
            }
        }
        bx(b"moov", &cat(&parts))
    };
    // Offsets depend on the moov's size, which doesn't depend on the offsets' values.
    let moov_len = build_moov(&tracks).len() as u64;
    let mut pos = ftyp.len() as u64 + moov_len + 16;
    let mut chunk_no = vec![0usize; tracks.len()];
    for &(ti, start, n) in &order {
        tracks[ti].offsets[chunk_no[ti]] = pos;
        chunk_no[ti] += 1;
        pos += tracks[ti].sizes[start..start + n].iter().map(|&s| s as u64).sum::<u64>();
    }
    let moov = build_moov(&tracks);
    let mdat_len = pos - ftyp.len() as u64 - moov.len() as u64;

    let file = std::fs::File::create(path).map_err(|e| format!("Couldn't create the clip: {e}"))?;
    let mut f = BufWriter::with_capacity(4 << 20, file);
    let io = |e: std::io::Error| format!("Couldn't write the clip: {e}");
    f.write_all(&ftyp).map_err(io)?;
    f.write_all(&moov).map_err(io)?;
    f.write_all(&1u32.to_be_bytes()).map_err(io)?;
    f.write_all(b"mdat").map_err(io)?;
    f.write_all(&mdat_len.to_be_bytes()).map_err(io)?;
    for &(ti, start, n) in &order {
        for s in start..start + n {
            if ti == 0 {
                f.write_all(&samples[s]).map_err(io)?;
            } else {
                f.write_all(&audio[ti - 1].frames[s]).map_err(io)?;
            }
        }
    }
    let file = f.into_inner().map_err(|e| format!("Couldn't write the clip: {e}"))?;
    file.sync_data().map_err(io)?;
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nals_and_converts() {
        let au = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 9];
        let n = nals(&au);
        assert_eq!(n.len(), 4);
        assert_eq!(n[1], &[0x67, 1, 2]);
        let (_, sps, pps) = params(&au, false).unwrap();
        assert_eq!((sps.as_slice(), pps.as_slice()), (&[0x67u8, 1, 2][..], &[0x68u8, 3][..]));
        assert_eq!(sample(&au, false), vec![0, 0, 0, 4, 0x65, 9, 9, 9]);
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
    }
}

#[cfg(test)]
mod real {
    use super::*;
    use crate::capture::native;
    use crate::capture::packets::{Format, Ring};
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    /// Captures 4 s natively, adds a tone track, writes an MP4 and decodes it with ffmpeg.
    /// cargo test mp4_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn mp4_real() {
        for hevc in [false, true] {
            let mon = crate::capture::game::monitors().first().map(|m| m.handle).unwrap();
            let ring = Arc::new(Mutex::new(Ring::default()));
            ring.lock().unwrap().set_format(Format { codec: hevc as u32, width: 1920, height: 1080, fps: 60 });
            let e = native::Engine::start(
                native::Options { hwnd: 0, hmonitor: mon, width: 1920, height: 1080, fps: 60, kbps: 20000, max_kbps: 30000, hevc, cursor: true, tap: None },
                ring.clone(),
                Arc::new(AtomicU32::new(30)),
                usize::MAX,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_secs(4));
            drop(e);
            let snap = ring.lock().unwrap().snapshot_since(60.0, None).unwrap();
            let secs = (snap.end_hns() - snap.start_hns()) as f64 / 1e7;
            let frames = (secs * 48_000.0) as usize;
            let pcm: Vec<i16> = (0..frames).flat_map(|i| { let v = ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16; [v, v] }).collect();
            let t0 = std::time::Instant::now();
            let a = native::aac(&pcm, 2, 48_000, 192).unwrap();
            let b = native::aac(&pcm, 2, 48_000, 192).unwrap();
            let tracks = [
                AudioTrack { title: "Game + mic".into(), rate: 48_000, channels: 2, asc: a.asc, frames: a.frames, kbps: 192 },
                AudioTrack { title: "Desktop".into(), rate: 48_000, channels: 2, asc: b.asc, frames: b.frames, kbps: 192 },
            ];
            let out = std::env::temp_dir().join(format!("clipit-native-{}.mp4", if hevc { "hevc" } else { "h264" }));
            let size = write(&out, &snap, &tracks).unwrap();
            println!("{} {:.2}s {} KB, audio+write took {:?}", out.display(), secs, size / 1024, t0.elapsed());
            let ff = crate::media::find_ffmpeg().unwrap().path;
            let o = std::process::Command::new(&ff).args(["-hide_banner", "-v", "error", "-i"]).arg(&out).args(["-f", "null", "-"]).output().unwrap();
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(o.status.success() && err.trim().is_empty(), "{err}");
            let info = std::process::Command::new(&ff).args(["-hide_banner", "-i"]).arg(&out).output().unwrap();
            let text = String::from_utf8_lossy(&info.stderr);
            for l in text.lines().filter(|l| l.contains("Duration") || l.contains("Stream") || l.contains("title")) {
                println!("{l}");
            }
            assert_eq!(text.matches("Audio: aac").count(), 2);
            let _ = std::fs::remove_file(&out);
        }
    }
}

#[cfg(test)]
mod sync {
    use super::*;
    use crate::capture::native;
    use crate::capture::packets::{Format, Ring};
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    /// Where a click put 0.5 s into the audio ends up in the saved clip.
    /// cargo test aac_delay_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn aac_delay_real() {
        let mon = crate::capture::game::monitors().first().map(|m| m.handle).unwrap();
        let ring = Arc::new(Mutex::new(Ring::default()));
        ring.lock().unwrap().set_format(Format { codec: 0, width: 1280, height: 720, fps: 60 });
        let e = native::Engine::start(
            native::Options { hwnd: 0, hmonitor: mon, width: 1280, height: 720, fps: 60, kbps: 8000, max_kbps: 12000, hevc: false, cursor: false, tap: None },
            ring.clone(), Arc::new(AtomicU32::new(30)), usize::MAX).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(e);
        let snap = ring.lock().unwrap().snapshot_since(60.0, None).unwrap();
        let frames = 48_000 * 2;
        let mut pcm = vec![0i16; frames * 2];
        for i in 24_000..24_048 { pcm[2 * i] = 30_000; pcm[2 * i + 1] = 30_000; }
        let a = native::aac(&pcm, 2, 48_000, 192).unwrap();
        let out = std::env::temp_dir().join("clipit-sync.mp4");
        write(&out, &snap, &[AudioTrack { title: "x".into(), rate: 48_000, channels: 2, asc: a.asc, frames: a.frames, kbps: 192 }]).unwrap();
        let ff = crate::media::find_ffmpeg().unwrap().path;
        let o = std::process::Command::new(&ff).args(["-hide_banner", "-v", "error", "-i"]).arg(&out).args(["-map", "0:a:0", "-f", "s16le", "-ac", "1", "-"]).output().unwrap();
        let s: Vec<i16> = o.stdout.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        let peak = s.iter().enumerate().max_by_key(|(_, v)| v.unsigned_abs()).map(|(i, _)| i).unwrap();
        println!("click at sample 24000 -> {peak} in the clip ({:+.1} ms)", (peak as f64 - 24_000.0) / 48.0);
        let _ = std::fs::remove_file(&out);
    }
}
