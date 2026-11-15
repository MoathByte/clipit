# Changelog

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
