use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Stable 64-bit FNV-1a hash (std's hasher is not guaranteed stable between Rust versions).
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Locates ffmpeg: next to the app exe, on PATH, or in winget's install folders.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe = "ffmpeg.exe";

    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let p = dir.join(exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let winget = PathBuf::from(local).join("Microsoft").join("WinGet");
        let link = winget.join("Links").join(exe);
        if link.is_file() {
            return Some(link);
        }
        // e.g. Packages\Gyan.FFmpeg_...\ffmpeg-9.0.2-full_build\bin\ffmpeg.exe
        if let Ok(pkgs) = std::fs::read_dir(winget.join("Packages")) {
            for pkg in pkgs.flatten() {
                if !pkg.file_name().to_string_lossy().to_lowercase().contains("ffmpeg") {
                    continue;
                }
                if let Ok(builds) = std::fs::read_dir(pkg.path()) {
                    for b in builds.flatten() {
                        let p = b.path().join("bin").join(exe);
                        if p.is_file() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

fn thumb_name(clip: &Path) -> String {
    let meta = std::fs::metadata(clip).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = format!("{}|{}|{}", clip.to_string_lossy().to_lowercase(), size, mtime);
    format!("{:016x}.jpg", fnv1a(key.as_bytes()))
}

fn run_ffmpeg(ffmpeg: &Path, clip: &Path, seek: &str, out: &Path) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-ss", seek, "-i"])
        .arg(clip)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "5", "-f", "image2", "-y"])
        .arg(out);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    matches!(cmd.status(), Ok(s) if s.success())
        && std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false)
}

/// Returns a cached thumbnail path for `clip`, generating it with ffmpeg if needed.
pub fn thumbnail(ffmpeg: &Path, cache_dir: &Path, clip: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let out = cache_dir.join(thumb_name(clip));
    if out.is_file() {
        return Ok(out);
    }
    // Write to a temp file first so a half-written image is never served.
    let tmp = out.with_extension("tmp.jpg");
    let ok = run_ffmpeg(ffmpeg, clip, "2", &tmp) || run_ffmpeg(ffmpeg, clip, "0", &tmp);
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return Err("ffmpeg could not read this clip".into());
    }
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Turns raw 16-bit little-endian mono PCM into `bins` loudness peaks (0–255),
/// normalized to the clip's loudest moment so quiet clips are still readable.
pub fn peaks_from_pcm(pcm: &[u8], bins: usize) -> Vec<u8> {
    let n = pcm.len() / 2;
    if n == 0 || bins == 0 {
        return Vec::new();
    }
    let bins = bins.min(n);
    let mut peaks = vec![0u16; bins];
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]).unsigned_abs();
        let b = i * bins / n;
        if s > peaks[b] {
            peaks[b] = s;
        }
    }
    // Scale to the loud-but-normal level (98th percentile), not the single loudest
    // spike, so one explosion doesn't flatten the rest of the clip.
    let mut sorted = peaks.clone();
    sorted.sort_unstable();
    let p98 = sorted[(sorted.len() - 1) * 98 / 100] as f32;
    let max = sorted[sorted.len() - 1] as f32;
    let reference = p98.max(max * 0.1).max(1.0);
    peaks
        .iter()
        .map(|&p| ((p as f32 / reference).min(1.0).powf(0.6) * 255.0).round() as u8)
        .collect()
}

/// Audio waveform of `clip` as 1,000 peaks (0–255), cached next to the thumbnails.
/// Returns an empty list for clips without audio.
pub fn waveform(ffmpeg: &Path, cache_dir: &Path, clip: &Path) -> Result<Vec<u8>, String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let out = cache_dir.join(thumb_name(clip).replace(".jpg", ".wave2"));
    if let Ok(bytes) = std::fs::read(&out) {
        return Ok(bytes);
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(clip)
        .args(["-map", "0:a:0", "-vn", "-ac", "1", "-ar", "8000", "-f", "s16le", "-"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(Vec::new()); // no audio track (or unreadable) — the UI draws a flat line
    }
    let peaks = peaks_from_pcm(&output.stdout, 1000);
    let tmp = out.with_extension("tmp");
    if std::fs::write(&tmp, &peaks).is_ok() {
        let _ = std::fs::rename(&tmp, &out);
    }
    Ok(peaks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn peaks_are_normalized_per_bin() {
        let p = peaks_from_pcm(&pcm(&[0, 100, -200, 50, 1000, -1000, 0, 10]), 4);
        assert_eq!(p.len(), 4);
        assert_eq!(p[2], 255); // loudest bin
        assert!(p[1] > p[0] && p[1] <= p[2]);
        assert!(p[3] < p[0]);
        // One huge spike doesn't flatten everything else.
        let mut loud = vec![1000i16; 200];
        loud[100] = 30000;
        let q = peaks_from_pcm(&pcm(&loud), 200);
        assert_eq!(q[100], 255);
        assert!(q[0] > 120, "normal parts stay visible: {}", q[0]);
        assert_eq!(peaks_from_pcm(&pcm(&[i16::MIN]), 10), vec![255]); // no overflow, bins capped
        assert!(peaks_from_pcm(&[], 10).is_empty());
        assert_eq!(peaks_from_pcm(&pcm(&[0, 0, 0]), 3), vec![0, 0, 0]); // silence
    }
}
