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
    /// Saved automatically by Clipit (a 3K, 4K, ace…).
    pub auto: bool,
    /// Seconds into the clip where your kills happen, once read.
    pub kill_times: Option<Vec<f32>>,
    /// Length in seconds (read from the file's header; None if it couldn't be).
    pub duration: Option<f64>,
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
    /// Every folder shown in the library, the clips folder first.
    pub folders: Vec<Folder>,
}

#[derive(Serialize)]
pub struct Folder {
    pub path: String,
    pub exists: bool,
    pub clip_count: usize,
    /// The clips folder (where new clips are saved).
    pub main: bool,
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
    let duration = crate::media::duration_cached(path, meta.len(), modified_ms);
    Some(Clip {
        duration,
        kills: info.kills,
        map: info.map,
        agent: info.agent,
        auto: info.auto,
        kill_times: info.kill_times,
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

/// Folders that are never games.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('$') || name.eq_ignore_ascii_case("System Volume Information")
}

/// Videos in `dir` and its sub-folders (a few levels deep).
fn videos_in(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if depth > 0 && !skip_dir(&e.file_name().to_string_lossy()) {
                videos_in(&p, depth - 1, out);
            }
        } else if is_video(&p) {
            out.push(p);
        }
    }
}

/// The game a clip belongs to: the first folder under the library folder it's in.
/// Loose files are "Other" in the clips folder, or named after an extra folder.
pub fn game_of(path: &Path, folders: &[String]) -> String {
    for (i, f) in folders.iter().enumerate() {
        let root = Path::new(f);
        if let Ok(rel) = path.strip_prefix(root) {
            let mut parts = rel.components();
            let first = parts.next();
            return match (first, parts.next()) {
                (Some(dir), Some(_)) => dir.as_os_str().to_string_lossy().into_owned(),
                _ if i == 0 => "Other".into(),
                _ => root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Other".into()),
            };
        }
    }
    "Other".into()
}

/// Scans the clips folder plus any extra folders. Every folder directly inside one
/// is a game (even an empty one); clips in deeper folders count for that game.
pub fn scan(folders: &[String], flags: &Flags) -> Library {
    let root = folders.first().cloned().unwrap_or_default();
    let mut clips: Vec<Clip> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut games: Vec<Game> = Vec::new();
    let mut infos = Vec::new();
    for (i, folder) in folders.iter().enumerate() {
        let fp = Path::new(folder);
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(fp) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) && !skip_dir(&name) && !games.iter().any(|g| g.name.eq_ignore_ascii_case(&name)) {
                    games.push(Game { name, clip_count: 0, total_size: 0, latest_ms: 0 });
                }
            }
            let mut files = Vec::new();
            videos_in(fp, 4, &mut files);
            for p in files {
                if !seen.insert(p.to_string_lossy().to_lowercase()) {
                    continue; // an extra folder inside another one
                }
                let game = game_of(&p, &folders[..=i]);
                // Folder names differ only by case: one game.
                let game = games.iter().find(|g| g.name.eq_ignore_ascii_case(&game)).map(|g| g.name.clone()).unwrap_or(game);
                if let Some(c) = clip_from(&p, &game, flags) {
                    count += 1;
                    clips.push(c);
                }
            }
        }
        infos.push(Folder { path: folder.clone(), exists: fp.is_dir(), clip_count: count, main: i == 0 });
    }

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
    // Most recently played first; empty folders last, by name.
    games.sort_by(|a, b| b.latest_ms.cmp(&a.latest_ms).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    Library {
        root_exists: Path::new(&root).is_dir(),
        root,
        games,
        clips,
        folders: infos,
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
    fn games_are_folders_and_extra_folders_count() {
        let dir = std::env::temp_dir().join(format!("clipit-scan-{}", std::process::id()));
        let main = dir.join("Clips");
        let extra = dir.join("Old clips");
        for d in [main.join("Valorant").join("Highlights"), main.join("Empty game"), extra.join("Valorant"), extra.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in [main.join("Valorant").join("a.mp4"), main.join("Valorant").join("Highlights").join("b.mp4"), main.join("loose.mp4"), extra.join("Valorant").join("c.mp4"), extra.join("d.mkv")] {
            std::fs::write(f, b"x").unwrap();
        }
        let (fav, rev, meta) = (BTreeSet::new(), BTreeSet::new(), std::collections::BTreeMap::new());
        let folders = vec![main.to_string_lossy().into_owned(), extra.to_string_lossy().into_owned()];
        let lib = scan(&folders, &Flags { favorites: &fav, reviewed: &rev, meta: &meta });
        let count = |n: &str| lib.games.iter().find(|g| g.name == n).map(|g| g.clip_count);
        assert_eq!(count("Valorant"), Some(3));
        assert_eq!(count("Empty game"), Some(0));
        assert_eq!(count("Other"), Some(1));
        assert_eq!(count("Old clips"), Some(1));
        assert_eq!(lib.folders.iter().map(|f| f.clip_count).collect::<Vec<_>>(), vec![3, 2]);
        std::fs::remove_dir_all(&dir).unwrap();
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
