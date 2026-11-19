# Changelog

## 0.17.0

### New
- **Game art for everyone, no key needed.** Covers, banners, logos and icons for 44 popular games now come with
  Clipit: Counter-Strike 2, Minecraft, VALORANT, GTA V and VI, Growtopia, Fortnite, Apex Legends, League of Legends,
  Rocket League, Overwatch 2, Warzone, Black Ops 6, Rainbow Six Siege, Roblox, Dota 2, PUBG, Rust, Destiny 2, Marvel
  Rivals, THE FINALS, Tarkov, Battlefield 6, ARC Raiders, Genshin Impact, Dead by Daylight, Lethal Company, Among Us,
  Fall Guys, TF2, Halo Infinite, Forza Horizon 5, EA FC, Elden Ring, Deadlock, RDR2, Cyberpunk 2077, Brawlhalla,
  Splitgate, Hunt: Showdown, REMATCH, Delta Force, PEAK and R.E.P.O. A SteamGridDB key (Settings > Storage) is now only
  needed for other games.
- **Folders get a proper icon** instead of their first letter (the Desktop folder gets a screen).
- **Filter menu for every game.** The new Filter button sorts clips by length (under 30 s, 30 s to 2 min, over
  2 min), size and date. The VALORANT filters (map, agent, kills) now show in the VALORANT folder; to have them on
  every page, tick "VALORANT filters" in the menu or turn on Settings > General > VALORANT filters everywhere. It's
  remembered.
- **Updates install themselves.** Clipit now looks for updates in the background (even while it's in the tray),
  downloads them, and installs them the next time you're not playing or recording and Clipit is tucked away in the
  tray. It restarts quietly in the tray, and "What's new" shows the next time you open it. On by default; turn it off
  in Settings > About > Install updates automatically.
- **A new first-time setup.** A welcome over drifting game covers and slow moving light, then a few short questions:
  what you'll use Clipit for (VALORANT, other games, recording sessions, or just your clip library), the look, how
  Clipit pops up (with live previews of each), hotkeys, sound, recording and whether it starts with Windows. Clipit
  sets itself up around the answers. Every step has "Skip, use default", and the welcome can skip everything.

### Feels better
- **Smooth scrolling**: a mouse wheel's notch glides instead of jumping.
- **Clip cards arrive one after another** when you open a page, lift on a soft spring when you hover, and their
  picture eases in a little closer. Buttons sink slightly when pressed. As before, nothing replays when you only
  change a setting or a filter.
- **New headings** in Bricolage Grotesque, rounder cards and dialogs, and a sidebar that floats in the window.
- Reduce motion turns all of it off.

## 0.16.1

### New
- **Any clip length.** Settings > Recording > Replay length has a **Custom** option: type the seconds you want, like
  45, anywhere from 10 seconds to 20 minutes. The RAM estimate and the Save button follow it.

### Calmer
- **Pages no longer re-animate every time you click.** The fade-in plays once when you go to another section (like
  Clips to Settings). Switching Settings tabs, changing a setting, filtering, a new clip arriving, or going from one
  game to another just updates in place. The fade itself is shorter and softer.
- The Settings tab icons no longer tilt on hover.

### Fixed
- The speed slider stayed open after closing a clip, and was already open on the next one.

## 0.16.0

### New look
- **Clipit has a new design**, inspired by BitChord's clean, Apple-Music-like style: neutral greys, the DM Sans
  typeface, big bold page titles, a calm sidebar with coloured icons, pill-shaped buttons, iOS-style switches and
  segmented controls, and settings in rounded grouped lists. Light mode follows the same look.
- **The recorder is a floating pill** at the bottom of the window, like a music player's: replay status, Save clip and
  Record are always one click away.

### Fixed
- **Recording works again.** "Capture didn't start, so recording couldn't begin" came from recordings still using the
  old recorder, which had to start up from scratch. Recordings now run on Clipit's own engine: they start instantly
  (the first second comes from the replay buffer), stop in a fraction of a second, and keep going if the engine has
  to restart.
- **The save chime plays the moment you press the key**, instead of after the clip is written. If saving then fails,
  you hear the error sound.

## 0.15.0

### New: Clipit's own recording engine
- **Clips are recorded by Clipit itself now**, not by ffmpeg. A native engine (C++) captures the game with Windows'
  capture, converts and scales on the graphics card, and encodes with your GPU's hardware encoder (NVIDIA, AMD or
  Intel). Frames never leave the graphics card.
- **Saving is instant.** The clip is written straight from memory, with no extra program started: a 25 s clip saves in
  about a third of a second, three times faster than before.
- **Never an old clip.** Frames keep coming at a steady rate even when the game doesn't draw (menus, loading
  screens), so the buffer always reaches the moment you press. If capture ever gets stuck, the engine restarts on its
  own within a few seconds **and keeps the replay buffer**, so a restart no longer means an empty clip.
- **Lighter.** No ffmpeg running while you play: about 40% less memory in our tests.
- Settings > Recording > Recorder lets you go back to the classic recorder. If the new engine fails three times in a
  row, Clipit switches to the classic one by itself (until the next start) and tells you.
- Long recordings (the Record button) and AV1 still use the classic recorder for now.

## 0.14.0

