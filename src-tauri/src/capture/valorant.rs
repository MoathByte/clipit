//! What a saved VALORANT clip shows: your kills, the map and the agent you played.
//!
//! Everything here reads the **saved video file** (a few frames decoded by
//! ffmpeg), never the screen or the game.
//!
//! * Agent: the four ability icons at the bottom of the HUD (C, Q, E, X) are
//!   compared with every agent's ability icons.
//! * Map: the minimap's walkable floor (flat grey) is matched against every map's
//!   layout. The minimap can be rotated (it turns with you), zoomed in and cut off
//!   by its circle, so the match searches rotations and zoom levels and finds where
//!   the visible piece fits, first coarsely, then finely for the best few maps.
//!   Several frames vote, so one bad frame (a menu, a flash) doesn't decide.
//!
//! Reference images (ability icons, map layouts) come from valorant-api.com, a
//! free community API, and are cached on disk (see `fetch_refs`).

use super::encoder::CREATE_NO_WINDOW;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ClipInfo {
    pub kills: Option<u32>,
    /// Saved automatically (a 3K, 4K, ace…), shown as "Auto clipped".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    pub map: Option<String>,
    pub agent: Option<String>,
}

// ---------- reference images ----------

const API: &str = "https://valorant-api.com/v1";
const SLOTS: [&str; 4] = ["Grenade", "Ability1", "Ability2", "Ultimate"]; // C, Q, E, X

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    /// file stem → display name
    agents: HashMap<String, String>,
    maps: HashMap<String, String>,
    fetched_unix: u64,
}

