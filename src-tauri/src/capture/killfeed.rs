//! Counts your kills in a saved VALORANT clip by reading its killfeed.
//!
//! Only the saved video file is read — never the screen or the game — so it
//! can't interfere with the game or its anti-cheat.
//!
//! Your own kills are drawn in the killfeed (top right) with a thin yellow
//! border: a yellow line along the top and bottom of a teal/red bar that runs to
//! the right edge of the screen. ffmpeg decodes just that corner of the clip,
//! four frames a second, and the bordered entries are counted. Each entry stays
//! up about five seconds; a new one means a new kill.
//!
//! Tuned on frames from real clips: walls, blonde hair, the gold "killed by"
//! card and teammates' kills (no yellow border) don't count.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// VALORANT's game exe (lower case).
pub const VALORANT_EXE: &str = "valorant-win64-shipping.exe";

/// No kill for this long and the round is over. A round's end screen plus the buy
/// phase take longer than this, so two rounds never share a clip, while an ace with
/// long pauses between kills still does.
pub const ROUND_GAP_SECS: u64 = 35;
/// A kill clip never covers more than this from the first kill (a full round with
/// spike plant fits).
pub const MAX_ROUND_SECS: u64 = 150;

/// Kills in the round being played: (first kill, last kill) on the capture clock.
#[derive(Default)]
pub struct Round {
    first: Option<(Instant, i64)>,
    last: Option<(Instant, i64)>,
    kills: u32,
}

impl Round {
    pub fn kill(&mut self, n: u32, now: Instant, now_hns: i64) {
        if self.first.is_none() {
            self.first = Some((now, now_hns));
        }
        self.last = Some((now, now_hns));
        self.kills += n;
    }

    /// The round's (first, last, kills) once it's over, else None.
    pub fn finished(&mut self, now: Instant) -> Option<(i64, i64, u32)> {
        let (first, last) = (self.first?, self.last?);
        let quiet = now.duration_since(last.0) >= Duration::from_secs(ROUND_GAP_SECS);
        let long = now.duration_since(first.0) >= Duration::from_secs(MAX_ROUND_SECS);
        (quiet || long).then(|| self.take()).flatten()
    }

    pub fn take(&mut self) -> Option<(i64, i64, u32)> {
        let r = Some((self.first?.1, self.last?.1, self.kills));
        *self = Self::default();
        r
    }
}

/// Clips in a folder with this name (any case) get their kills counted.
pub fn is_valorant(game: &str) -> bool {
    game.trim().eq_ignore_ascii_case("valorant")
}

/// Killfeed area relative to the game window, at 1080p (scaled by height).
const AREA_W: f64 = 770.0;
const AREA_Y0: f64 = 40.0;
const AREA_Y1: f64 = 330.0;

/// A pixel of the yellow "your kill" border (it's slightly see-through, so it
/// gets paler over a bright sky — hence the loose limits).
#[inline]
fn yellow(r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    r >= 170 && g >= 160 && r.min(g) - b >= 40 && (r - g).abs() <= 50
}

/// A pixel of a killfeed bar: teal (your team) or red (enemy).
#[inline]
fn bar_color(r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (g > r + 40 && g > 110) || (r > 170 && r > g + 60 && r > b + 60)
}

/// A captured area: top-down rows of BGR pixels.
pub struct Image<'a> {
    pub w: usize,
    pub h: usize,
    pub stride: usize,
    pub bgr: &'a [u8],
}

impl Image<'_> {
    #[inline]
    fn px(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = y * self.stride + x * 3;
        (self.bgr[i + 2], self.bgr[i + 1], self.bgr[i])
    }
    #[inline]
    fn is_yellow(&self, x: usize, y: usize) -> bool {
        let (r, g, b) = self.px(x, y);
        yellow(r, g, b)
    }
    /// Share of yellow pixels on row `y` between x0 and x1 (0 outside the image).
    fn yellow_share(&self, y: isize, x0: usize, x1: usize) -> f64 {
        if y < 0 || y as usize >= self.h || x1 < x0 {
            return 0.0;
        }
        let n = (x0..=x1).filter(|&x| self.is_yellow(x, y as usize)).count();
        n as f64 / (x1 - x0 + 1) as f64
    }
}

