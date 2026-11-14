use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Folder clips are saved into. Each game gets its own sub-folder.
    pub clips_root: String,
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
    /// Seconds kept before the first kill of a round.
    pub kill_before_secs: u32,
    /// Seconds kept after the last kill of a round.
    pub kill_after_secs: u32,
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
            kill_before_secs: 8,
            kill_after_secs: 4,
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
