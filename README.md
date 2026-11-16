<p align="center">
  <img src="app-icon.svg" width="96" height="96" alt="Clipit logo">
</p>

<h1 align="center">Clipit</h1>

<p align="center">
  A lightweight game clipper and clip library for Windows.<br>
  Keeps the last minute of your game in RAM, saves it with one key, and clips your VALORANT rounds for you.
</p>

<p align="center">
  <a href="https://github.com/MoathByte/clipit/releases/latest"><b>Download for Windows</b></a>
</p>

---

## What it does

**Clip**
- **Instant replay**: the last 15 s to 10 min of your game, always ready. Press your hotkey to save it.
- **Hotkeys** can be combinations (Alt+F9) or a single key (L, /, ]). Single keys only fire while a game is in front
  and still reach the game; they're read with Raw Input, so nothing is hooked and the key isn't taken from other apps.
- **Recording**: start and stop with another hotkey. Crash-safe: an interrupted recording is recovered on the next start.
- **Game window only** (nothing runs until a game is in focus, and pop-ups never end up in your clips) or **whole monitor**.
- **Hand cam** (optional): a webcam or phone camera added onto your saved clips. Drag it into place on a preview and
  pick size, shape (rectangle, rounded, circle), crop, border, mirror, rotation, zoom and a timing offset. Brightness,
  contrast, saturation, gamma, colour temperature and sharpness work with any camera; the camera's own settings window
  (exposure, white balance, focus) opens from Clipit too. The camera
  is recorded into its own small RAM buffer and only runs while a game is captured; the game recording itself is
  never changed until a clip is saved.
- Desktop audio and microphone, each on any device, with volume, mic noise suppression and optional separate tracks.
- 720p to native, 30–144 fps, H.264 / HEVC / AV1 on your graphics card.

**VALORANT**
- **Auto clips for highlights**: 3Ks with kills close together, and every 4K and ace from the whole round, saved as
  their own clips and tagged **Auto clipped** (with 3K / 4K / Ace badges). Everything is adjustable: fewest kills
  (2K to ace), how close together kills must be, whole-round clips for 4Ks and aces, seconds before and after, and the
  largest clip size. Deathmatch and Team Deathmatch are skipped.
- **Kill counts, map and agent** on every VALORANT clip: the map is read from the minimap (zoomed in, rotating or
  cut off by its circle), the agent from your ability icons. Filter your library by map, agent or kills (3K, 4K, aces).
- **Vanguard-friendly by design**: Clipit never reads game memory, never injects anything, never sends input and
  never takes screenshots for this. Kills are read from Clipit's own recording (the same Windows Graphics Capture
  that OBS and Discord use), as a tiny 4-frames-a-second copy of the killfeed corner; map and agent are read from
  the saved video file.

**Watch, edit, share**
- **Home** shows your newest clip in a big theatre-style player, your games above it and earlier clips below.
- **Quick view** (`Alt+Z` by default) pops Clipit over your game with a soft blur; click away and it gives focus back.
- Player with an audio waveform you can scrub, speed control, frame stepping and fullscreen. On VALORANT clips
  each of your kills is marked on the timeline: click a mark, or press N / P to jump between kills.
- **Edit & Send**: trim, crop to 9:16 / 1:1 / 4:5 / 4:3, compress under a size limit (10 / 50 / 500 MB or custom),
  then **Compress & Copy** to paste the file straight into Discord.
- **Review**: go through new clips one by one: keep, delete to the Recycle Bin, skip, undo.
- Favorites, rename, search, game artwork from SteamGridDB (optional free API key).
- **Library folders**: every folder in your clips folder is a game, and you can add other folders (another app's
  clips, an old drive) to show their clips too.

**Make it yours**
- Dark, Light or Match Windows, in three palettes: **Tally** (vermilion), **Volt** (chartreuse) and **Bubblegum** (pink).
- Settings grouped into tabs, with a Save button and a prompt if you leave with unsaved changes.
- **Automatic updates**: signed releases from this repository, installed when you click "Restart to update".

## Why it's light

Replay tools that buffer on disk rewrite a temp file all the time; users report terabytes written to their SSDs.
Clipit keeps the replay in RAM and writes each clip exactly once.

| | How |
|---|---|
| Capture + encode | Windows Graphics Capture → GPU scaling → the GPU's video encoder in one ffmpeg process; frames stay on the GPU (≈1–4% of one CPU core measured in-game) |
| Replay buffer | Already-encoded video in **RAM** (≈150 MB per minute at 1080p60); audio as raw PCM in RAM, only encoded when you save |
| Saving | Video is copied, not re-encoded; the MP4 index is reserved up front, so each clip is written **once, front to back** |
| Kill watching | 4 frames a second of a 770×290 corner, branched from the capture; detection takes ~0.5 ms per frame |
| Sync | Audio and video share the system clock; measured offset 1–2 ms |
| Idle | In game-window mode nothing runs without a game; ffmpeg can't outlive Clipit; after 2 minutes hidden the UI is unloaded (~30 MB total) |