/// Horizontal runs of yellow on one row (small gaps allowed), at least `min` long.
fn runs(img: &Image, y: usize, min: usize) -> Vec<(usize, usize)> {
    const GAP: usize = 4;
    let mut out = Vec::new();
    let mut x = 0;
    while x < img.w {
        if !img.is_yellow(x, y) {
            x += 1;
            continue;
        }
        let (x0, mut last) = (x, x);
        x += 1;
        while x < img.w && x - last <= GAP + 1 {
            if img.is_yellow(x, y) {
                last = x;
            }
            x += 1;
        }
        if last - x0 + 1 >= min {
            out.push((x0, last));
        }
        x = last + 1;
    }
    out
}

/// Your kills the killfeed shows right now: the top edge (y) of each entry,
/// top to bottom. `s` = screen height / 1080.
pub fn own_kills(img: &Image, s: f64) -> Vec<usize> {
    if img.w < 50 || img.h < 50 {
        return Vec::new();
    }
    let min_run = (24.0 * s).round() as usize;
    // Yellow lines; a line two pixels thick counts once (keeping its longest run).
    let mut lines: Vec<(usize, usize, usize)> = Vec::new();
    for y in 0..img.h {
        for (x0, x1) in runs(img, y, min_run) {
            match lines.iter_mut().find(|(ly, lx0, lx1)| y - *ly <= 2 && !(x1 < *lx0 || x0 > *lx1)) {
                None => lines.push((y, x0, x1)),
                Some(l) if x1 - x0 > l.2 - l.1 => {
                    l.1 = x0;
                    l.2 = x1;
                }
                Some(_) => {}
            }
        }
    }
    let (dy_min, dy_max) = ((27.0 * s) as usize, (37.0 * s).ceil() as usize);
    let mut used = vec![false; lines.len()];
    let mut found = Vec::new();
    for i in 0..lines.len() {
        if used[i] {
            continue;
        }
        let (y, x0, x1) = lines[i];
        // A top border has (almost) no yellow just above it.
        if img.yellow_share(y as isize - 3, x0, x1) > 0.35 {
            continue;
        }
        for j in 0..lines.len() {
            if j == i || used[j] {
                continue;
            }
            let (y2, a0, a1) = lines[j];
            if y2 < y + dy_min || y2 > y + dy_max {
                continue;
            }
            if (a0 as f64 - x0 as f64).abs() > 8.0 * s {
                continue;
            }
            if img.yellow_share(y2 as isize + 3, a0, a1) > 0.35 {
                continue;
            }
            let dy = y2 - y;
            // Inside is not yellow (a yellow block isn't a border).
            if img.yellow_share((y + dy / 2) as isize, x0.max(a0), x1.min(a1)) > 0.6 {
                continue;
            }
            // The entry is a teal/red bar running to the right edge.
            let lx = x0.min(a0);
            let right = img.w.saturating_sub((70.0 * s) as usize);
            if right < lx + (120.0 * s) as usize {
                continue;
            }
            let ym = y + dy / 2;
            let off = (6.0 * s) as usize;
            let (mut bar, mut tot) = (0usize, 0usize);
            for yy in [ym.saturating_sub(off), (ym + off).min(img.h - 1)] {
                for x in (lx..right).step_by(2) {
                    let (r, g, b) = img.px(x, yy);
                    tot += 1;
                    if bar_color(r, g, b) {
                        bar += 1;
                    }
                }
            }
            if (bar as f64) < 0.25 * tot as f64 {
                continue;
            }
            used[i] = true;
            used[j] = true;
            found.push(y);
            break;
        }
    }
    found.sort_unstable();
    found
}

/// How long one killfeed entry stays up (measured: ~5 s).
const ENTRY_LIFE: Duration = Duration::from_millis(5500);

