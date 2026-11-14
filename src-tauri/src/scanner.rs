use crate::library::Favorites;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "mov", "webm"];

#[derive(Serialize, Clone)]
pub struct Clip {
    pub path: String,
    pub file_name: String,
    pub game: String,
    pub size: u64,
    /// File modified time, unix milliseconds.
    pub modified_ms: u64,
    /// Local recording time parsed from the file name ("YYYY-MM-DDTHH:MM:SS").
    pub recorded: Option<String>,
    /// Name the user gave the clip (None for dated "Game YYYY.MM.DD - ..." names).
    pub title: Option<String>,
    pub favorite: bool,
    /// Made by Clipit's editor ("... (edited).mp4").
    pub edited: bool,
    /// Already gone through Review mode.
    pub reviewed: bool,
    /// Your kills in it, once counted (VALORANT clips).
    pub kills: Option<u32>,
    /// Map and agent, once recognised (VALORANT clips).
    pub map: Option<String>,
    pub agent: Option<String>,
}

/// Paths Clipit keeps per-clip flags for.
pub struct Flags<'a> {
    pub favorites: &'a BTreeSet<String>,
    pub reviewed: &'a BTreeSet<String>,
    pub meta: &'a std::collections::BTreeMap<String, crate::library::ClipInfo>,
}

/// True for files Clipit's editor made: "... (edited).mp4", "... (edited 2).mp4"
/// (and "(Clipit)" from v0.2).
pub fn is_edited(file_name: &str) -> bool {
    let stem = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let Some(open) = stem.rfind('(') else { return false };
    let inner = &stem[open + 1..];
    inner.ends_with(')') && (inner.starts_with("edited") || inner.starts_with("clipit") || inner.starts_with("clipidia"))
}

#[derive(Serialize, Clone)]
pub struct Game {
    pub name: String,
    pub clip_count: usize,
    pub total_size: u64,
    pub latest_ms: u64,
}

#[derive(Serialize)]
pub struct Library {
    pub root: String,
    pub root_exists: bool,
    pub games: Vec<Game>,
    pub clips: Vec<Clip>,
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.iter().any(|v| v.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Finds the timestamp "YYYY.MM.DD - HH.MM.SS" anywhere in the file name.
pub fn parse_recorded(name: &str) -> Option<String> {
    const PATTERN: &[u8] = b"####.##.## - ##.##.##";
    let bytes = name.as_bytes();
    if bytes.len() < PATTERN.len() {
        return None;
    }
    for start in 0..=bytes.len() - PATTERN.len() {
        let window = &bytes[start..start + PATTERN.len()];
        let matches = window.iter().zip(PATTERN).all(|(c, p)| match p {
            b'#' => c.is_ascii_digit(),
            other => c == other,
        });
        if matches {
            let s = std::str::from_utf8(window).ok()?;
            // "2026.07.06 - 06.43.28" -> "2026-07-06T06:43:28"
            return Some(format!(
                "{}-{}-{}T{}:{}:{}",
                &s[0..4], &s[5..7], &s[8..10], &s[13..15], &s[16..18], &s[19..21]
            ));
        }
    }
    None
}

/// A user-given title, if the file isn't a dated "Game YYYY.MM.DD - ..." name.
pub fn custom_title(file_name: &str, game: &str) -> Option<String> {
    let stem = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dated = parse_recorded(file_name).is_some()
        && stem.to_lowercase().starts_with(&game.to_lowercase());
    if dated {
        None
    } else {
        Some(stem)
    }
}

pub fn clip_from(path: &Path, game: &str, flags: &Flags) -> Option<Clip> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let path_str = path.to_string_lossy().into_owned();
    let info = crate::library::ClipMeta::get_in(flags.meta, &path_str);
    Some(Clip {
        kills: info.kills,
        map: info.map,
        agent: info.agent,
        favorite: Favorites::is_favorite_in(flags.favorites, &path_str),
        reviewed: Favorites::is_favorite_in(flags.reviewed, &path_str),
        edited: is_edited(&file_name),
        title: custom_title(&file_name, game),
        path: path_str,
        recorded: parse_recorded(&file_name),
        file_name,
        game: game.to_string(),
        size: meta.len(),
        modified_ms,
    })
}

pub fn scan(root: &str, flags: &Flags) -> Library {
    let root_path = Path::new(root);
    let mut clips = Vec::new();

    if let Ok(entries) = std::fs::read_dir(root_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let game = entry.file_name().to_string_lossy().into_owned();
                if let Ok(files) = std::fs::read_dir(&path) {
                    for f in files.flatten() {
                        let p = f.path();
                        if is_video(&p) {
                            if let Some(c) = clip_from(&p, &game, flags) {
                                clips.push(c);
                            }
                        }
                    }
                }
            } else if is_video(&path) {
                if let Some(c) = clip_from(&path, "Other", flags) {
                    clips.push(c);
                }
            }
        }
    }

    let mut games: Vec<Game> = Vec::new();
    for c in &clips {
        match games.iter_mut().find(|g| g.name == c.game) {
            Some(g) => {
                g.clip_count += 1;
                g.total_size += c.size;
                g.latest_ms = g.latest_ms.max(c.modified_ms);
            }
            None => games.push(Game {
                name: c.game.clone(),
                clip_count: 1,
                total_size: c.size,
                latest_ms: c.modified_ms,
            }),
        }
    }
    // Most recently played first.
    games.sort_by(|a, b| b.latest_ms.cmp(&a.latest_ms));

    Library {
        root: root.to_string(),
        root_exists: root_path.is_dir(),
        games,
        clips,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dated_names() {
        assert_eq!(
            parse_recorded("Valorant 2026.07.06 - 06.43.28.03.DVR.mp4").as_deref(),
            Some("2026-07-06T06:43:28")
        );
        assert_eq!(parse_recorded("random.mp4"), None);
    }

    #[test]
    fn custom_titles() {
        assert_eq!(custom_title("Valorant 2026.07.06 - 06.43.28.03.DVR.mp4", "Valorant"), None);
        assert_eq!(custom_title("Desktop 2026.10.15 - 18.06.26.02.DVR.mp4", "Desktop"), None);
        assert_eq!(custom_title("Ace on Ascent.mp4", "Valorant").as_deref(), Some("Ace on Ascent"));
        // Edited copies keep the date, so they sort next to the original.
        assert_eq!(custom_title("Valorant 2026.07.06 - 06.43.28.03 (edited).mp4", "Valorant"), None);
    }

    #[test]
    fn edited_names() {
        assert!(is_edited("Valorant 2026.07.06 - 06.43.28.03 (edited).mp4"));
        assert!(is_edited("Valorant 2026.07.06 - 06.43.28.03 (edited 3).mp4"));
        assert!(is_edited("Valorant 2026.07.06 - 06.43.28.03 (Clipit).mp4"));
        assert!(!is_edited("Valorant 2026.07.06 - 06.43.28.03.DVR.mp4"));
        assert!(!is_edited("Ace (round 3).mp4"));
    }
}