### New
- **Slow motion slider.** Click the speed button under a clip for a slider from 0.1× to 2× in 0.1 steps, with quick
  picks for 0.2×, 0.5×, 1×, 1.5× and 2×. Scroll on the button or press **[** / **]** to change it by 0.1. The speed
  stays as you move between clips.
- **Smooth speed ramps.** Changing speed eases into the new speed like a velocity edit (a slow start, a quick middle and
  a soft landing) instead of jumping. Settings > Appearance > Reduce motion makes it instant.
- **Even sound in slow motion.** Below 1× the sound is levelled: quiet parts are brought up, loud shots and explosions
  don't blow out, and the pitch stays normal. At normal speed the sound is untouched.

## 0.13.3

Bug fixes and stability.

### Fixed
- **Watching a clip while the hand cam is being added no longer lags.** The clip shows a spinner ("Adding hand cam…")
  on its thumbnail, and opening it shows a short wait instead of a stuttering video. It plays on its own as soon as
  the hand cam version is ready.
- **Reading a VALORANT clip after saving is much lighter.** The map and agent are read from about one frame a second
  instead of every frame (about 8× less work, same results), so a clip you're watching doesn't stutter meanwhile.
  The thumbnail shows "Reading clip…" while it runs.
- **Opening an older clip no longer lags while its kills are read.** Reading waits until you pause, so playback
  stays smooth.

### New
- **Settings > Recording > Restart recorder.** If clipping lags or stops working, this starts capture, audio and the
  hand cam fresh, like reopening Clipit. A recording in progress is saved first.

## 0.13.2

### Fixed
- **"Couldn't save the clip: ... being used by another process (os error 32)".** Right after a clip is written,
  another program (often antivirus) can open it for a moment, and since 0.13.1 saves finish fast enough to run into
  that. Clipit now waits it out (up to 15 s, in the background) instead of failing. The same goes for exports and
  renaming a clip.
- **Clips caught by that error are no longer deleted.** A clip that was fully saved but left as a ".mp4.part" file is
  now recovered when Clipit starts; only half-written files are removed.

## 0.13.1

### Fixed
- **Clips save in about a second again, even mid-match.** Saving ran at a low priority, so a busy game could hold it
  up for many seconds (over a minute on a fully loaded PC), and on slower PCs it could look like nothing was clipped.
  Saving now gets its fair share of the CPU: a 30 s clip takes under a second while you play.
- **The clip button and clip hotkey no longer lag.** Saving starts in the background, so pressing the key or button
  never holds up Clipit's window, and the clip still ends exactly where you pressed. The button says "Saving clip…"
  right away.

## 0.13.0

### New
- **Folders.** Press **+** next to Folders in the sidebar to make your own ("Funny moments", "Clutches"...). They're
  real folders in your clips folder. Rename or delete (when empty) from the folder's page or Settings > Storage.
- **Select several clips.** Tick a clip's corner, or Ctrl/Shift+click, then Favorite, Move to..., Copy (paste into
  Discord), Compress or Delete them all at once. Ctrl+A selects the page, Esc clears.
- **Space saver.** VALORANT clips with no kills are made smaller (light / balanced / small) while you're away from the
  PC and no game runs; it stops the moment you come back. A notice (and a Windows notification) says how much was
  cleared, and the Compressed clips page lets you undo any of them while the originals are kept (7 days by default).
  Favorites and edited clips are left alone. All of it is configurable in Settings > Space saver.
- **Back button** (top left), plus the mouse's back button and Alt+Left.
- **Tray menu**: Save clip, Start/Stop recording, View clips, Open clips folder, Settings, and Clipit's CPU and RAM.
- **No repeated footage**: clip twice in a row and the second clip starts where the first ended (at least 5 s).
  Settings > Recording.
- **What's new** card after an update.
- **ffmpeg installs by itself** in first-time setup, with progress, and Clipit restarts on its own when it's ready.

### Faster
- **Kill marks appear instantly on new clips.** Kills are noted (just the time, no video) the moment they happen while
  you play; a saved clip takes the ones inside it and the rest are forgotten. The clip's killfeed no longer has to be
  read after saving. Older clips get their marks while the PC is idle, and opening one reads it right away.
- **Clips end where you pressed.** On PCs that record a bit behind the game, Clipit waits (up to 3 s) for that moment,
  so you never press twice. If your PC keeps falling behind, it tells you once.

### Fixed
- Alt+Z with the app window open (behind another window or minimized) turned it into the quick view showing Settings.
  It now brings the window forward, and the quick view always opens on your clips.

### Feels better
- Small animations on buttons, cards, pages and settings, and dialogs open over a blurred background.
  Settings > Appearance > Reduce motion turns them off (Windows' own setting does too).

## 0.12.3

- **Hand cam back to how 0.12.0 worked** (the camera is added to saved clips, not drawn live), which fixes the
  dropped frames and hangs some people had in 0.12.1–0.12.2.
- **Fix: saving with a hand cam was slow.** The clip now saves instantly like any other; the camera is added a few
  seconds later in the background (on the graphics card with NVIDIA) and the file is swapped in when it's ready.
- **Fix: hand cam clips were much bigger.** The camera version keeps the clip's own bitrate, so it's about the same size.
- Kept from 0.12.2: webcams are asked for a mode they can send smoothly.

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