/// Turns what the killfeed shows into kill events.
///
/// Each of your entries stays up about five seconds. New entries are added at
/// the bottom and older ones slide up as they expire. So a kill is either more
/// of your entries than are still alive, or one of yours showing up below your
/// lowest live one (a kill that lands just as the previous one fades out).
///
/// A sample is only believed when the next one shows the same entries in the
/// same places (they slide in), and an entry that flickers out over a bright
/// background is still alive, so it isn't counted twice. After a reset (game
/// just focused again) the first steady sample is only the starting point, so
/// kills already in the feed aren't saved again.
pub struct Tracker {
    last: Option<Vec<usize>>,
    primed: bool,
    /// Live entries: (first seen, last y), top to bottom.
    live: Vec<(Instant, usize)>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self { last: None, primed: false, live: Vec::new() }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feed one sample (entry tops from `own_kills`); `row` = entry height in
    /// pixels. Returns how many new kills it shows.
    pub fn feed(&mut self, rows: &[usize], row: usize, now: Instant) -> usize {
        let settle = (row / 8).max(2);
        let stable = self.last.as_ref().is_some_and(|l| {
            l.len() == rows.len() && l.iter().zip(rows).all(|(a, b)| a.abs_diff(*b) <= settle)
        });
        self.last = Some(rows.to_vec());
        if !stable {
            return 0;
        }
        self.live.retain(|(t, _)| now.duration_since(*t) < ENTRY_LIFE);
        if !self.primed {
            self.primed = true;
            self.live = rows.iter().map(|&y| (now, y)).collect();
            return 0;
        }
        let below = match self.live.iter().map(|(_, y)| *y).max() {
            Some(lowest) => rows.iter().filter(|&&y| y > lowest + row / 2).count(),
            None => 0,
        };
        let new = rows.len().saturating_sub(self.live.len()).max(below);
        // The entries already known moved to where the upper rows are now.
        let old = rows.len() - new;
        let n = self.live.len();
        for (k, &y) in rows[..old].iter().rev().enumerate() {
            if k < n {
                self.live[n - 1 - k].1 = y;
            }
        }
        self.live.extend(rows[old..].iter().map(|&y| (now, y)));
        new
    }
}

/// Size of the killfeed crop the detector works on (1080p pixels).
const CROP_W: usize = AREA_W as usize;
const CROP_H: usize = (AREA_Y1 - AREA_Y0) as usize;

/// ffmpeg filter: the killfeed corner of any 16:9-ish video, scaled to 1080p
/// size, four frames a second.
fn crop_filter() -> String {
    format!(
        "fps=4,crop=w='min(iw,{w}*ih/1080)':h='{h}*ih/1080':x='iw-min(iw,{w}*ih/1080)':y='{y}*ih/1080',scale={w}:{h}:flags=bilinear,format=bgr24",
        w = AREA_W as u32,
        h = (AREA_Y1 - AREA_Y0) as u32,
        y = AREA_Y0 as u32
    )
}

