use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Folder clips are saved into. Each game gets its own sub-folder.
    pub clips_root: String,
    /// More folders to show clips from (e.g. another app's clips folder).
    /// Their sub-folders are games too; Clipit only saves into `clips_root`.
    pub extra_folders: Vec<String>,
    /// Optional SteamGridDB API key used to fetch game artwork.
    pub steamgriddb_key: String,
    /// Global hotkey that opens the quick view.
    pub hotkey: String,
    /// Where edited/compressed clips are saved. Empty = next to the original.
    pub export_dir: String,
    /// The first-run setup (hotkeys, audio, capture) has been completed.
    pub setup_done: bool,
    /// "both" (Alt+Z = quick view, tray = app), "quick" or "app".
    pub window_mode: String,
    /// Quick view entrance: "blur", "dim" or "none".
    pub animation: String,
    /// 0–100.
    pub blur_strength: u32,
    /// "dark", "light" or "system".
    pub theme_mode: String,
    /// Colour theme: "tally", "volt" or "bubblegum".
    pub theme: String,
    /// Look for a new version at start (installing always waits for you).
    pub auto_update: bool,
    pub capture: Capture,
}

/// `Videos\Clipit` in the user's profile (the setup lets people pick another).
pub fn default_clips_root() -> String {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Videos").join("Clipit").to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clips_root: default_clips_root(),
            extra_folders: Vec::new(),
            steamgriddb_key: String::new(),
            hotkey: "Alt+Z".into(),
            export_dir: String::new(),
            setup_done: false,
            window_mode: "both".into(),
            animation: "blur".into(),
            blur_strength: 60,
            theme_mode: "dark".into(),
            theme: "tally".into(),
            auto_update: true,
            capture: Capture::default(),
        }
    }
}

/// Clipit's own recorder (instant replay + recording).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Capture {
    /// Keep an instant-replay buffer running.
    pub enabled: bool,
    pub replay_secs: u32,
    /// "native", "1440", "1080" or "720" (height; never upscales).
    pub resolution: String,
    pub fps: u32,
    /// "high", "balanced" or "small".
    pub quality: String,
    /// "h264", "hevc" or "av1".
    pub codec: String,
    /// "auto" or an ffmpeg encoder name.
    pub encoder: String,
    /// "game" (only the game window) or "monitor".
    pub target: String,
    /// Monitor id ("\\.\DISPLAY1"); empty = main monitor.
    pub monitor: String,
    /// "all" detected games, or only the "selected" ones.
    pub games_mode: String,
    pub games: Vec<GameRule>,
    pub cursor: bool,
    pub desktop_audio: bool,
    /// Empty = Windows default (follows it).
    pub desktop_device: String,
    pub desktop_volume: u32,
    pub mic: bool,
    pub mic_device: String,
    pub mic_volume: u32,
    pub noise_suppression: bool,
    /// Also save desktop and mic as their own audio tracks (besides the mix).
    pub separate_tracks: bool,
    /// Chime when a clip is saved / recording starts.
    pub sounds: bool,
    pub clip_hotkey: String,
    pub record_hotkey: String,
    /// Count kills, and tag map + agent, in saved VALORANT clips (reads the saved file).
    pub count_kills: bool,
    /// Save a clip of every round you get kills in (VALORANT). Reads the killfeed
    /// from Clipit's own capture, never the screen or the game.
    pub auto_clip_kills: bool,
    /// Seconds kept before the first kill of a highlight.
    pub kill_before_secs: u32,
    /// Seconds kept after the last kill of a highlight.
    pub kill_after_secs: u32,
    /// Fewest kills in a streak to auto clip (2–5).
    pub auto_min_kills: u32,
    /// Kills further apart than this (seconds) start a new streak.
    pub auto_max_gap_secs: u32,
    /// Rounds with at least this many kills are clipped whole, however spread out
    /// (4 = 4Ks and aces, 5 = aces only, 0 = never).
    pub auto_whole_round_from: u32,
    /// Don't auto clip Deathmatch, Team Deathmatch and other modes without rounds.
    pub auto_skip_deathmatch: bool,
    /// Largest auto clip (MB); longer highlights keep their end.
    pub auto_max_mb: u32,
    /// A camera (hand cam) added onto saved clips.
    pub handcam: HandCam,
}

