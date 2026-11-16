//! The replay buffer: encoded video kept in RAM as MPEG-TS, split at keyframes.
//!
//! ffmpeg writes a video-only MPEG-TS stream to its stdout. We cut it into
//! "chunks" that each start at a keyframe, keep the last N seconds of chunks and
//! drop older ones. Saving a clip = PAT + PMT + the chunks, handed to a muxer.
//! Nothing touches the disk until a clip is saved.

use std::collections::VecDeque;
use std::sync::Arc;

pub const PACKET: usize = 188;
const PTS_HZ: i64 = 90_000;

/// One keyframe-to-keyframe piece of video.
#[derive(Clone)]
pub struct Chunk {
    /// PTS of the keyframe, 90 kHz, unwrapped.
    pub pts: i64,
    /// PTS of the last frame in the chunk.
    pub last_pts: i64,
    pub bytes: Arc<Vec<u8>>,
}

/// Everything needed to write a clip: headers + chunks + timing.
pub struct Snapshot {
    pub bytes: Vec<Arc<Vec<u8>>>,
    pub header: Vec<u8>,
    pub start_pts: i64,
    pub end_pts: i64,
    /// Clock offset: QPC time (100 ns) = pts * 1e7 / 90000 + offset_hns.
    pub offset_hns: i64,
    pub frame_hns: i64,
}

impl Snapshot {
    pub fn start_hns(&self) -> i64 {
        pts_to_hns(self.start_pts) + self.offset_hns
    }
    /// End of the last frame.
    pub fn end_hns(&self) -> i64 {
        pts_to_hns(self.end_pts) + self.offset_hns + self.frame_hns
    }
}

pub fn pts_to_hns(pts: i64) -> i64 {
    pts * 10_000_000 / PTS_HZ
}

#[derive(Default)]
pub struct Ring {
    /// Bytes not yet forming a whole packet.
    partial: Vec<u8>,
    pat: Option<Vec<u8>>,
    pmt: Option<Vec<u8>>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    chunks: VecDeque<Chunk>,
    open: Option<Chunk>,
    open_bytes: Vec<u8>,
    last_raw_pts: Option<i64>,
    pts_base: i64,
    /// Smallest (arrival time - pts) seen: maps PTS to the QPC clock.
    offset_hns: Option<i64>,
    pub frame_hns: i64,
    total: usize,
    /// Live recording: every video packet, with (starts a keyframe, unwrapped PTS).
    pub tee: Option<Box<dyn FnMut(&[u8], bool, Option<i64>) + Send>>,
}

impl Ring {
    pub fn new(frame_hns: i64) -> Self {
        Self { frame_hns, ..Default::default() }
    }

    /// Feeds raw stdout bytes from ffmpeg. `now_hns` = QPC time of arrival.
    pub fn feed(&mut self, data: &[u8], now_hns: i64) {
        self.partial.extend_from_slice(data);
        let mut start = 0;
        while self.partial.len() - start >= PACKET {
            if self.partial[start] != 0x47 {
                start += 1; // lost sync: skip to the next 0x47
                continue;
            }
            let pkt: [u8; PACKET] = self.partial[start..start + PACKET].try_into().unwrap();
            self.packet(&pkt, now_hns);
            start += PACKET;
        }
        self.partial.drain(..start);
    }