/// Counts your kills in a saved clip. Runs ffmpeg at low priority on the
/// video's killfeed corner only (a 1-minute 1080p clip takes a few seconds).
pub fn count_in_clip(ffmpeg: &Path, clip: &Path, on_spawn: &dyn Fn(&std::process::Child)) -> Result<u32, String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2", "-i"])
        .arg(clip)
        .args(["-an", "-sn", "-dn", "-vf", &crop_filter(), "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000 /* no window */ | 0x0000_4000 /* below normal priority */);
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start ffmpeg: {e}"))?;
    on_spawn(&child);
    let mut out = child.stdout.take().ok_or("no ffmpeg output")?;
    let mut err = child.stderr.take().ok_or("no ffmpeg output")?;
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let stride = CROP_W * 3;
    let mut frame = vec![0u8; stride * CROP_H];
    let t0 = Instant::now();
    // The first steady frame is the starting point: entries already showing
    // when the clip starts are kills from before it.
    let mut tracker = Tracker::new();
    let (mut n, mut kills) = (0u64, 0u32);
    while out.read_exact(&mut frame).is_ok() {
        n += 1;
        let rows = own_kills(&Image { w: CROP_W, h: CROP_H, stride, bgr: &frame }, 1.0);
        kills += tracker.feed(&rows, 32, t0 + Duration::from_millis(n * 250)) as u32;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let errs = errs.join().unwrap_or_default();
    if !status.success() || n == 0 {
        let msg = errs.lines().last().unwrap_or("ffmpeg couldn't read the clip").to_string();
        return Err(msg);
    }
    Ok(kills)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paints a killfeed-like picture: background, teal/red bars, and optionally
    /// the yellow border of "your" kill.
    fn feed_image(rows: &[(usize, bool)], w: usize, h: usize) -> Vec<u8> {
        let stride = (w * 3 + 3) & !3;
        let mut px = vec![0u8; stride * h];
        let mut set = |x: usize, y: usize, (r, g, b): (u8, u8, u8)| {
            let i = y * stride + x * 3;
            px[i] = b;
            px[i + 1] = g;
            px[i + 2] = r;
        };
        for y in 0..h {
            for x in 0..w {
                set(x, y, (98, 78, 49)); // brown wall
            }
        }
        for &(top, mine) in rows {
            let x0 = 360;
            for y in top..top + 33 {
                for x in x0..w - 20 {
                    set(x, y, if x < 560 { (94, 184, 170) } else { (240, 104, 84) });
                }
            }
            if mine {
                for x in x0..x0 + 120 {
                    set(x, top, (234, 231, 110));
                    set(x, top + 32, (226, 208, 129));
                }
                for y in top..top + 33 {
                    set(x0, y, (230, 225, 110));
                }
            }
        }
        px
    }

    fn count(rows: &[(usize, bool)]) -> usize {
        let (w, h) = (770, 290);
        let px = feed_image(rows, w, h);
        own_kills(&Image { w, h, stride: (w * 3 + 3) & !3, bgr: &px }, 1.0).len()
    }

    #[test]
    fn counts_only_your_kills() {
        assert_eq!(count(&[]), 0);
        assert_eq!(count(&[(56, false)]), 0);
        assert_eq!(count(&[(56, true)]), 1);
        assert_eq!(count(&[(56, false), (95, true), (134, false)]), 1);
        assert_eq!(count(&[(56, true), (95, true), (134, true)]), 3);
    }

    #[test]
    fn a_yellow_block_is_not_a_kill() {
        let (w, h) = (770, 290);
        let stride = (w * 3 + 3) & !3;
        let mut px = vec![0u8; stride * h];
        for y in 60..92 {
            for x in 300..700 {
                let i = y * stride + x * 3;
                px[i] = 120;
                px[i + 1] = 200;
                px[i + 2] = 210;
            }
        }
        assert!(own_kills(&Image { w, h, stride, bgr: &px }, 1.0).is_empty());
    }

    #[test]
    fn tracker_counts_each_kill_once() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut t = Tracker::new();
        let mut feed = |rows: &[usize], ms: u64| t.feed(rows, 32, at(ms));
        assert_eq!(feed(&[], 0), 0);
        assert_eq!(feed(&[], 250), 0);
        assert_eq!(feed(&[95], 500), 0, "needs two samples");
        assert_eq!(feed(&[95], 750), 1);
        assert_eq!(feed(&[95], 1000), 0);
        // flickers out for a while, comes back: still the same kill
        assert_eq!(feed(&[], 1250), 0);
        assert_eq!(feed(&[], 1500), 0);
        assert_eq!(feed(&[], 3500), 0);
        assert_eq!(feed(&[95], 3750), 0);
        assert_eq!(feed(&[95], 4000), 0);
        // the entry above expires and yours slides up: not a kill
        assert_eq!(feed(&[56], 4250), 0);
        assert_eq!(feed(&[56], 4500), 0);
        // a double kill; the newest slides in from below first
        assert_eq!(feed(&[56, 95, 175], 4750), 0);
        assert_eq!(feed(&[56, 95, 140], 5000), 0, "still moving");
        assert_eq!(feed(&[56, 95, 134], 5250), 0);
        assert_eq!(feed(&[56, 95, 134], 5500), 2);
        // the first one fades out just as another kill comes in below
        assert_eq!(feed(&[95, 134, 173], 6500), 0);
        assert_eq!(feed(&[95, 134, 173], 6750), 1);
        // everything expires; a new kill at the top
        assert_eq!(feed(&[], 13000), 0);
        assert_eq!(feed(&[], 13250), 0);
        assert_eq!(feed(&[56], 13500), 0);
        assert_eq!(feed(&[56], 13750), 1);
    }

    #[test]
    fn two_kills_in_a_row_at_the_same_spot() {
        // One entry for ~5 s, a short gap, then the next kill in the same place.
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut t = Tracker::new();
        let mut kills = 0;
        let _ = t.feed(&[], 32, at(0));
        let _ = t.feed(&[], 32, at(250));
        for i in 0..20 {
            kills += t.feed(&[56], 32, at(500 + i * 250));
        }
        for i in 0..4 {
            kills += t.feed(&[], 32, at(5500 + i * 250));
        }
        for i in 0..20 {
            kills += t.feed(&[56], 32, at(6500 + i * 250));
        }
        assert_eq!(kills, 2);
    }

    #[test]
    fn kills_already_in_the_feed_dont_count_after_alt_tab() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut t = Tracker::new();
        assert_eq!(t.feed(&[56, 95], 32, at(0)), 0);
        assert_eq!(t.feed(&[56, 95], 32, at(250)), 0);
        assert_eq!(t.feed(&[56, 95, 134], 32, at(500)), 0);
        assert_eq!(t.feed(&[56, 95, 134], 32, at(750)), 1);
    }

    /// Counts kills in a real clip: CLIPIT_KILLS_CLIP=<mp4> cargo test count_real_clip -- --ignored --nocapture
    #[test]
    #[ignore]
    fn count_real_clip() {
        let clip = std::env::var("CLIPIT_KILLS_CLIP").expect("CLIPIT_KILLS_CLIP");
        let t = Instant::now();
        let kills = count_in_clip(Path::new("ffmpeg"), Path::new(&clip), &|_| {}).unwrap();
        println!("{kills} kill(s) in {:?}", t.elapsed());
    }

    /// Runs the detector over killfeed frames cut from real clips:
    /// CLIPIT_KF_DIR=<folder of 770x290 BMPs> cargo test real_frames -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_frames() {
        let dir = std::env::var("CLIPIT_KF_DIR").expect("CLIPIT_KF_DIR");
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        // Files are "<clip>_<frame>.bmp" at 4 frames a second.
        let t0 = Instant::now();
        let mut tracker = Tracker::new();
        let (mut clip, mut frame, mut total) = (String::new(), 0u64, 0usize);
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let this = name.split('_').next().unwrap_or("").to_string();
            if this != clip {
                clip = this;
                frame = 0;
                tracker.reset();
            }
            frame += 1;
            let data = std::fs::read(&f).unwrap();
            if data.len() < 54 || &data[0..2] != b"BM" {
                continue;
            }
            let off = u32::from_le_bytes(data[10..14].try_into().unwrap()) as usize;
            let w = i32::from_le_bytes(data[18..22].try_into().unwrap()) as usize;
            let hh = i32::from_le_bytes(data[22..26].try_into().unwrap());
            let h = hh.unsigned_abs() as usize;
            let stride = (w * 3 + 3) & !3;
            let mut rows = data[off..off + stride * h].to_vec();
            if hh > 0 {
                // bottom-up → top-down
                let mut flipped = vec![0u8; rows.len()];
                for y in 0..h {
                    flipped[y * stride..(y + 1) * stride].copy_from_slice(&rows[(h - 1 - y) * stride..(h - y) * stride]);
                }
                rows = flipped;
            }
            let n = own_kills(&Image { w, h, stride, bgr: &rows }, h as f64 / 290.0);
            let new = tracker.feed(&n, 32, t0 + Duration::from_millis(frame * 250));
            total += new;
            if new > 0 {
                println!("{name}: +{new} kill(s) at {:.2} s, yours at {n:?}", frame as f64 / 4.0);
            }
        }
        println!("total kills: {total}");
    }
}