/// Hand cam: a camera recorded next to the game and added onto every saved clip
/// (never shown live, never in the replay buffer itself).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct HandCam {
    pub enabled: bool,
    /// DirectShow camera name, e.g. "Iriun Webcam".
    pub device: String,
    /// Capture height: 480, 720 or 1080.
    pub height: u32,
    /// 30 or 60.
    pub fps: u32,
    /// Top-left corner of the camera box, as a fraction of the frame (0–1).
    pub x: f64,
    pub y: f64,
    /// Width of the camera box, as a fraction of the frame width.
    pub size: f64,
    /// "16:9", "4:3", "1:1", "3:4" or "9:16" (the picture is cropped to it).
    pub aspect: String,
    /// "rect", "rounded" or "circle".
    pub shape: String,
    /// "none", "white", "black" or "accent".
    pub border: String,
    pub flip_h: bool,
    pub flip_v: bool,
    /// 0, 90, 180 or 270 degrees clockwise.
    pub rotate: u32,
    /// Zoom into the middle of the picture (1 = whole picture).
    pub zoom: f64,
    /// Shifts the camera in time to match the game (ms; positive = later).
    pub delay_ms: i32,
}

impl Default for HandCam {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            height: 720,
            fps: 30,
            x: 0.73,
            y: 0.68,
            size: 0.25,
            aspect: "16:9".into(),
            shape: "rounded".into(),
            border: "none".into(),
            flip_h: false,
            flip_v: false,
            rotate: 0,
            zoom: 1.0,
            delay_ms: 0,
        }
    }
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            enabled: true,
            replay_secs: 60,
            resolution: "1080".into(),
            fps: 60,
            quality: "high".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            target: "game".into(),
            monitor: String::new(),
            games_mode: "all".into(),
            games: Vec::new(),
            cursor: false,
            desktop_audio: true,
            desktop_device: String::new(),
            desktop_volume: 100,
            mic: true,
            mic_device: String::new(),
            mic_volume: 100,
            noise_suppression: false,
            separate_tracks: true,
            sounds: true,
            clip_hotkey: String::new(),
            record_hotkey: String::new(),
            count_kills: true,
            auto_clip_kills: true,
            kill_before_secs: 6,
            kill_after_secs: 3,
            auto_min_kills: 3,
            auto_max_gap_secs: 20,
            auto_whole_round_from: 4,
            auto_skip_deathmatch: true,
            auto_max_mb: 300,
            handcam: HandCam::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GameRule {
    /// Executable name, lower case.
    pub exe: String,
    pub name: String,
    pub enabled: bool,
}

impl Config {
    /// The clips folder first, then the extra folders (no blanks or repeats).
    pub fn library_folders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in std::iter::once(&self.clips_root).chain(self.extra_folders.iter()) {
            let mut f = f.trim().trim_end_matches(['\\', '/']).to_string();
            if f.ends_with(':') {
                f.push('\\'); // "D:" alone means "the current folder on D:"
            }
            if !f.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&f)) {
                out.push(f);
            }
        }
        out
    }

    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_get_new_defaults() {
        let old = r#"{"clips_root":"D:\\x","steamgriddb_key":"","hotkey":"Alt+Z","export_dir":""}"#;
        let c: Config = serde_json::from_str(old).unwrap();
        assert_eq!(c.clips_root, r"D:\x");
        assert!(!c.setup_done);
        assert_eq!(c.capture.replay_secs, 60);
        assert_eq!(c.capture.resolution, "1080");
        assert_eq!(c.window_mode, "both");
        let partial = r#"{"capture":{"fps":120}}"#;
        let c: Config = serde_json::from_str(partial).unwrap();
        assert_eq!(c.capture.fps, 120);
        assert!(c.capture.enabled);
        assert!(c.capture.count_kills);
    }
}
