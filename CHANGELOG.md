# Changelog

## 0.12.2

- **Fix: choppy hand cam (2–5 fps) on some webcams.** Clipit now asks the camera for a size and frame rate it can
  deliver smoothly (using its MJPEG mode when it has one). Before, many USB webcams sent their biggest uncompressed
  picture, which they can only send a few times a second. If a camera refuses, it falls back to its defaults.

## 0.12.1

- **Hand cam is now live, like a camera source in OBS**: the camera is drawn onto the game while it records, so saving
  a clip is instant again (no more waiting while the clip was re-encoded) and clips are their normal size again.
- **Better sync**: the camera and the game share one clock as they're recorded. If your camera is still ahead,
  **Delay camera** holds it back.
- **Live preview**: Settings → Hand cam shows the camera live, exactly as it will look in clips (shape, border,
  turn, zoom and adjustments).
- With the hand cam on, recording uses a little more CPU (under one core); with it off nothing changes.

## 0.12.0

- **Hand cam**: add a camera (a webcam, or your phone through an app like Iriun Webcam) to your clips. Pick the camera,
  drag it where you want it on a preview, resize it, and choose rectangle, rounded or circle, a crop (16:9 to 9:16),
  a border, mirror, upside down, rotation, zoom and a timing offset. Adjust brightness, contrast, saturation,
  gamma, colour temperature and sharpness with a live preview, or open the camera's own settings window (exposure,
  white balance, focus…) like OBS's Configure Video. It's only added to saved clips (replays, auto
  clips and recordings) and the camera only runs while a game is captured. Off by default: **Settings → Hand cam**.
- **More folders in your library**: show clips from other folders too (Settings → Storage → Also show clips from).
- **Every folder is a game**: each folder in your clips folder shows up as a game, even when it's empty, and clips in
  folders inside it count for that game.

## 0.11.0

- **Kills on the timeline**: VALORANT clips show a marker for each of your kills on the player's timeline (and in the
  trim editor). Click one to jump to just before the kill, or press **N** / **P** for the next or previous kill.
  Older clips get their markers the first time you open them.
- **Single-key hotkeys**: set Save clip, Record or Quick view to one key like **L**, **/** or **]**. Single keys only
  work while a game is in front and the game still gets the key, so typing in Discord or a browser never triggers
  them. Combinations (Alt+Z, Ctrl+F9…) work as before.

## 0.10.0

- **Auto clips are now highlights**: 3Ks with kills close together, plus every 4K and ace from the whole round, instead
  of every round with a kill. All of it is adjustable in the new **Settings → Auto clips** tab: fewest kills (2K to ace),
  how close kills must be, whole-round clips for 4Ks and aces, time before and after, and the largest clip size.
- **Deathmatch is skipped**: Deathmatch, Team Deathmatch and other modes without rounds no longer produce huge clips.
- **Auto clipped** section in the sidebar, an "Auto clipped" tag and 3K / 4K / Ace badges on clips.
- **Older ffmpeg versions are detected**: Clipit picks an ffmpeg that can record games (8 or newer) and, if there isn't
  one, explains it and offers a one-click update through winget. Fixes "Capture stopped: Filter not found".
- A clear summary of what auto clips will do, and estimates of clip length and RAM for your quality.

## 0.9.0

- **Round clips for VALORANT**: Clipit saves one clip for every round you get kills in, from a few seconds before
  your first kill to after your last. A round only ends after 35 seconds without a kill, so aces with long pauses stay
  in one clip. Kills are read from Clipit's own recording (a tiny 4-frames-a-second copy of the killfeed corner),
  never from your screen or the game.
- **Map and agent tags**: every VALORANT clip is tagged with the map (read from the minimap, even zoomed in or
  rotating) and the agent you played (from your ability icons). Filter clips by map, agent and kills.
- **Automatic updates**: Clipit checks GitHub for signed releases and installs them when you click
  "Restart to update".
- Kill counts, map and agent are kept in `clip-info.json` (older kill counts are carried over).

## 0.8.0

- Renamed to Clipit, with a new logo.
- Kill counts read from saved clips.
- Themes: Dark, Light or Match Windows, in Tally, Volt and Bubblegum.
- Settings reorganised into tabs with grouped rows and a Save button.

## 0.7.0

- Theatre-style Home with the newest clip on top.
- The quick view opens on Home after 3 minutes away.

## 0.6.0

- Clipit records: instant replay in RAM, recording, desktop audio and microphone.