    fn packet(&mut self, p: &[u8; PACKET], now_hns: i64) {
        let pid = (((p[1] & 0x1f) as u16) << 8) | p[2] as u16;
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 3;
        let mut payload = 4;
        let mut random_access = false;
        if afc & 2 != 0 {
            let len = p[4] as usize;
            if len > 0 {
                random_access = p[5] & 0x40 != 0;
            }
            payload = 5 + len;
        }
        if afc & 1 == 0 || payload >= PACKET {
            payload = PACKET;
        }

        if pid == 0 {
            if pusi {
                self.pmt_pid = parse_pat(&p[payload..]);
            }
            self.pat = Some(p.to_vec());
            return;
        }
        if Some(pid) == self.pmt_pid {
            if pusi {
                if let Some(v) = parse_pmt(&p[payload..]) {
                    self.video_pid = Some(v);
                }
            }
            self.pmt = Some(p.to_vec());
            return;
        }
        if Some(pid) != self.video_pid {
            return; // SDT and anything else: not needed
        }
        let mut pts = None;
        if pusi {
            if let Some(raw) = parse_pes_pts(&p[payload..]) {
                pts = Some(self.unwrap_pts(raw));
            }
        }
        if let Some(tee) = self.tee.as_mut() {
            tee(p, pusi && random_access, pts);
        }
        if let Some(pts) = pts {
            let diff = now_hns - pts_to_hns(pts);
            self.offset_hns = Some(self.offset_hns.map_or(diff, |o| o.min(diff)));
        }

        if pusi && random_access {
            // A keyframe starts a new chunk.
            self.close_open();
            let pts = pts.unwrap_or(0);
            self.open = Some(Chunk { pts, last_pts: pts, bytes: Arc::new(Vec::new()) });
        } else if let (Some(pts), Some(open)) = (pts, self.open.as_mut()) {
            open.last_pts = open.last_pts.max(pts);
        }
        if self.open.is_some() {
            self.open_bytes.extend_from_slice(p);
        }
        // else: before the first keyframe — undecodable, drop it
    }

    fn unwrap_pts(&mut self, raw: i64) -> i64 {
        const WRAP: i64 = 1 << 33;
        if let Some(last) = self.last_raw_pts {
            if raw < last && last - raw > WRAP / 2 {
                self.pts_base += WRAP;
            }
        }
        self.last_raw_pts = Some(raw);
        raw + self.pts_base
    }

    fn close_open(&mut self) {
        if let Some(mut c) = self.open.take() {
            c.bytes = Arc::new(std::mem::take(&mut self.open_bytes));
            self.total += c.bytes.len();
            self.chunks.push_back(c);
        }
    }

    /// Drops chunks so that at most `keep_secs` (+ one chunk) and `max_bytes` remain.
    pub fn trim(&mut self, keep_secs: f64, max_bytes: usize) {
        let newest = self.newest_pts();
        let keep = (keep_secs * PTS_HZ as f64) as i64;
        while self.chunks.len() > 1 {
            // Keep the oldest chunk while the *next* one starts after the window start,
            // so a full `keep_secs` is always available.
            let too_old = newest - self.chunks[1].pts >= keep;
            let too_big = self.total > max_bytes;
            if !(too_old || too_big) {
                break;
            }
            let c = self.chunks.pop_front().unwrap();
            self.total -= c.bytes.len();
        }
    }

    fn newest_pts(&self) -> i64 {
        self.open
            .as_ref()
            .map(|c| c.last_pts)
            .or_else(|| self.chunks.back().map(|c| c.last_pts))
            .unwrap_or(0)
    }

    /// Bytes held in memory.
    pub fn bytes(&self) -> usize {
        self.total + self.open_bytes.len()
    }

    /// Seconds of video currently buffered.
    pub fn buffered_secs(&self) -> f64 {
        match self.chunks.front().or(self.open.as_ref()) {
            Some(first) => (self.newest_pts() - first.pts) as f64 / PTS_HZ as f64,
            None => 0.0,
        }
    }

    /// QPC time (100 ns) the newest buffered frame was captured at.
    pub fn newest_hns(&self) -> Option<i64> {
        let offset = self.offset_hns?;
        self.open.as_ref().or(self.chunks.back())?;
        Some(pts_to_hns(self.newest_pts()) + offset + self.frame_hns)
    }

    /// The last `secs` seconds (starting at a keyframe), or None if nothing is buffered.
    pub fn snapshot(&self, secs: f64) -> Option<Snapshot> {
        self.snapshot_since(secs, None)
    }

