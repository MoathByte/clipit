//! The native engine's replay buffer: encoded video packets (Annex B) with their
//! time on the QPC clock, kept in RAM and cut at keyframes.

use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone)]
pub struct Packet {
    /// QPC time, 100 ns.
    pub t: i64,
    pub key: bool,
    pub data: Arc<Vec<u8>>,
}

/// What the packets are: they're only mixed when these match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Format {
    /// 0 = H.264, 1 = HEVC.
    pub codec: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Default)]
pub struct Ring {
    pub format: Format,
    packets: VecDeque<Packet>,
    bytes: usize,
}

pub struct Snapshot {
    pub format: Format,
    pub packets: Vec<Packet>,
}

impl Snapshot {
    pub fn start_hns(&self) -> i64 {
        self.packets.first().map(|p| p.t).unwrap_or(0)
    }
    /// End of the last frame.
    pub fn end_hns(&self) -> i64 {
        self.packets.last().map(|p| p.t + frame_hns(self.format.fps)).unwrap_or(0)
    }
}

pub fn frame_hns(fps: u32) -> i64 {
    10_000_000 / fps.max(1) as i64
}

impl Ring {
    /// Empties the buffer if the new format can't be joined to what's in it.
    pub fn set_format(&mut self, f: Format) {
        if self.format != f {
            self.packets.clear();
            self.bytes = 0;
            self.format = f;
        }
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
    }

    pub fn push(&mut self, p: Packet) {
        // Nothing before the first keyframe is decodable.
        if self.packets.is_empty() && !p.key {
            return;
        }
        // Time never runs backwards (a restarted encoder): drop the odd one.
        if let Some(last) = self.packets.back() {
            if p.t <= last.t {
                return;
            }
        }
        self.bytes += p.data.len();
        self.packets.push_back(p);
    }

    /// Keeps at least `keep_secs` (whole keyframe intervals) and at most `max_bytes`.
    pub fn trim(&mut self, keep_secs: f64, max_bytes: usize) {
        let Some(newest) = self.packets.back().map(|p| p.t) else { return };
        let keep = (keep_secs * 1e7) as i64;
        loop {
            // The next keyframe after the first packet: dropping up to it keeps a keyframe at the front.
            let Some(next_key) = self.packets.iter().skip(1).position(|p| p.key).map(|i| i + 1) else { break };
            let too_old = newest - self.packets[next_key].t >= keep;
            if !(too_old || self.bytes > max_bytes) {
                break;
            }
            for _ in 0..next_key {
                if let Some(p) = self.packets.pop_front() {
                    self.bytes -= p.data.len();
                }
            }
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn newest_hns(&self) -> Option<i64> {
        self.packets.back().map(|p| p.t + frame_hns(self.format.fps))
    }

    pub fn buffered_secs(&self) -> f64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(a), Some(b)) => (b.t - a.t + frame_hns(self.format.fps)) as f64 / 1e7,
            _ => 0.0,
        }
    }

    /// The last `secs` (from a keyframe), never starting before `not_before` (the
    /// keyframe at or just before it).
    pub fn snapshot_since(&self, secs: f64, not_before: Option<i64>) -> Option<Snapshot> {
        let last = self.packets.back()?;
        let mut want = last.t - (secs * 1e7) as i64;
        if let Some(f) = not_before {
            want = want.max(f);
        }
        self.from(want, i64::MAX)
    }

    /// From the keyframe at or before `start` to the last packet at or before `end`.
    pub fn snapshot_range(&self, start: i64, end: i64) -> Option<Snapshot> {
        self.from(start, end)
    }

    fn from(&self, start: i64, end: i64) -> Option<Snapshot> {
        let first = self
            .packets
            .iter()
            .rposition(|p| p.key && p.t <= start)
            .or_else(|| self.packets.iter().position(|p| p.key))?;
        let packets: Vec<Packet> = self.packets.iter().skip(first).take_while(|p| p.t <= end).cloned().collect();
        (!packets.is_empty()).then(|| Snapshot { format: self.format, packets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(secs: i64) -> Ring {
        let mut r = Ring::default();
        r.set_format(Format { codec: 0, width: 1920, height: 1080, fps: 60 });
        for f in 0..secs * 60 {
            r.push(Packet { t: 1_000_000_000 + f * 166_667, key: f % 60 == 0, data: Arc::new(vec![0u8; 1000]) });
        }
        r
    }

    #[test]
    fn keeps_whole_seconds_and_cuts_at_keyframes() {
        let mut r = ring(20);
        r.trim(5.0, usize::MAX);
        let s = r.buffered_secs();
        assert!((5.0..7.0).contains(&s), "{s}");
        let snap = r.snapshot_since(3.0, None).unwrap();
        assert!(snap.packets[0].key);
        assert!(snap.end_hns() - snap.start_hns() >= 30_000_000);
        r.trim(60.0, 120_000);
        assert!(r.bytes() <= 120_000 + 60_000);
    }

    #[test]
    fn ranges_and_floors() {
        let r = ring(30);
        let at = |s: f64| 1_000_000_000 + (s * 1e7) as i64;
        let snap = r.snapshot_range(at(10.5), at(20.2)).unwrap();
        assert_eq!(snap.start_hns(), 1_000_000_000 + 600 * 166_667); // the keyframe at 10 s
        assert!(snap.packets.last().unwrap().t <= at(20.2));
        let since = r.snapshot_since(60.0, Some(at(25.4))).unwrap();
        assert!((since.start_hns() - at(25.0)).abs() < 1000);
        // Leading non-keyframes are never kept; time never goes back.
        let mut e = Ring::default();
        e.push(Packet { t: 5, key: false, data: Arc::new(vec![1]) });
        assert_eq!(e.bytes(), 0);
        e.push(Packet { t: 10, key: true, data: Arc::new(vec![1]) });
        e.push(Packet { t: 9, key: false, data: Arc::new(vec![1]) });
        assert_eq!(e.bytes(), 1);
    }
}