fn safe(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Downloads ability icons and map layouts (once a week; new agents and maps
/// appear over time). Safe to call at every start.
pub async fn fetch_refs(http: &reqwest::Client, dir: &Path) -> Result<(), String> {
    let manifest_path = dir.join("manifest.json");
    if let Some(m) = std::fs::read_to_string(&manifest_path).ok().and_then(|s| serde_json::from_str::<Manifest>(&s).ok()) {
        if now_unix().saturating_sub(m.fetched_unix) < 7 * 86_400 && !m.agents.is_empty() {
            return Ok(());
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let get = |url: String| async move {
        http.get(url).send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())
    };
    let agents: serde_json::Value = get(format!("{API}/agents?isPlayableCharacter=true")).await?.json().await.map_err(|e| e.to_string())?;
    let maps: serde_json::Value = get(format!("{API}/maps")).await?.json().await.map_err(|e| e.to_string())?;
    let mut manifest = Manifest::default();
    let mut jobs: Vec<(String, std::path::PathBuf)> = Vec::new();
    for a in agents["data"].as_array().into_iter().flatten() {
        let name = a["displayName"].as_str().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        manifest.agents.insert(safe(name), name.to_string());
        for ab in a["abilities"].as_array().into_iter().flatten() {
            let slot = ab["slot"].as_str().unwrap_or_default();
            if let (true, Some(url)) = (SLOTS.contains(&slot), ab["displayIcon"].as_str()) {
                jobs.push((url.to_string(), dir.join(format!("ab_{}_{}.png", safe(name), slot))));
            }
        }
    }
    for m in maps["data"].as_array().into_iter().flatten() {
        let name = m["displayName"].as_str().unwrap_or_default();
        // Only real competitive maps have sites (skips the range, team deathmatch, skirmish).
        let sites = m["tacticalDescription"].as_str().unwrap_or_default();
        if let (false, Some(url)) = (sites.is_empty() || name.is_empty(), m["displayIcon"].as_str()) {
            manifest.maps.insert(safe(name), name.to_string());
            jobs.push((url.to_string(), dir.join(format!("map_{}.png", safe(name)))));
        }
    }
    for (url, path) in jobs {
        if path.exists() {
            continue;
        }
        let bytes = get(url).await?.bytes().await.map_err(|e| e.to_string())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    }
    manifest.fetched_unix = now_unix();
    std::fs::write(&manifest_path, serde_json::to_string(&manifest).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    // Drop what's loaded so the next clip uses the new images.
    *REFS.get_or_init(|| Mutex::new(None)).lock().unwrap() = None;
    Ok(())
}

/// A grey image as f32 rows.
#[derive(Clone)]
struct Grid {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

impl Grid {
    fn new(w: usize, h: usize, fill: f32) -> Self {
        Self { w, h, v: vec![fill; w * h] }
    }
    fn at(&self, x: usize, y: usize) -> f32 {
        self.v[y * self.w + x]
    }
    /// Bilinear sample with `outside` beyond the edges.
    fn sample(&self, x: f32, y: f32, outside: f32) -> f32 {
        if x < 0.0 || y < 0.0 || x > (self.w - 1) as f32 || y > (self.h - 1) as f32 {
            return outside;
        }
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let a = self.at(x0, y0) * (1.0 - fx) + self.at(x1, y0) * fx;
        let b = self.at(x0, y1) * (1.0 - fx) + self.at(x1, y1) * fx;
        a * (1.0 - fy) + b * fy
    }
    /// Area-average downscale (or bilinear upscale) to w×h.
    fn resize(&self, w: usize, h: usize) -> Grid {
        let mut out = Grid::new(w, h, 0.0);
        let (sx, sy) = (self.w as f32 / w as f32, self.h as f32 / h as f32);
        for y in 0..h {
            for x in 0..w {
                if sx <= 1.0 && sy <= 1.0 {
                    out.v[y * w + x] = self.sample((x as f32 + 0.5) * sx - 0.5, (y as f32 + 0.5) * sy - 0.5, 0.0);
                } else {
                    let x0 = ((x as f32 * sx) as usize).min(self.w - 1);
                    let y0 = ((y as f32 * sy) as usize).min(self.h - 1);
                    let x1 = (((x + 1) as f32 * sx) as usize).clamp(x0 + 1, self.w);
                    let y1 = (((y + 1) as f32 * sy) as usize).clamp(y0 + 1, self.h);
                    let mut acc = 0.0;
                    for yy in y0..y1 {
                        for xx in x0..x1 {
                            acc += self.at(xx, yy);
                        }
                    }
                    out.v[y * w + x] = acc / ((x1 - x0) * (y1 - y0)) as f32;
                }
            }
        }
        out
    }
}

/// Crops `g` to the box of values above `t` and pads it to a centred square.
fn square_bbox(g: &Grid, t: f32) -> Option<Grid> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    let mut n = 0;
    for y in 0..g.h {
        for x in 0..g.w {
            if g.at(x, y) > t {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
                n += 1;
            }
        }
    }
    if n < 15 {
        return None;
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let s = w.max(h);
    let mut out = Grid::new(s, s, 0.0);
    let (ox, oy) = ((s - w) / 2, (s - h) / 2);
    for y in 0..h {
        for x in 0..w {
            out.v[(y + oy) * s + x + ox] = g.at(x0 + x, y0 + y);
        }
    }
    Some(out)
}

/// Zero-mean, unit-length vector (for correlation).
fn normalized(g: &Grid) -> Vec<f32> {
    let mean = g.v.iter().sum::<f32>() / g.v.len() as f32;
    let mut v: Vec<f32> = g.v.iter().map(|x| x - mean).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn load_png(path: &Path) -> Option<image::RgbaImage> {
    image::open(path).ok().map(|i| i.to_rgba8())
}

const ICON: usize = 40;
const REF1: usize = 56; // coarse map size
const FFT1: usize = 160;
const REF2: usize = 112; // fine map size
const FFT2: usize = 288;

struct MapRef {
    name: String,
    coarse: Vec<Complex32>, // FFT of the ±1 layout on an FFT1 canvas
    fine: Vec<Complex32>,
    fine_walk: Vec<bool>, // REF2×REF2 on an FFT2 canvas
}

struct Refs {
    /// agent → [C, Q, E, X] icon vectors
    agents: Vec<(String, [Option<Vec<f32>>; 4])>,
    maps: Vec<MapRef>,
}

static REFS: OnceLock<Mutex<Option<Arc<Refs>>>> = OnceLock::new();

fn refs(dir: &Path) -> Option<Arc<Refs>> {
    let cell = REFS.get_or_init(|| Mutex::new(None));
    let mut slot = cell.lock().unwrap();
    if let Some(r) = slot.as_ref() {
        return Some(r.clone());
    }
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).ok()?).ok()?;
    let mut agents = Vec::new();
    for (stem, name) in &manifest.agents {
        let mut icons: [Option<Vec<f32>>; 4] = Default::default();
        for (i, slot) in SLOTS.iter().enumerate() {
            let Some(img) = load_png(&dir.join(format!("ab_{stem}_{slot}.png"))) else { continue };
            let mut g = Grid::new(img.width() as usize, img.height() as usize, 0.0);
            for (x, y, p) in img.enumerate_pixels() {
                g.v[y as usize * g.w + x as usize] = p[3] as f32 / 255.0;
            }
            icons[i] = square_bbox(&g, 0.2).map(|s| normalized(&s.resize(ICON, ICON)));
        }
        agents.push((name.clone(), icons));
    }
    let mut planner = FftPlanner::<f32>::new();
    let mut maps = Vec::new();
    for (stem, name) in &manifest.maps {
        let Some(img) = load_png(&dir.join(format!("map_{stem}.png"))) else { continue };
        let mut g = Grid::new(img.width() as usize, img.height() as usize, 0.0);
        for (x, y, p) in img.enumerate_pixels() {
            g.v[y as usize * g.w + x as usize] = if p[3] > 128 { 1.0 } else { 0.0 };
        }
        let canvas = |size: usize, fft: usize| -> (Vec<Complex32>, Vec<bool>) {
            let small = g.resize(size, size);
            let mut c = vec![Complex32::new(-1.0, 0.0); fft * fft];
            let mut walk = vec![false; fft * fft];
            for y in 0..size {
                for x in 0..size {
                    let w = small.at(x, y) > 0.5;
                    c[y * fft + x] = Complex32::new(if w { 1.0 } else { -1.0 }, 0.0);
                    walk[y * fft + x] = w;
                }
            }
            (c, walk)
        };
        let (mut coarse, _) = canvas(REF1, FFT1);
        fft2(&mut planner, &mut coarse, FFT1, false);
        let (mut fine, fine_walk) = canvas(REF2, FFT2);
        fft2(&mut planner, &mut fine, FFT2, false);
        maps.push(MapRef { name: name.clone(), coarse, fine, fine_walk });
    }
    if agents.is_empty() && maps.is_empty() {
        return None;
    }
    let r = Arc::new(Refs { agents, maps });
    *slot = Some(r.clone());
    Some(r)
}

fn fft2(planner: &mut FftPlanner<f32>, data: &mut [Complex32], n: usize, inverse: bool) {
    let f: Arc<dyn Fft<f32>> = if inverse { planner.plan_fft_inverse(n) } else { planner.plan_fft_forward(n) };
    for row in data.chunks_mut(n) {
        f.process(row);
    }
    let mut col = vec![Complex32::new(0.0, 0.0); n];
    for x in 0..n {
        for y in 0..n {
            col[y] = data[y * n + x];
        }
        f.process(&mut col);
        for y in 0..n {
            data[y * n + x] = col[y];
        }
    }
}

// ---------- reading frames ----------

/// A 1920×1080 frame, BGR.
struct Frame(Vec<u8>);
const FW: usize = 1920;
const FH: usize = 1080;

impl Frame {
    fn px(&self, x: usize, y: usize) -> (f32, f32, f32) {
        let i = (y * FW + x) * 3;
        (self.0[i + 2] as f32, self.0[i + 1] as f32, self.0[i] as f32)
    }
}

/// Up to `n` frames spread over the clip, scaled to 1920×1080.
fn frames(ffmpeg: &Path, clip: &Path, n: usize, on_spawn: &dyn Fn(&std::process::Child)) -> Vec<Frame> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "2", "-i"])
        .arg(clip)
        .args(["-an", "-sn", "-dn", "-vf", "fps=1/4,scale=1920:1080,format=bgr24", "-frames:v", &n.to_string(), "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | 0x0000_4000);
    let Ok(mut child) = cmd.spawn() else { return Vec::new() };
    on_spawn(&child);
    let mut out = Vec::new();
    if let Some(mut so) = child.stdout.take() {
        loop {
            let mut buf = vec![0u8; FW * FH * 3];
            if so.read_exact(&mut buf).is_err() {
                break;
            }
            out.push(Frame(buf));
        }
    }
    let _ = child.wait();
    out
}

// ---------- agent ----------

const ABILITY_CENTERS: [(usize, usize); 4] = [(790, 1005), (902, 1005), (1015, 1005), (1128, 1005)];

/// White (or greyed-out, on cooldown) icon pixels around an ability slot.
fn icon_vector(f: &Frame, cx: usize, cy: usize) -> Option<Vec<f32>> {
    let half = 28;
    let n = half * 2;
    let mut lum = vec![0.0f32; n * n];
    let mut sat = vec![0.0f32; n * n];
    for y in 0..n {
        for x in 0..n {
            let (r, gg, b) = f.px(cx - half + x, cy - half + y);
            lum[y * n + x] = (r + gg + b) / 3.0;
            sat[y * n + x] = r.max(gg).max(b) - r.min(gg).min(b);
        }
    }
    // Brighter than the background behind the HUD (icons on cooldown are a dim grey).
    let mut sorted = lum.clone();
    sorted.sort_by(f32::total_cmp);
    let bg = sorted[sorted.len() * 35 / 100];
    let lo = (bg + 18.0).max(120.0);
    let mut g = Grid::new(n, n, 0.0);
    for i in 0..n * n {
        g.v[i] = if sat[i] < 45.0 { ((lum[i] - lo) / 50.0).clamp(0.0, 1.0) } else { 0.0 };
    }
    square_bbox(&g, 0.3).map(|s| normalized(&s.resize(ICON, ICON)))
}

/// Best agent for one frame: (name, score, margin over the next one).
fn agent_in(f: &Frame, refs: &Refs) -> Option<(String, f32, f32)> {
    let hud: Vec<Option<Vec<f32>>> = ABILITY_CENTERS.iter().map(|&(x, y)| icon_vector(f, x, y)).collect();
    if hud.iter().filter(|v| v.is_some()).count() < 2 {
        return None;
    }
    let mut scores: Vec<(f32, &str)> = refs
        .agents
        .iter()
        .map(|(name, icons)| {
            let (mut tot, mut n) = (0.0, 0);
            for (h, r) in hud.iter().zip(icons) {
                if let (Some(h), Some(r)) = (h, r) {
                    tot += h.iter().zip(r).map(|(a, b)| a * b).sum::<f32>();
                    n += 1;
                }
            }
            (if n > 0 { tot / n as f32 } else { 0.0 }, name.as_str())
        })
        .collect();
    scores.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (best, second) = (scores.first()?, scores.get(1).map(|s| s.0).unwrap_or(0.0));
    Some((best.1.to_string(), best.0, best.0 - second))
}

// ---------- map ----------

/// Minimap disc at 1080p.
const MM_CX: f32 = 245.0;
const MM_CY: f32 = 270.0;
const MM_R: f32 = 228.0;
const WALL: f32 = -0.5;

/// The minimap as +1 (flat grey floor) / WALL (anything else) / 0 (outside the disc).
fn minimap(f: &Frame) -> Option<Grid> {
    let d = (MM_R * 2.0) as usize;
    let (x0, y0) = ((MM_CX - MM_R) as usize, (MM_CY - MM_R) as usize);
    let mut g = Grid::new(d, d, 0.0);
    let mut walk = 0usize;
    let rr = (MM_R - 3.0) * (MM_R - 3.0);
    for y in 0..d {
        for x in 0..d {
            let (dx, dy) = (x as f32 - MM_R, y as f32 - MM_R);
            if dx * dx + dy * dy >= rr {
                continue;
            }
            let (r, gg, b) = f.px(x0 + x, y0 + y);
            let lum = (r + gg + b) / 3.0;
            let sat = r.max(gg).max(b) - r.min(gg).min(b);
            // The minimap floor is one flat grey (lum ≈ 115–125, no colour); light
            // walls behind the minimap are close, so the window is tight.
            let w = sat < 10.0 && lum > 108.0 && lum < 150.0;
            walk += w as usize;
            g.v[y * d + x] = if w { 1.0 } else { WALL };
        }
    }
    // A minimap shows a fair amount of floor; menus and death screens don't.
    let frac = walk as f32 / (std::f32::consts::PI * rr);
    (0.05..0.6).contains(&frac).then_some(g)
}

/// The minimap scaled by `t` (layout px per screen px, at `size`/1024 layout scale)
/// and rotated by `deg`, as a patch of ±1/WALL/0 values.
fn transform(mm: &Grid, t: f32, deg: f32, size_ref: usize) -> Option<Grid> {
    let s = ((mm.w as f32) * t * size_ref as f32 / 1024.0).round() as usize;
    if s < 8 {
        return None;
    }
    let (sin, cos) = deg.to_radians().sin_cos();
    let c = s as f32 / 2.0;
    let k = mm.w as f32 / s as f32;
    let mut out = Grid::new(s, s, 0.0);
    for y in 0..s {
        for x in 0..s {
            // rotate the destination point back into the source
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let (sx, sy) = (cos * dx + sin * dy, -sin * dx + cos * dy);
            let v = mm.sample((sx + c) * k - 0.5, (sy + c) * k - 0.5, 0.0);
            out.v[y * s + x] = if v.abs() < 0.2 { 0.0 } else if v > 0.0 { 1.0 } else { WALL };
        }
    }
    Some(out)
}

/// Correlates a patch against a map: best score (per known pixel) and its shift.
fn correlate(planner: &mut FftPlanner<f32>, patch: &Grid, fft: usize, maps: &[(&[Complex32], usize)]) -> Vec<(f32, usize, usize)> {
    let mut p = vec![Complex32::new(0.0, 0.0); fft * fft];
    let mut n = 0usize;
    for y in 0..patch.h {
        for x in 0..patch.w {
            let v = patch.at(x, y);
            if v != 0.0 {
                n += 1;
            }
            p[y * fft + x] = Complex32::new(v, 0.0);
        }
    }
    fft2(planner, &mut p, fft, false);
    let mut out = Vec::new();
    let mut c = vec![Complex32::new(0.0, 0.0); fft * fft];
    for (m, _) in maps {
        for i in 0..fft * fft {
            c[i] = m[i] * p[i].conj();
        }
        fft2(planner, &mut c, fft, true);
        let (mut best, mut at) = (f32::MIN, 0);
        for (i, v) in c.iter().enumerate() {
            if v.re > best {
                best = v.re;
                at = i;
            }
        }
        // (inverse FFT isn't normalised: divide by fft², then by the known pixels)
        out.push((best / (fft * fft) as f32 / n.max(1) as f32, at / fft, at % fft));
    }
    out
}

/// Map scores for one frame: map index → IoU of walkable floor at its best fit.
fn maps_in(f: &Frame, refs: &Refs, planner: &mut FftPlanner<f32>) -> Option<Vec<f32>> {
    let mm = minimap(f)?;
    let coarse: Vec<(&[Complex32], usize)> = refs.maps.iter().map(|m| (m.coarse.as_slice(), 0)).collect();
    // Coarse: all zoom levels and rotations for every map.
    let mut best = vec![(f32::MIN, 0.0f32, 0.0f32); refs.maps.len()];
    let mut t = 1.0f32;
    while t <= 3.7 {
        for step in 0..24 {
            let deg = step as f32 * 15.0;
            let Some(patch) = transform(&mm, t, deg, REF1) else { continue };
            if patch.w + REF1 > FFT1 {
                continue;
            }
            for (i, (score, _, _)) in correlate(planner, &patch, FFT1, &coarse).into_iter().enumerate() {
                if score > best[i].0 {
                    best[i] = (score, t, deg);
                }
            }
        }
        t *= 1.12;
    }
    // Fine: around each map's best fit, for the best few maps; scored by IoU.
    let mut order: Vec<usize> = (0..refs.maps.len()).collect();
    order.sort_by(|a, b| best[*b].0.total_cmp(&best[*a].0));
    let mut iou = vec![0.0f32; refs.maps.len()];
    for &i in order.iter().take(4) {
        let m = &refs.maps[i];
        let (_, t0, d0) = best[i];
        let mut top = (f32::MIN, None);
        for ts in [t0 / 1.06, t0, t0 * 1.06] {
            for dd in [-10.0, -5.0, 0.0, 5.0, 10.0] {
                let Some(patch) = transform(&mm, ts, d0 + dd, REF2) else { continue };
                if patch.w + REF2 > FFT2 {
                    continue;
                }
                let (score, dy, dx) = correlate(planner, &patch, FFT2, &[(m.fine.as_slice(), 0)])[0];
                if score > top.0 {
                    top = (score, Some((patch, dy, dx)));
                }
            }
        }
        if let (_, Some((patch, dy, dx))) = top {
            let (mut inter, mut union) = (0usize, 0usize);
            for y in 0..patch.h {
                for x in 0..patch.w {
                    let v = patch.at(x, y);
                    if v == 0.0 {
                        continue;
                    }
                    let (ry, rx) = ((y + dy) % FFT2, (x + dx) % FFT2);
                    let r = m.fine_walk[ry * FFT2 + rx];
                    let p = v > 0.0;
                    inter += (p && r) as usize;
                    union += (p || r) as usize;
                }
            }
            iou[i] = inter as f32 / union.max(1) as f32;
        }
    }
    Some(iou)
}

// ---------- the whole clip ----------

/// Kills (unless already known), map and agent for a saved clip. Never fails:
/// what can't be told is left empty.
pub fn analyze_clip(ffmpeg: &Path, clip: &Path, refs_dir: &Path, kills: Option<u32>, on_spawn: &dyn Fn(&std::process::Child)) -> ClipInfo {
    let kills = kills.or_else(|| super::killfeed::count_in_clip(ffmpeg, clip, on_spawn).ok());
    let mut info = ClipInfo { kills, ..Default::default() };
    let Some(refs) = refs(refs_dir) else { return info };
    let frames = frames(ffmpeg, clip, 8, on_spawn);
    // Agent: confident frames vote, weighted by how clearly they win. After you die
    // the HUD shows the teammate you spectate, so a clip can show several agents;
    // yours is the one on screen the most.
    let mut votes: HashMap<String, f32> = HashMap::new();
    for f in &frames {
        if let Some((name, score, margin)) = agent_in(f, &refs) {
            if score > 0.25 && margin > 0.1 {
                *votes.entry(name).or_default() += margin;
            }
        }
    }
    let mut ranked: Vec<(String, f32)> = votes.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    info.agent = match ranked.as_slice() {
        [(a, n), rest @ ..] if rest.first().map_or(true, |(_, m)| n - m > 0.05) => Some(a.clone()),
        _ => None,
    };
    // Map: sum of IoU over frames; must clearly beat the runner-up.
    if !refs.maps.is_empty() {
        let mut planner = FftPlanner::<f32>::new();
        let mut total = vec![0.0f32; refs.maps.len()];
        let mut used = 0;
        for f in frames.iter().take(4) {
            if let Some(s) = maps_in(f, &refs, &mut planner) {
                total.iter_mut().zip(&s).for_each(|(t, v)| *t += v);
                used += 1;
            }
        }
        if used > 0 {
            let mut order: Vec<usize> = (0..total.len()).collect();
            order.sort_by(|a, b| total[*b].total_cmp(&total[*a]));
            let best = total[order[0]] / used as f32;
            let second = order.get(1).map(|&i| total[i] / used as f32).unwrap_or(0.0);
            if best > 0.5 && best - second > 0.04 {
                info.map = Some(refs.maps[order[0]].name.clone());
            }
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_keeps_average() {
        let mut g = Grid::new(8, 8, 0.0);
        for y in 0..8 {
            for x in 4..8 {
                g.v[y * 8 + x] = 1.0;
            }
        }
        let s = g.resize(4, 4);
        assert_eq!((s.w, s.h), (4, 4));
        assert!((s.at(0, 0) - 0.0).abs() < 1e-6 && (s.at(3, 3) - 1.0).abs() < 1e-6);
        let avg: f32 = s.v.iter().sum::<f32>() / 16.0;
        assert!((avg - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fft_correlation_finds_the_shift() {
        let mut planner = FftPlanner::<f32>::new();
        let n = 64;
        // A "map": an L-shaped floor on a -1 canvas.
        let mut m = vec![Complex32::new(-1.0, 0.0); n * n];
        for y in 10..40 {
            for x in 10..16 {
                m[y * n + x] = Complex32::new(1.0, 0.0);
            }
        }
        for y in 34..40 {
            for x in 10..35 {
                m[y * n + x] = Complex32::new(1.0, 0.0);
            }
        }
        let mut mf = m.clone();
        fft2(&mut planner, &mut mf, n, false);
        // The patch is the corner of the L, cut out at (8, 30).
        let mut patch = Grid::new(14, 14, 0.0);
        for y in 0..14 {
            for x in 0..14 {
                patch.v[y * 14 + x] = if m[(30 + y) * n + 8 + x].re > 0.0 { 1.0 } else { WALL };
            }
        }
        let (score, dy, dx) = correlate(&mut planner, &patch, n, &[(mf.as_slice(), 0)])[0];
        assert_eq!((dy, dx), (30, 8), "score {score}");
    }

    /// Real clips: CLIPIT_VALORANT_REFS=<refs dir> CLIPIT_INFO_CLIP=<mp4> cargo test analyze_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn analyze_real() {
        let refs = std::env::var("CLIPIT_VALORANT_REFS").expect("CLIPIT_VALORANT_REFS");
        for clip in std::env::var("CLIPIT_INFO_CLIP").expect("CLIPIT_INFO_CLIP").split(';') {
            let t = std::time::Instant::now();
            let info = analyze_clip(Path::new("ffmpeg"), Path::new(clip), Path::new(&refs), Some(0), &|_| {});
            println!("{clip}: {info:?} in {:?}", t.elapsed());
        }
    }
}