    /// Like `snapshot`, but never starting before `not_before_hns` (QPC): the
    /// keyframe at or just before it, so nothing already clipped is repeated
    /// beyond that one keyframe interval.
    pub fn snapshot_since(&self, secs: f64, not_before_hns: Option<i64>) -> Option<Snapshot> {
        let header = [self.pat.as_ref()?.as_slice(), self.pmt.as_ref()?.as_slice()].concat();
        let mut all: Vec<&Chunk> = self.chunks.iter().collect();
        let open_copy;
        if let Some(open) = &self.open {
            if !self.open_bytes.is_empty() {
                open_copy = Chunk { bytes: Arc::new(self.open_bytes.clone()), ..open.clone() };
                all.push(&open_copy);
            }
        }
        let last = *all.last()?;
        let mut want_start = last.last_pts - (secs * PTS_HZ as f64) as i64;
        if let (Some(floor), Some(offset)) = (not_before_hns, self.offset_hns) {
            want_start = want_start.max((floor - offset) * PTS_HZ / 10_000_000);
        }
        // Latest keyframe at or before the wanted start; else the oldest we have.
        let first = all.iter().rposition(|c| c.pts <= want_start).unwrap_or(0);
        let picked = &all[first..];
        Some(Snapshot {
            bytes: picked.iter().map(|c| c.bytes.clone()).collect(),
            header,
            start_pts: picked[0].pts,
            end_pts: last.last_pts,
            offset_hns: self.offset_hns.unwrap_or(0),
            frame_hns: self.frame_hns,
        })
    }

    /// The video between two clock times (QPC, 100 ns): from the keyframe at or
    /// before `start_hns` to the end of the chunk holding `end_hns`. Used for kill
    /// clips, which end a while before they're saved.
    pub fn snapshot_range(&self, start_hns: i64, end_hns: i64) -> Option<Snapshot> {
        let header = [self.pat.as_ref()?.as_slice(), self.pmt.as_ref()?.as_slice()].concat();
        let offset = self.offset_hns?;
        let to_pts = |hns: i64| (hns - offset) * PTS_HZ / 10_000_000;
        let (want_start, want_end) = (to_pts(start_hns), to_pts(end_hns));
        let mut all: Vec<&Chunk> = self.chunks.iter().collect();
        let open_copy;
        if let Some(open) = &self.open {
            if !self.open_bytes.is_empty() {
                open_copy = Chunk { bytes: Arc::new(self.open_bytes.clone()), ..open.clone() };
                all.push(&open_copy);
            }
        }
        if all.is_empty() {
            return None;
        }
        let first = all.iter().rposition(|c| c.pts <= want_start).unwrap_or(0);
        // Last chunk that starts at or before the wanted end.
        let last = all.iter().rposition(|c| c.pts <= want_end).unwrap_or(first).max(first);
        let picked = &all[first..=last];
        Some(Snapshot {
            bytes: picked.iter().map(|c| c.bytes.clone()).collect(),
            header,
            start_pts: picked[0].pts,
            end_pts: picked[picked.len() - 1].last_pts,
            offset_hns: offset,
            frame_hns: self.frame_hns,
        })
    }

    /// PAT + PMT packets (needed at the start of any file we write).
    pub fn header(&self) -> Option<Vec<u8>> {
        Some([self.pat.as_ref()?.as_slice(), self.pmt.as_ref()?.as_slice()].concat())
    }

    pub fn offset_hns(&self) -> Option<i64> {
        self.offset_hns
    }
}