While VALORANT is recorded with auto clips on, the replay buffer keeps enough to finish a highlight (the largest clip
plus 45 seconds; about 400 MB of RAM at the defaults).

## Install

1. Download `Clipit_x.y.z_x64-setup.exe` from the [latest release](https://github.com/MoathByte/clipit/releases/latest) and run it.
2. Clipit needs ffmpeg 8 or newer (for Windows Graphics Capture). If it's missing or too old, Clipit offers a
   one-click install through winget; or run `winget install Gyan.FFmpeg` yourself.
3. Open Clipit and follow the short setup: look, hotkeys, audio, what to capture and where clips go
   (default `Videos\Clipit`).

Windows 10/11, 64-bit. A graphics card with a hardware video encoder is recommended; a processor fallback exists.

## How the VALORANT detection works

- **Highlights**: a round ends 40 seconds after your last kill (longer than a round's end plus the buy phase).
  Then its kills are checked against your rules: streaks of kills close together, or the whole round for 4Ks and aces.
  More than six kills without a round break, or a "round" over 150 seconds, means Deathmatch-style play, and auto
  clips pause until three minutes pass without a kill.
- **Kills**: your own kills appear in the killfeed with a thin yellow border. Clipit finds yellow top and bottom edges
  on a teal/red bar that reaches the screen's right edge. Each entry stays up about five seconds and new ones are added
  at the bottom, so a kill is "more of your entries than are alive" or "one of yours below your lowest live one".
  Tuned on 40 real clips (33 kills, including multi-kills); walls, blonde hair, the gold "killed by" card and
  teammates' kills don't count.
- **Agent**: the four ability icons (C, Q, E, X) are compared with every agent's icons. After you die the HUD shows
  the teammate you spectate, so frames vote and the agent on screen the most wins; unclear clips stay untagged.
- **Map**: the minimap's flat grey floor is matched against every map layout, searching rotations and zoom levels with
  FFT correlation (coarse, then fine for the best few maps), then checking the overlap of floor areas. Several frames vote.
- Reference images (ability icons, map layouts) come from the free community API at valorant-api.com and are
  refreshed weekly, so new agents and maps are picked up without an update.

Clipit isn't endorsed by Riot Games. VALORANT is a trademark of Riot Games, Inc.

## Build from source

Requires Rust (rustup), Node.js 18+, Visual Studio C++ build tools and ffmpeg.

```bash
npm install
npm run dev      # development build
npm run build    # optimized exe + installer in src-tauri/target/release/bundle/nsis
```

Tests:

```bash
cd src-tauri
cargo test
# with real clips (optional)
CLIPIT_KILLS_CLIP=<clip.mp4> cargo test count_real_clip -- --ignored --nocapture
CLIPIT_VALORANT_REFS=<refs dir> CLIPIT_INFO_CLIP=<clip.mp4> cargo test analyze_real -- --ignored --nocapture
```

## Releasing (maintainers)

Releases are signed so installed copies can trust them. The private key lives outside the repo
(`%USERPROFILE%\.clipit\updater.key` or the `TAURI_SIGNING_PRIVATE_KEY` variable); the public key is in
`src-tauri/tauri.conf.json`.

1. Bump the version in `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml` and `package.json`.
2. Add a `## x.y.z` section to `CHANGELOG.md`.
3. `npm run release`: builds, signs, writes `latest.json` and publishes the GitHub release. Installed copies see
   it on their next start.

## Project layout

```
ui/                  index.html, styles.css, app.js  (plain HTML/JS, no framework)
src-tauri/src/
  main.rs            app setup, commands, hotkeys, tray, updater, folder watcher
  config.rs          settings (%APPDATA%\com.clipit.app\config.json)
  scanner.rs         finds clips, reads the date from "Game YYYY.MM.DD - HH.MM.SS" names
  media.rs           ffmpeg discovery, thumbnails, audio waveforms
  export.rs          trim / crop / compress
  keys.rs            single-key hotkeys (Raw Input, only while a game is in front)
  library.rs         favorites, reviewed, clip info (kills, map, agent), rename, start with Windows
  stream.rs          local HTTP server (127.0.0.1 + random token) for clips and images
  overlay.rs         quick-view backdrop and focus checks
  art.rs             SteamGridDB artwork
  capture/
    mod.rs           the recorder engine: replay, recording, round clips
    encoder.rs       ffmpeg capture pipeline (+ the killfeed branch)
    ts.rs            RAM replay buffer (MPEG-TS split at keyframes)
    audio.rs         WASAPI desktop + mic capture
    mux.rs           saving clips, live recording, named pipes
    killfeed.rs      your kills in the killfeed (and when), round tracking
    valorant.rs      map + agent recognition, reference images
    handcam.rs       hand cam: camera buffer, placement, adding it to clips
    game.rs, sound.rs
scripts/release.mjs  signed release → GitHub
```

## License

[MIT](LICENSE) © 2026 MoathByte
