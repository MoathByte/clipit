//! Favorites, renaming, and Start-with-Windows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---------- clip info (kills, map, agent) ----------

pub use crate::capture::valorant::ClipInfo;

/// What's known about each clip (path, lowercased → info), in clip-info.json.
/// Older versions kept only kill counts in kills.json; those are read once.
pub struct ClipMeta {
    file: PathBuf,
    map: Mutex<BTreeMap<String, ClipInfo>>,
}

impl ClipMeta {
    pub fn load(file: PathBuf) -> Self {
        let mut map: BTreeMap<String, ClipInfo> = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if map.is_empty() {
            let old = file.with_file_name("kills.json");
            if let Some(kills) = std::fs::read_to_string(&old).ok().and_then(|s| serde_json::from_str::<BTreeMap<String, u32>>(&s).ok()) {
                for (k, v) in kills {
                    map.insert(k, ClipInfo { kills: Some(v), ..Default::default() });
                }
            }
        }
        Self { file, map: Mutex::new(map) }
    }

    pub fn snapshot(&self) -> BTreeMap<String, ClipInfo> {
        self.map.lock().unwrap().clone()
    }

    pub fn get_in(map: &BTreeMap<String, ClipInfo>, path: &str) -> ClipInfo {
        map.get(&key(path)).cloned().unwrap_or_default()
    }

    /// Merges what's newly known (empty fields keep what was there).
    pub fn set(&self, path: &str, info: &ClipInfo) -> Result<ClipInfo, String> {
        let mut map = self.map.lock().unwrap();
        let e = map.entry(key(path)).or_default();
        if info.kills.is_some() {
            e.kills = info.kills;
        }
        if info.map.is_some() {
            e.map = info.map.clone();
        }
        if info.agent.is_some() {
            e.agent = info.agent.clone();
        }
        e.auto |= info.auto;
        let out = e.clone();
        self.save(&map)?;
        Ok(out)
    }

    pub fn moved(&self, from: &str, to: Option<&str>) {
        let mut map = self.map.lock().unwrap();
        if let Some(k) = map.remove(&key(from)) {
            if let Some(to) = to {
                map.insert(key(to), k);
            }
            let _ = self.save(&map);
        }
    }

    fn save(&self, map: &BTreeMap<String, ClipInfo>) -> Result<(), String> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string(map).map_err(|e| e.to_string())?;
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())
    }
}

// ---------- favorites / reviewed ----------

/// A saved set of clip paths (favorites, reviewed clips), stored lowercased
/// because Windows paths are case-insensitive.
pub struct PathSet {
    file: PathBuf,
    set: Mutex<BTreeSet<String>>,
}

pub type Favorites = PathSet;

fn key(path: &str) -> String {
    path.to_lowercase()
}

impl PathSet {
    /// Adds or removes several paths with a single save.
    pub fn set_many(&self, paths: &[String], on: bool) -> Result<(), String> {
        let mut set = self.set.lock().unwrap();
        let mut changed = false;
        for p in paths {
            changed |= if on { set.insert(key(p)) } else { set.remove(&key(p)) };
        }
        if changed {
            self.save(&set)?;
        }
        Ok(())
    }

    pub fn load(file: PathBuf) -> Self {
        let set = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str::<BTreeSet<String>>(&s).ok())
            .unwrap_or_default();
        Self { file, set: Mutex::new(set) }
    }

    fn save(&self, set: &BTreeSet<String>) -> Result<(), String> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(set).map_err(|e| e.to_string())?;
        // Write then rename so a crash never leaves a half-written file.
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    pub fn contains(&self, path: &str) -> bool {
        self.set.lock().unwrap().contains(&key(path))
    }

    pub fn snapshot(&self) -> BTreeSet<String> {
        self.set.lock().unwrap().clone()
    }

    pub fn set(&self, path: &str, favorite: bool) -> Result<(), String> {
        let mut set = self.set.lock().unwrap();
        let changed = if favorite { set.insert(key(path)) } else { set.remove(&key(path)) };
        if changed {
            self.save(&set)?;
        }
        Ok(())
    }

    pub fn moved(&self, from: &str, to: &str) -> Result<(), String> {
        let mut set = self.set.lock().unwrap();
        if set.remove(&key(from)) {
            set.insert(key(to));
            self.save(&set)?;
        }
        Ok(())
    }

    pub fn is_favorite_in(set: &BTreeSet<String>, path: &str) -> bool {
        set.contains(&key(path))
    }
}

// ---------- rename ----------

/// Checks a new clip name (without extension) is a valid Windows file name.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("The name can't be empty".into());
    }
    if name.chars().count() > 180 {
        return Err("That name is too long".into());
    }
    if let Some(c) = name.chars().find(|c| r#"<>:"/\|?*"#.contains(*c) || c.is_control()) {
        return Err(format!("Names can't contain {c}"));
    }
    if name.ends_with('.') {
        return Err("Names can't end with a dot".into());
    }
    let upper = name.split('.').next().unwrap_or("").trim().to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    let numbered = (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.len() == 4
        && upper.as_bytes()[3].is_ascii_digit();
    if reserved.contains(&upper.as_str()) || numbered {
        return Err("Windows doesn't allow that name".into());
    }
    Ok(name.to_string())
}