/// PMT PID of the first program in a PAT section.
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let ptr = *payload.first()? as usize;
    let s = payload.get(1 + ptr..)?;
    if *s.first()? != 0x00 {
        return None;
    }
    let section_len = (((s[1] & 0x0f) as usize) << 8) | s[2] as usize;
    let end = (3 + section_len).saturating_sub(4).min(s.len()); // minus CRC
    let mut i = 8;
    while i + 4 <= end {
        let program = ((s[i] as u16) << 8) | s[i + 1] as u16;
        let pid = (((s[i + 2] & 0x1f) as u16) << 8) | s[i + 3] as u16;
        if program != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

/// PID of the first elementary stream in a PMT section (our stream is video-only).
fn parse_pmt(payload: &[u8]) -> Option<u16> {
    let ptr = *payload.first()? as usize;
    let s = payload.get(1 + ptr..)?;
    if *s.first()? != 0x02 {
        return None;
    }
    let section_len = (((s[1] & 0x0f) as usize) << 8) | s[2] as usize;
    let end = (3 + section_len).saturating_sub(4).min(s.len());
    let prog_info_len = (((*s.get(10)? & 0x0f) as usize) << 8) | *s.get(11)? as usize;
    let i = 12 + prog_info_len;
    if i + 5 > end {
        return None;
    }
    Some((((s[i + 1] & 0x1f) as u16) << 8) | s[i + 2] as u16)
}

/// PTS from the start of a PES packet.
fn parse_pes_pts(p: &[u8]) -> Option<i64> {
    if p.len() < 14 || p[0] != 0 || p[1] != 0 || p[2] != 1 {
        return None;
    }
    if p[7] & 0x80 == 0 {
        return None; // no PTS
    }
    let b = &p[9..14];
    Some(
        (((b[0] as i64 >> 1) & 0x07) << 30)
            | ((b[1] as i64) << 22)
            | (((b[2] as i64) >> 1) << 15)
            | ((b[3] as i64) << 7)
            | ((b[4] as i64) >> 1),
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn pkt(pid: u16, pusi: bool, random_access: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x47, ((pid >> 8) as u8 & 0x1f) | if pusi { 0x40 } else { 0 }, pid as u8];
        if random_access {
            p.push(0x30); // adaptation + payload
            p.push(1);
            p.push(0x40);
        } else {
            p.push(0x10);
        }
        p.extend_from_slice(payload);
        p.resize(PACKET, 0xff);
        p
    }

    fn pes(pts: i64) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5];
        v.push(0x21 | (((pts >> 30) & 7) << 1) as u8);
        v.push((pts >> 22) as u8);
        v.push(0x01 | (((pts >> 15) & 0x7f) << 1) as u8);
        v.push((pts >> 7) as u8);
        v.push(0x01 | ((pts & 0x7f) << 1) as u8);
        v
    }

    fn pat() -> Vec<u8> {
        // pointer, table 0, len 13, tsid, ver, sec, last, program 1 -> PMT 0x1000, CRC
        pkt(0, true, false, &[0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00, 1, 2, 3, 4])
    }

    fn pmt() -> Vec<u8> {
        // table 2, len 18, prog 1, ver, sec, last, PCR PID 0x100, prog info 0, stream 0x1b PID 0x100
        pkt(0x1000, true, false, &[0, 0x02, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0x00, 0x1b, 0xe1, 0x00, 0xf0, 0x00, 1, 2, 3, 4])
    }

    /// A fake stream: `secs` seconds of 60 fps video with a keyframe every second.
    pub fn stream(secs: i64, start_pts: i64) -> Vec<u8> {
        let mut out = pat();
        out.extend(pmt());
        for f in 0..secs * 60 {
            let pts = start_pts + f * 1500;
            out.extend(pkt(0x100, true, f % 60 == 0, &pes(pts)));
            out.extend(pkt(0x100, false, false, &[0xaa; 50]));
        }
        out
    }

    #[test]
    fn pes_pts_roundtrip() {
        for pts in [0i64, 1, 90_000, (1 << 33) - 1, 123_456_789] {
            assert_eq!(parse_pes_pts(&pes(pts)), Some(pts));
        }
    }

    #[test]
    fn chunks_start_at_keyframes() {
        let mut r = Ring::new(166_667);
        // Feed in odd-sized pieces to exercise reassembly.
        let s = stream(10, 126_000);
        for (i, piece) in s.chunks(1000).enumerate() {
            r.feed(piece, 50_000_000 + i as i64);
        }
        assert_eq!(r.video_pid, Some(0x100));
        assert!((r.buffered_secs() - 9.983).abs() < 0.01, "{}", r.buffered_secs());
        let snap = r.snapshot(3.0).unwrap();
        assert_eq!(snap.header.len(), 2 * PACKET);
        // last frame at 9.983 s; wanted start 6.983 -> keyframe at 6 s
        assert_eq!(snap.start_pts, 126_000 + 6 * 90_000);
        assert_eq!(snap.end_pts, 126_000 + (600 - 1) * 1500);
        assert_eq!(snap.bytes.len(), 4); // 6,7,8 + open 9
        let first = &snap.bytes[0];
        assert_eq!(first[0], 0x47);
        assert_eq!(first[3] & 0x20, 0x20); // starts with the keyframe packet
    }

    #[test]
    fn range_snapshots() {
        let mut r = Ring::new(166_667);
        // Arrival exactly at PTS time + 1 s, so offset = 1 s.
        let s = stream(30, 0);
        r.feed(&s, 10_000_000);
        let off = r.offset_hns().unwrap();
        let at = |secs: f64| off + (secs * 1e7) as i64;
        let snap = r.snapshot_range(at(10.5), at(20.2)).unwrap();
        assert_eq!(snap.start_pts, 10 * 90_000, "starts at the keyframe before 10.5 s");
        assert_eq!(snap.end_pts, 20 * 90_000 + 59 * 1500, "ends with the chunk holding 20.2 s");
        assert_eq!(snap.bytes.len(), 11);
        // Asking for more than is buffered gives what's there.
        let all = r.snapshot_range(at(-50.0), at(500.0)).unwrap();
        assert_eq!(all.start_pts, 0);
        assert_eq!(all.end_pts, 29 * 90_000 + 59 * 1500);
    }

    #[test]
    fn clips_dont_repeat_and_know_the_newest_frame() {
        let mut r = Ring::new(166_667);
        r.feed(&stream(60, 0), 10_000_000);
        let off = r.offset_hns().unwrap();
        let at = |secs: f64| off + (secs * 1e7) as i64;
        // newest frame ends at 59.983 + one frame
        assert_eq!(r.newest_hns(), Some(at(59.0) + pts_to_hns(59 * 1500) + 166_667));
        // A 60 s clip, but the last one ended at 50.4 s: starts at the keyframe at 50 s.
        let snap = r.snapshot_since(60.0, Some(at(50.4))).unwrap();
        assert_eq!(snap.start_pts, 50 * 90_000);
        // No previous clip: the whole minute.
        assert_eq!(r.snapshot_since(60.0, None).unwrap().start_pts, 0);
        // A floor older than the window changes nothing.
        assert_eq!(r.snapshot_since(10.0, Some(at(5.0))).unwrap().start_pts, 49 * 90_000);
        assert_eq!(Ring::new(1).newest_hns(), None);
    }

    #[test]
    fn trims_to_window_and_size() {
        let mut r = Ring::new(166_667);
        r.feed(&stream(20, 0), 0);
        r.trim(5.0, usize::MAX);
        let secs = r.buffered_secs();
        assert!(secs >= 5.0 && secs < 7.0, "{secs}");
        let snap = r.snapshot(5.0).unwrap();
        assert!(snap.end_pts - snap.start_pts >= 5 * 90_000);
        r.trim(60.0, 200_000);
        assert!(r.bytes() <= 200_000 + 130 * PACKET);
    }

    #[test]
    fn maps_pts_to_clock() {
        let mut r = Ring::new(166_667);
        let s = stream(2, 90_000);
        // arrival = pts time + 20 ms latency (+ jitter on some)
        let mut fed = 0;
        for f in 0..120i64 {
            let piece = &s[fed..fed + if f == 0 { 4 * PACKET } else { 2 * PACKET }];
            fed += piece.len();
            let jitter = if f % 7 == 0 { 50_000 } else { 0 };
            r.feed(piece, pts_to_hns(90_000 + f * 1500) + 200_000 + jitter + 1_000_000_000);
        }
        assert_eq!(r.offset_hns(), Some(200_000 + 1_000_000_000));
    }

    #[test]
    fn pts_wraps() {
        let mut r = Ring::new(166_667);
        let near = (1i64 << 33) - 90_000;
        let mut s = pat();
        s.extend(pmt());
        s.extend(pkt(0x100, true, true, &pes(near)));
        s.extend(pkt(0x100, true, true, &pes(1000)));
        r.feed(&s, 0);
        let snap = r.snapshot(100.0).unwrap();
        assert_eq!(snap.end_pts, (1i64 << 33) + 1000);
    }
}
