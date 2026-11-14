# Changelog

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