/// Renames `path` to `new_name` + its original extension, in the same folder.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, String> {
    let name = validate_name(new_name)?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".into());
    let target = path.with_file_name(format!("{name}.{ext}"));
    if target == path {
        return Ok(target);
    }
    let same_file_other_case =
        target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if target.exists() && !same_file_other_case {
        return Err("A clip with that name already exists".into());
    }
    std::fs::rename(path, &target).map_err(|e| match e.raw_os_error() {
        Some(32) => "The clip is open in another program — close it and try again".to_string(),
        _ => format!("Couldn't rename: {e}"),
    })?;
    Ok(target)
}

// ---------- start with Windows ----------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Clipit";
/// The name used before the app was called Clipit.
#[cfg_attr(debug_assertions, allow(dead_code))]
const OLD_RUN_VALUE: &str = "Clipidia";

fn run_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\" --minimized", exe.display()))
}

/// Moves a "start with Windows" entry from the old app name to this exe.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub fn migrate_autostart() {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY) else { return };
    if key.get_value::<String, _>(OLD_RUN_VALUE).is_ok() {
        if let Ok(cmd) = run_command() {
            if key.set_value(RUN_VALUE, &cmd).is_ok() {
                let _ = key.delete_value(OLD_RUN_VALUE);
            }
        }
    }
}

pub fn autostart_enabled() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY)
        .map_err(|e| e.to_string())?;
    if on {
        key.set_value(RUN_VALUE, &run_command()?).map_err(|e| e.to_string())
    } else {
        match key.delete_value(RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(validate_name("  Ace on Ascent ").unwrap(), "Ace on Ascent");
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("what?").is_err());
        assert!(validate_name("ends.").is_err());
        assert!(validate_name("CON").is_err());
        assert!(validate_name("com1").is_err());
        assert!(validate_name("Comeback").is_ok());
        assert!(validate_name("1v5 clutch 😎").is_ok());
    }

    #[test]
    fn rename_keeps_extension_and_refuses_overwrite() {
        let dir = std::env::temp_dir().join(format!("clipit-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("Valorant 2026.07.06 - 06.43.28.03.DVR.mp4");
        let b = dir.join("taken.mp4");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let renamed = rename(&a, "My clutch").unwrap();
        assert_eq!(renamed.file_name().unwrap(), "My clutch.mp4");
        assert!(renamed.exists() && !a.exists());
        assert!(rename(&renamed, "taken").is_err());
        let case = rename(&renamed, "MY CLUTCH").unwrap();
        assert_eq!(case.file_name().unwrap(), "MY CLUTCH.mp4");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Touches the real HKCU Run key, then restores it. Run with --ignored.
    #[test]
    #[ignore]
    fn autostart_toggle_restores() {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let before = autostart_enabled();
        set_autostart(true).unwrap();
        let value: String = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN_KEY)
            .unwrap()
            .get_value(RUN_VALUE)
            .unwrap();
        assert!(value.ends_with("\" --minimized"), "{value}");
        set_autostart(false).unwrap();
        assert!(!autostart_enabled());
        set_autostart(false).unwrap(); // removing twice is fine
        if before {
            set_autostart(true).unwrap();
        }
    }

    /// Real Recycle Bin round trip (same matching logic as the restore_clip command).
    #[test]
    #[ignore]
    fn recycle_bin_restore() {
        let dir = std::env::temp_dir().join(format!("clipit-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("restore me.mp4");
        std::fs::write(&file, b"clip").unwrap();
        trash::delete(&file).unwrap();
        assert!(!file.exists());
        let item = trash::os_limited::list()
            .unwrap()
            .into_iter()
            .filter(|i| i.name == file.file_name().unwrap() && i.original_parent == dir)
            .max_by_key(|i| i.time_deleted)
            .expect("found in recycle bin");
        trash::os_limited::restore_all([item]).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"clip");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn favorites_roundtrip() {
        let file = std::env::temp_dir().join(format!("clipit-fav-{}.json", std::process::id()));
        let f = Favorites::load(file.clone());
        f.set(r"D:\Clips\A.mp4", true).unwrap();
        assert!(f.contains(r"d:\clips\a.mp4"));
        f.moved(r"D:\Clips\A.mp4", r"D:\Clips\B.mp4").unwrap();
        let again = Favorites::load(file.clone());
        assert!(again.contains(r"D:\Clips\B.mp4") && !again.contains(r"D:\Clips\A.mp4"));
        let _ = std::fs::remove_file(file);
    }
}
