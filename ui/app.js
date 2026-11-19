"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel, root = document) => root.querySelector(sel);

// Saved under the old app name before: carry the keys over once.
try {
  for (const k of ["session", "reviewRange", "export"]) {
    const old = localStorage.getItem("clipidia." + k);
    if (old !== null && localStorage.getItem("clipit." + k) === null) localStorage.setItem("clipit." + k, old);
    localStorage.removeItem("clipidia." + k);
  }
} catch {}

// Last theme used, applied before the first paint (the config arrives a moment later).
try {
  const t = JSON.parse(localStorage.getItem("clipit.theme") || "null");
  if (t) {
    const dark = t.mode === "system" ? window.matchMedia("(prefers-color-scheme: dark)").matches : t.mode !== "light";
    document.documentElement.dataset.mode = dark ? "dark" : "light";
    document.documentElement.dataset.theme = t.theme || "tally";
  }
} catch {}
const main = $("#main");

const state = {
  lib: null,              // { root, root_exists, games, clips }
  status: null,
  config: null,
  view: { name: "home" }, // home | all | game | settings
  art: {},                // game name -> GameArt
  thumbs: new Map(),      // clip path -> asset url
  search: "",
  sort: "new",
  list: [],               // clips currently shown
  player: null,           // { list, index }
};

// ---------- helpers ----------

const DAY_MS = 86400000;

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

function fmtSize(bytes) {
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(1) + " GB";
  if (bytes >= 1e6) return Math.round(bytes / 1e6) + " MB";
  return Math.max(1, Math.round(bytes / 1e3)) + " KB";
}


const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "long", month: "long", day: "numeric", year: "numeric" });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const shortFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric" });

function dayLabel(t) {
  const d = new Date(t);
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const that = new Date(d); that.setHours(0, 0, 0, 0);
  const diff = Math.round((today - that) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return dayFmt.format(d);
}

function toast(msg, isError = false) {
  const el = document.createElement("div");
  el.className = "toast" + (isError ? " error" : "");
  el.textContent = msg;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), 4200);
}

// Every local file (clips, thumbnails, art) is served by Clipit's own local
// server (stream.rs) — Tauri's asset protocol leaked memory and dropped images.
const fileUrl = (path) => state.streamBase + encodeURIComponent(path);
const assetUrl = (p) => (p && state.streamBase ? fileUrl(p) : null);

// ---------- data ----------

async function loadLibrary() {
  try {
    const lib = await invoke("scan_library");
    for (const c of lib.clips) {
      const t = c.recorded ? new Date(c.recorded).getTime() : NaN;
      c.t = Number.isFinite(t) ? t : c.modified_ms;
    }
    state.lib = lib;
  } catch (e) {
    toast("Couldn't read clips folder: " + e, true);
    state.lib = { root: "", root_exists: false, games: [], clips: [] };
  }
  loadArt();
}

let artErrorShown = false;
function loadArt() {
  for (const g of state.lib.games) {
    if (state.art[g.name]) continue;
    state.art[g.name] = {}; // placeholder so we only request once
    invoke("get_game_art", { name: g.name })
      .then((art) => {
        state.art[g.name] = art;
        onArtLoaded(g.name);
      })
      .catch((e) => {
        delete state.art[g.name];
        if (!artErrorShown) { artErrorShown = true; toast("Artwork: " + e, true); }
      });
  }
}

function onArtLoaded(name) {
  renderSidebar();
  const v = state.view;
  if (v.name === "home") renderHomeArt(name);
  if (v.name === "game" && v.game === name) renderHero();
}

async function loadStatus() {
  try {
    state.status = await invoke("get_status");
    state.config = await invoke("get_config");
  } catch (e) { toast(String(e), true); }
  renderHotkeyChip();
}

// ---------- sidebar ----------

function gameIcon(name) {
  const art = state.art[name] || {};
  const src = assetUrl(art.icon) || assetUrl(art.grid);
  if (src) return `<img class="game-icon" src="${src}" alt="">`;
  return `<span class="game-icon">${esc(name.slice(0, 1).toUpperCase())}</span>`;
}

function renderSidebar() {
  const games = state.lib ? state.lib.games : [];
  $("#games-list").innerHTML = games.map((g) => `
    <button class="nav-item" data-game="${esc(g.name)}">
      ${gameIcon(g.name)}<span class="name">${esc(g.name)}</span><span class="count">${g.clip_count}</span>
    </button>`).join("");
  $("#all-count").textContent = state.lib ? state.lib.clips.length : "";
  const favs = state.lib ? state.lib.clips.filter((c) => c.favorite).length : 0;
  $("#fav-count").textContent = favs || "";
  const autos = state.lib ? state.lib.clips.filter((c) => c.auto).length : 0;
  $("#auto-count").textContent = autos || "";
  const toReview = state.lib ? reviewQueue(DAY_MS).length : 0;
  $("#review-count").textContent = toReview || "";
  highlightNav();
}

function highlightNav() {
  const v = state.view;
  document.querySelectorAll(".nav-item").forEach((el) => {
    const active = (el.dataset.nav && el.dataset.nav === v.name) || (v.name === "game" && el.dataset.game === v.game);
    el.classList.toggle("active", !!active);
  });
}

function renderHotkeyChip() {
  const chip = $("#hotkey-chip");
  const hk = state.status && state.status.hotkey;
  if (!hk) { chip.innerHTML = ""; return; }
  chip.className = "hotkey-hint" + (hk.registered ? "" : " bad");
  chip.innerHTML = hk.registered ? `<kbd>${esc(fmtKey(hk.hotkey))}</kbd> opens Clipit` : `<kbd>${esc(fmtKey(hk.hotkey))}</kbd> is taken by another app`;
  chip.title = hk.error || "";
}

document.querySelector(".sidebar").addEventListener("click", (e) => {
  const item = e.target.closest(".nav-item");
  if (!item) return;
  if (item.dataset.game) go({ name: "game", game: item.dataset.game });
  else if (item.dataset.nav === "settings") openSettingsPage();
  else if (item.dataset.nav) go({ name: item.dataset.nav });
});

// ---------- routing ----------

// Settings live in the app window: from the quick view, switch to it.
function openSettingsPage() {
  go({ name: "settings" });
  const mode = state.config && state.config.window_mode;
  if (!isAppMode() && mode !== "quick") invoke("open_app_window").catch(() => {});
}

function go(view, opts = {}) {
  // Leaving Settings with unsaved changes asks first.
  if (state.view.name === "settings" && view.name !== "settings" && settingsDirty()) {
    confirmLeaveSettings().then((ok) => { if (ok) { dropSettingsDraft(); go(view, opts); } });
    return;
  }
  if (state.view.name === "settings" && view.name !== "settings") dropSettingsDraft();
  if (!opts.back && state.view && !sameView(state.view, view)) {
    navStack.push({ view: state.view, scroll: main.scrollTop });
    if (navStack.length > 50) navStack.shift();
  }
  if (!sameView(state.view, view)) { sel.paths.clear(); sel.anchor = null; }
  state.view = view;
  state.search = "";
  state.filterMap = state.filterAgent = "";
  state.filterKills = 0;
  main.scrollTop = 0;
  render();
  syncSelection();
  if (opts.back && opts.scroll) requestAnimationFrame(() => (main.scrollTop = opts.scroll));
  animatePage();
  updateBackBtn();
}

function render() {
  highlightNav();
  if (!state.lib) return;
  releaseMainVideos();
  const v = state.view;
  if (v.name === "settings") {
    renderSettings();
    openSettings(); // fresh device list + hotkey status
    return;
  }
  if (v.name === "review") return renderReview();
  if (v.name === "compressed") return renderCompressed();
  if (!state.lib.root_exists || state.lib.clips.length === 0) return renderEmpty();
  if (v.name === "home") return renderHome();
  if (v.name === "all" || v.name === "favorites" || v.name === "auto") return renderClipsPage(null);
  if (v.name === "game") {
    if (!state.lib.games.some((g) => g.name === v.game)) { state.view = { name: "home" }; return render(); }
    return renderClipsPage(v.game);
  }
}

function emptyHint() {
  const c = (state.config && state.config.capture) || {};
  if (c.enabled && c.clip_hotkey) return `Press <kbd>${esc(fmtKey(c.clip_hotkey))}</kbd> in a game to save the last ${esc(LENGTHS.find((l) => l[0] === c.replay_secs)?.[1] || "moments")}. Clips show up here right away.`;
  return "Clips in your clips folder show up here. Turn on instant replay in Settings to start saving your own.";
}

function renderEmpty() {
  const missing = !state.lib.root_exists;
  main.innerHTML = `
    <div class="empty">
      <h2>${missing ? "Clips folder not found" : "No clips yet"}</h2>
      <p>${missing
        ? `Clipit couldn't find <code>${esc(state.lib.root || "(not set)")}</code>. Choose your clips folder in Settings.`
        : emptyHint()}</p>
      <button class="btn primary" id="empty-settings">Open settings</button>
    </div>`;
  $("#empty-settings").onclick = () => openSettingsPage();
}

// ---------- home ----------

function posterArt(name) {
  const art = state.art[name] || {};
  const src = assetUrl(art.grid);
  if (src) return `<img src="${src}" alt="">`;
  return `<span class="poster-fallback">${esc(name.slice(0, 1).toUpperCase())}</span>`;
}

// "Just now", "12 min ago", "Today at 2:14 AM", "Yesterday at …"
function whenText(t) {
  const mins = Math.floor((Date.now() - t) / 60000);
  if (mins < 1) return "Just now";
  if (mins < 60) return `${mins} min ago`;
  return `${dayLabel(t)} at ${timeFmt.format(new Date(t))}`;
}

const HOME_EARLIER = 24;

// Home: a compact games strip, the newest clip big (theatre style), earlier clips below.
function renderHome() {
  const { games, clips } = state.lib;
  const recent = [...clips].sort((a, b) => b.t - a.t).slice(0, HOME_EARLIER + 1);
  state.list = recent;
  const top = recent[0];
  const earlier = recent.slice(1);
  main.innerHTML = `
    <div class="page home">
      <section class="home-games" aria-label="Your games">
        <div class="section-label">Your games</div>
        <div class="game-strip">
          ${games.map((g) => `
            <button class="game-tile" data-game="${esc(g.name)}">
              <span class="game-tile-art" data-poster="${esc(g.name)}">${posterArt(g.name)}</span>
              <span class="game-tile-text"><span class="game-tile-name">${esc(g.name)}</span><span class="game-tile-sub">${g.clip_count} clip${g.clip_count === 1 ? "" : "s"} · ${fmtSize(g.total_size)}</span></span>
            </button>`).join("")}
        </div>
      </section>

      <section class="feature" data-path="${esc(top.path)}">
        <button class="feature-stage" id="feature-stage" title="Watch">
          <img class="feature-thumb" id="feature-thumb" alt="">
          <video class="feature-video" id="feature-video" muted playsinline preload="metadata" src="${videoUrl(top.path)}#t=0.1"></video>
          <span class="feature-play"><svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg></span>
        </button>
        <div class="feature-info">
          <div class="feature-text">
            <div class="feature-kicker">Latest clip · ${esc(whenText(top.t))}</div>
            <h1 class="feature-title" title="${esc(top.title || top.game)}">${esc(top.title || top.game)}</h1>
            <div class="feature-meta">${top.title ? esc(top.game) + " · " : ""}${esc(shortFmt.format(new Date(top.t)))} · ${fmtSize(top.size)}${tagText(top) ? " · " + esc(tagText(top)) : ""}${top.kills ? " · " + multiText(top.kills) : ""}${top.auto ? " · Auto clipped" : ""}${top.edited ? " · Edited" : ""}</div>
          </div>
          <div class="feature-actions">
            <button class="btn ghost fav-toggle${top.favorite ? " on" : ""}" id="feature-fav" title="Favorite">
              <svg viewBox="0 0 24 24"><path d="m12 3.5 2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.9l-5.2 2.7 1-5.8-4.3-4.1 5.9-.9z"/></svg> <span>${top.favorite ? "Favorited" : "Favorite"}</span>
            </button>
            <button class="btn ghost" id="feature-copy" title="Copy the file — paste it anywhere with Ctrl+V">
              <svg viewBox="0 0 24 24"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"/></svg> Copy
            </button>
            <button class="btn ghost" id="feature-reveal" title="Show in File Explorer">
              <svg viewBox="0 0 24 24"><path d="M3 6h7l2 2h9v11H3z"/></svg> Show in folder
            </button>
            <button class="btn primary" id="feature-edit" title="Trim, crop and compress">
              <svg viewBox="0 0 24 24"><path d="M6 3v12a3 3 0 0 0 3 3h12M3 6h12a3 3 0 0 1 3 3v12"/></svg> Edit &amp; send
            </button>
          </div>
        </div>
      </section>

      ${earlier.length ? `
        <section class="home-earlier">
          <div class="section-head"><h2>Earlier clips</h2><button class="link" id="home-all">All ${clips.length} clips</button></div>
          <div class="clip-area"><div class="clip-grid">${earlier.map((c, i) => clipCard(c, i + 1, { date: true, game: games.length > 1 })).join("")}</div></div>
        </section>` : ""}
    </div>`;
  main.querySelectorAll(".game-tile").forEach((el) => (el.onclick = () => go({ name: "game", game: el.dataset.game })));
  if ($("#home-all")) $("#home-all").onclick = () => go({ name: "all" });
  wireFeature(top);
  wireClips();
}

function wireFeature(c) {
  const stage = $("#feature-stage");
  const v = $("#feature-video");
  const thumb = $("#feature-thumb");
  // The small thumbnail shows at once; the full-size frame fades in over it.
  if (state.thumbs.has(c.path)) thumb.src = state.thumbs.get(c.path);
  else {
    invoke("get_thumbnail", { path: c.path })
      .then((p) => {
        const url = fileUrl(p);
        state.thumbs.set(c.path, url);
        if (thumb.isConnected) thumb.src = url;
      })
      .catch(() => {});
  }
  v.addEventListener("loadeddata", () => stage.classList.add("ready"), { once: true });
  // Hovering plays a silent preview; clicking opens the player with sound.
  stage.addEventListener("pointerenter", () => { if (overlayVisible() && !state.player) v.play().catch(() => {}); });
  stage.addEventListener("pointerleave", () => v.pause());
  stage.onclick = () => openPlayer(state.list, 0);
  $("#feature-edit").onclick = () => { openPlayer(state.list, 0, { play: false }); setEditor(true); };
  $("#feature-fav").onclick = () => toggleFavorite(c);
  $("#feature-copy").onclick = () => invoke("copy_file", { path: c.path }).then(() => toast("Copied — paste it anywhere with Ctrl+V")).catch((e) => toast(String(e), true));
  $("#feature-reveal").onclick = () => invoke("reveal_clip", { path: c.path }).catch((e) => toast(String(e), true));
}

// Stops and unloads videos on the page (the home preview) so Windows lets go of the file.
function releaseMainVideos() {
  main.querySelectorAll("video").forEach((v) => {
    v.pause();
    v.removeAttribute("src");
    v.load();
  });
}

function renderHomeArt(name) {
  main.querySelectorAll("[data-poster]").forEach((el) => {
    if (el.dataset.poster === name) el.innerHTML = posterArt(name);
  });
}

// ---------- clips page (game or all) ----------

// The folder actions shown on a folder's page.
function folderTools(game) {
  return `<div class="folder-tools"><button type="button" class="btn sm ghost" data-folder-rename="${esc(game)}" title="Rename this folder">Rename</button><button type="button" class="btn sm ghost" data-folder-delete="${esc(game)}" title="Delete this folder (only when it's empty)">Delete folder</button></div>`;
}
main.addEventListener("click", (e) => {
  const r = e.target.closest("[data-folder-rename]");
  if (r) return renameFolder(r.dataset.folderRename);
  const d = e.target.closest("[data-folder-delete]");
  if (d) return deleteFolder(d.dataset.folderDelete);
});

function renderClipsPage(game) {
  main.innerHTML = `
    ${game ? `<header class="hero" id="hero"></header>` : `<div class="page" style="padding-bottom:0"><h1>${{ favorites: "Favorites", auto: "Auto clipped" }[state.view.name] || "All clips"}</h1></div>`}
    <div class="toolbar">
      <label class="search">
        <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>
        <input id="search" type="text" placeholder="Search" value="${esc(state.search)}" spellcheck="false">
      </label>
      <span id="tag-filters"></span>
      <select class="select" id="sort">
        <option value="new">Newest first</option>
        <option value="old">Oldest first</option>
        <option value="big">Largest first</option>
      </select>
      <span class="result-count" id="result-count"></span>
      ${game ? folderTools(game) : ""}
    </div>
    <div class="clip-area" id="clip-area"></div>`;
  if (game) renderHero();
  renderTagFilters();
  $("#sort").value = state.sort;
  $("#search").oninput = (e) => { state.search = e.target.value; renderClipArea(); };
  $("#sort").onchange = (e) => { state.sort = e.target.value; renderClipArea(); };
  renderClipArea();
}

// Map / agent / kills filters, only when the clips on this page have tags.
function renderTagFilters() {
  const box = $("#tag-filters");
  if (!box) return;
  const v = state.view;
  let base = state.lib.clips;
  if (v.name === "game") base = base.filter((c) => c.game === v.game);
  if (v.name === "favorites") base = base.filter((c) => c.favorite);
  if (v.name === "auto") base = base.filter((c) => c.auto);
  const maps = [...new Set(base.map((c) => c.map).filter(Boolean))].sort();
  const agents = [...new Set(base.map((c) => c.agent).filter(Boolean))].sort();
  const anyKills = base.some((c) => c.kills > 0);
  if (!maps.length && !agents.length && !anyKills) { box.innerHTML = ""; return; }
  const sel = (id, all, items, value) => `<select class="select" id="${id}"><option value="">${all}</option>${items.map((m) => `<option value="${esc(String(m[0]))}"${String(m[0]) === String(value || "") ? " selected" : ""}>${esc(m[1])}</option>`).join("")}</select>`;
  box.className = "tag-filters";
  box.innerHTML =
    (maps.length ? sel("f-map", "All maps", maps.map((m) => [m, m]), state.filterMap) : "") +
    (agents.length ? sel("f-agent", "All agents", agents.map((a) => [a, a]), state.filterAgent) : "") +
    (anyKills ? sel("f-kills", "Any kills", [[1, "1+ kills"], [3, "3K or more"], [4, "4K or more"], [5, "Aces"]], state.filterKills) : "");
  const wire = (id, key, num) => { const el = $("#" + id); if (el) el.onchange = () => { state[key] = num ? Number(el.value) || 0 : el.value; renderClipArea(); }; };
  wire("f-map", "filterMap");
  wire("f-agent", "filterAgent");
  wire("f-kills", "filterKills", true);
}

function renderHero() {
  const hero = $("#hero");
  const name = state.view.game;
  const g = state.lib.games.find((x) => x.name === name);
  if (!hero || !g) return;
  const art = state.art[name] || {};
  const bg = assetUrl(art.hero) || assetUrl(art.grid);
  const logo = assetUrl(art.logo);
  hero.innerHTML = `
    <div class="hero-bg" style="${bg ? `background-image:url('${bg}')` : ""}"></div>
    <div class="hero-content">
      ${logo ? `<img class="hero-logo" src="${logo}" alt="${esc(name)}">` : `<h1>${esc(name)}</h1>`}
      <div class="hero-stats">${g.clip_count ? `${g.clip_count} clip${g.clip_count === 1 ? "" : "s"} · ${fmtSize(g.total_size)} · last clip ${shortFmt.format(new Date(g.latest_ms))}` : "No clips yet"}</div>
    </div>`;
}

function currentClips() {
  const v = state.view;
  let list = state.lib.clips;
  if (v.name === "game") list = list.filter((c) => c.game === v.game);
  if (v.name === "favorites") list = list.filter((c) => c.favorite);
  if (v.name === "auto") list = list.filter((c) => c.auto);
  if (state.filterMap) list = list.filter((c) => c.map === state.filterMap);
  if (state.filterAgent) list = list.filter((c) => c.agent === state.filterAgent);
  if (state.filterKills) list = list.filter((c) => (c.kills || 0) >= state.filterKills);
  const q = state.search.trim().toLowerCase();
  if (q) {
    list = list.filter((c) =>
      c.file_name.toLowerCase().includes(q) ||
      (c.title || "").toLowerCase().includes(q) ||
      c.game.toLowerCase().includes(q) ||
      (c.map || "").toLowerCase().includes(q) ||
      (c.agent || "").toLowerCase().includes(q) ||
      (c.auto && "auto clipped".includes(q)) ||
      dayLabel(c.t).toLowerCase().includes(q) ||
      shortFmt.format(new Date(c.t)).toLowerCase().includes(q));
  }
  list = [...list];
  if (state.sort === "old") list.sort((a, b) => a.t - b.t);
  else if (state.sort === "big") list.sort((a, b) => b.size - a.size);
  else list.sort((a, b) => b.t - a.t);
  return list;
}

function renderClipArea() {
  const area = $("#clip-area");
  if (!area) return;
  const list = currentClips();
  state.list = list;
  $("#result-count").textContent = `${list.length} clip${list.length === 1 ? "" : "s"}`;
  const multiGame = ["all", "favorites", "auto"].includes(state.view.name);
  let empty = `<div class="empty"><p>No clips match “${esc(state.search)}”.</p></div>`;
  if (!state.search.trim() && state.view.name === "favorites") {
    empty = `<div class="empty"><h2>No favorites yet</h2><p>Hover a clip and click the star, or press <kbd>F</kbd> while watching one.</p></div>`;
  }
  if (!state.search.trim() && state.view.name === "auto") {
    empty = `<div class="empty"><h2>No auto clips yet</h2><p>Play VALORANT with auto clips on (Settings, Auto clips) and your 3Ks, 4Ks and aces land here.</p></div>`;
  }
  area.innerHTML = list.length ? clipGrid(list, multiGame, state.sort !== "big") : empty;
  wireClips();
}

const killText = (k) => `${k} kill${k === 1 ? "" : "s"}`;
// "3K", "4K", "Ace" (5 kills in a round) — or "1 kill", "7 kills".
const multiText = (k) => (k === 5 ? "Ace" : k >= 2 && k <= 4 ? `${k}K` : killText(k));
const isValorant = (game) => String(game || "").trim().toLowerCase() === "valorant";

// opts: { date: show the date before the time, game: show the game name }
// ---------- clips still being worked on after saving ----------
// "handcam": the clip is being replaced by its hand cam version (don't play it yet);
// "reading": its map and agent are being read (playable, just a little busy).
const busy = new Map(); // path -> task
const BUSY_TEXT = { handcam: "Adding hand cam", reading: "Reading clip" };
const busyClass = (path) => (busy.get(path) === "handcam" ? " busy" : "");
const busyBadge = (path) => {
  const t = busy.get(path);
  return t ? `<span class="busy-badge"><span class="spin"></span>${BUSY_TEXT[t] || "Working"}…</span>` : "";
};

function setBusy(path, task) {
  if (task) busy.set(path, task); else busy.delete(path);
  // Cards on screen, in place (no re-render, so scrolling and hovers stay put).
  document.querySelectorAll(".thumb[data-path]").forEach((t) => {
    if (t.dataset.path !== path) return;
    t.classList.toggle("busy", task === "handcam");
    const old = t.querySelector(".busy-badge");
    if (old) old.remove();
    if (task) t.querySelector("img").insertAdjacentHTML("afterend", busyBadge(path));
  });
  const cur = curClip();
  if (cur && cur.path === path) syncPlayerBusy();
}

listen("clip-busy", (e) => setBusy(e.payload.path, e.payload.task));
invoke("busy_clips").then((list) => list.forEach(([p, t]) => setBusy(p, t))).catch(() => {});

function clipCard(c, i, opts) {
  const when = (opts.date ? shortFmt.format(new Date(c.t)) + " · " : "") + timeFmt.format(new Date(c.t));
  const tags = tagText(c);
  const meta = (c.title
    ? `<span class="clip-title" title="${esc(c.title)}">${esc(c.title)}</span><span class="clip-sub">${esc(when)}</span>`
    : `<span class="clip-time">${esc(when)}</span>${opts.game ? `<span class="clip-game">${esc(c.game)}</span>` : ""}`)
    + (tags ? `<span class="clip-tags">${esc(tags)}</span>` : "");
  return `
    <button class="clip${c.favorite ? " is-fav" : ""}${sel.paths.has(c.path) ? " selected" : ""}" data-i="${i}">
      <div class="thumb${busyClass(c.path)}" data-path="${esc(c.path)}">
        <img alt="">
        ${busyBadge(c.path)}
        <span class="sel-btn" role="button" title="Select (Ctrl+click, Shift+click for a range)"><svg viewBox="0 0 24 24"><path d="m5 12.5 4.5 4.5L19 7.5"/></svg></span>
        <span class="play"><svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg></span>
        ${c.edited ? `<span class="edited-badge">Edited</span>` : ""}
        <span class="fav-btn" role="button" title="Favorite"><svg viewBox="0 0 24 24"><path d="m12 3.5 2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.9l-5.2 2.7 1-5.8-4.3-4.1 5.9-.9z"/></svg></span>
        ${c.kills || c.auto ? `<span class="kills-badge">${c.auto ? `<span class="auto-dot" title="Auto clipped"></span>` : ""}${c.kills ? multiText(c.kills) : "Auto"}</span>` : ""}
        <span class="badge">${fmtSize(c.size)}</span>
      </div>
      <div class="clip-meta">${meta}</div>
    </button>`;
}

function clipGrid(list, showGame, groupByDay) {
  if (!groupByDay) return `<div class="clip-grid">${list.map((c, i) => clipCard(c, i, { date: true, game: showGame })).join("")}</div>`;
  let html = "";
  let open = false;
  let lastDay = null;
  list.forEach((c, i) => {
    const day = dayLabel(c.t);
    if (day !== lastDay) {
      if (open) html += "</div>";
      html += `<div class="day-label">${esc(day)}</div><div class="clip-grid">`;
      open = true;
      lastDay = day;
    }
    html += clipCard(c, i, { date: false, game: showGame });
  });
  if (open) html += "</div>";
  return html;
}

// Lazy thumbnails: load them when they come near the screen and unload them when
// they scroll far away, so a 1,000+ clip library doesn't keep every image in memory.
// Observes the cards (not the thumbnails inside), because off-screen cards use
// content-visibility and their insides aren't laid out.
const thumbObserver = new IntersectionObserver((entries) => {
  for (const entry of entries) {
    const thumb = entry.target.querySelector(".thumb");
    if (!thumb) continue;
    if (entry.isIntersecting) loadThumb(thumb);
    else unloadThumb(thumb);
  }
}, { root: main, rootMargin: "1200px 0px" });

function loadThumb(el) {
  const path = el.dataset.path;
  const img = el.querySelector("img");
  if (img.getAttribute("src")) return;
  el.dataset.want = "1";
  const show = (url) => {
    if (el.dataset.want !== "1") return; // scrolled away before it finished
    img.onload = () => el.classList.add("loaded");
    img.onerror = () => el.classList.add("failed");
    img.src = url;
  };
  if (state.thumbs.has(path)) return show(state.thumbs.get(path));
  el._show = show;
  thumbQueue.push(el);
  pumpThumbs();
}

// Thumbnail requests: a few at a time, newest first, and skip cards that have
// scrolled away — so a fast scroll never leaves the visible cards waiting.
const thumbQueue = [];
let thumbActive = 0;
function pumpThumbs() {
  while (thumbActive < 4 && thumbQueue.length) {
    const el = thumbQueue.pop();
    if (el.dataset.want !== "1" || !el.isConnected || el.dataset.pending === "1") continue;
    const path = el.dataset.path;
    if (state.thumbs.has(path)) { el._show(state.thumbs.get(path)); continue; }
    el.dataset.pending = "1";
    thumbActive++;
    invoke("get_thumbnail", { path })
      .then((p) => {
        const url = fileUrl(p);
        state.thumbs.set(path, url);
        el._show(url);
      })
      .catch(() => el.classList.add("failed"))
      .finally(() => {
        el.dataset.pending = "0";
        thumbActive--;
        pumpThumbs();
      });
  }
  if (thumbQueue.length > 400) thumbQueue.splice(0, thumbQueue.length - 400); // drop stale backlog
}

function unloadThumb(el) {
  el.dataset.want = "0";
  const img = el.querySelector("img");
  if (!img.getAttribute("src")) return;
  img.onload = img.onerror = null;
  img.removeAttribute("src");
  el.classList.remove("loaded");
}

function wireClips() {
  thumbObserver.disconnect(); // old cards are gone; don't keep observing them
  main.querySelectorAll(".clip").forEach((el) => thumbObserver.observe(el));
}

const videoUrl = fileUrl;

main.addEventListener("click", (e) => {
  const card = e.target.closest(".clip");
  if (!card) return;
  const i = Number(card.dataset.i);
  const clip = state.list[i];
  if (e.target.closest(".fav-btn")) {
    e.stopPropagation();
    if (clip) toggleFavorite(clip);
    return;
  }
  // Selecting: the tick, Ctrl/Shift+click, or any click once something is selected.
  if (clip && (e.target.closest(".sel-btn") || e.ctrlKey || e.metaKey || e.shiftKey || sel.paths.size)) {
    e.preventDefault();
    selectClip(i, e.shiftKey);
    return;
  }
  openPlayer(state.list, i);
});

// ---------- selecting several clips ----------

const sel = { paths: new Set(), anchor: null };

function selectClip(i, range) {
  const c = state.list[i];
  if (!c) return;
  if (range && sel.anchor != null && state.list[sel.anchor]) {
    const [a, b] = [Math.min(sel.anchor, i), Math.max(sel.anchor, i)];
    for (let k = a; k <= b; k++) sel.paths.add(state.list[k].path);
  } else if (sel.paths.has(c.path)) sel.paths.delete(c.path);
  else sel.paths.add(c.path);
  sel.anchor = i;
  syncSelection();
}

function clearSelection() {
  if (!sel.paths.size) return;
  sel.paths.clear();
  sel.anchor = null;
  syncSelection();
}

function selectedClips() {
  const byPath = new Map((state.lib ? state.lib.clips : []).map((c) => [c.path, c]));
  return [...sel.paths].map((p) => byPath.get(p) || state.list.find((c) => c.path === p)).filter(Boolean);
}

function syncSelection() {
  main.querySelectorAll(".clip").forEach((el) => {
    const c = state.list[Number(el.dataset.i)];
    el.classList.toggle("selected", !!c && sel.paths.has(c.path));
  });
  const n = sel.paths.size;
  document.body.classList.toggle("selecting", n > 0);
  $("#sel-bar").classList.toggle("on", n > 0);
  $("#sel-count").textContent = `${n} selected`;
  const clips = selectedClips();
  $("#sel-fav-label").textContent = clips.length && clips.every((c) => c.favorite) ? "Unfavorite" : "Favorite";
  if (!n) closeMoveMenu();
}

async function selAction(what, btn) {
  const clips = selectedClips();
  const paths = clips.map((c) => c.path);
  const plural = (n) => `${n} clip${n === 1 ? "" : "s"}`;
  try {
    if (what === "clear") return clearSelection();
    if (what === "all") { state.list.forEach((c) => sel.paths.add(c.path)); return syncSelection(); }
    if (!paths.length) return;
    if (what === "fav") {
      const on = !clips.every((c) => c.favorite);
      await invoke("favorite_clips", { paths, favorite: on });
      paths.forEach((p) => findClips(p).forEach((c) => (c.favorite = on)));
      renderSidebar();
      rerenderKeepScroll();
      syncSelection();
      toast(on ? `Added ${plural(paths.length)} to Favorites` : `Removed ${plural(paths.length)} from Favorites`);
    } else if (what === "move") {
      openMoveMenu(btn);
    } else if (what === "copy") {
      await invoke("copy_files", { paths });
      toast(`Copied ${plural(paths.length)}. Paste them in Discord or a folder with Ctrl+V.`);
    } else if (what === "compress") {
      const bg = (state.config && state.config.background) || {};
      const keep = bg.keep_days ? `The originals are kept for ${bg.keep_days} days, so you can undo it.` : "The originals are deleted afterwards.";
      if (!(await confirmDialog(`Compress ${plural(paths.length)}?`, `They're made smaller in the background (never while a game is running). ${keep}`, { yes: "Compress", danger: false }))) return;
      await invoke("compress_clips", { paths });
      clearSelection();
      toast(`Compressing ${plural(paths.length)} in the background`);
    } else if (what === "delete") {
      if (!(await confirmDialog(`Delete ${plural(paths.length)}?`, "They go to the Recycle Bin."))) return;
      const done = await invoke("delete_clips", { paths });
      clearSelection();
      await refreshLibrary();
      toast(`Deleted ${plural(done.length)}`);
    }
  } catch (e) { toast(String(e), true); }
}
document.querySelectorAll("[data-sel]").forEach((b) => (b.onclick = (e) => { e.stopPropagation(); selAction(b.dataset.sel, b); }));

// "Move to…": the folders in your clips folder, or a new one.
function openMoveMenu(btn) {
  const menu = $("#move-menu");
  const games = state.lib ? state.lib.games.map((g) => g.name) : [];
  menu.innerHTML = `<div class="menu-title">Move to</div>${games.map((g) => `<button class="menu-item" data-move="${esc(g)}">${gameIcon(g)}<span>${esc(g)}</span></button>`).join("")}
    <div class="menu-sep"></div><button class="menu-item" data-move-new="1"><svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14"/></svg><span>New folder…</span></button>`;
  menu.classList.remove("hidden");
  const r = btn.getBoundingClientRect();
  const h = menu.offsetHeight;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  menu.style.top = `${Math.max(8, r.top - h - 8)}px`;
  menu.querySelectorAll("[data-move]").forEach((b) => (b.onclick = () => moveSelected(b.dataset.move)));
  const nb = menu.querySelector("[data-move-new]");
  if (nb) nb.onclick = async () => {
    closeMoveMenu();
    const name = await newFolder();
    if (name) moveSelected(name);
  };
}
function closeMoveMenu() { $("#move-menu").classList.add("hidden"); }
document.addEventListener("pointerdown", (e) => { if (!e.target.closest("#move-menu") && !e.target.closest("[data-sel=move]")) closeMoveMenu(); });

async function moveSelected(folder) {
  closeMoveMenu();
  const paths = selectedClips().map((c) => c.path);
  try {
    await invoke("move_clips", { paths, folder });
    clearSelection();
    await refreshLibrary();
    toast(`Moved ${paths.length} clip${paths.length === 1 ? "" : "s"} to ${folder}`);
  } catch (e) { toast(String(e), true); }
}

// ---------- folders ----------

let promptResolve = null;
function promptDialog(title, text, opts = {}) {
  $("#prompt-title").textContent = title;
  $("#prompt-text").textContent = text;
  $("#prompt-yes").textContent = opts.yes || "OK";
  const input = $("#prompt-input");
  input.value = opts.value || "";
  input.placeholder = opts.placeholder || "";
  $("#prompt").classList.remove("hidden");
  setTimeout(() => { input.focus(); input.select(); }, 30);
  return new Promise((res) => (promptResolve = res));
}
function resolvePrompt(ok) {
  $("#prompt").classList.add("hidden");
  const v = $("#prompt-input").value.trim();
  if (promptResolve) { promptResolve(ok && v ? v : null); promptResolve = null; }
}
$("#prompt-yes").onclick = () => resolvePrompt(true);
$("#prompt-no").onclick = () => resolvePrompt(false);
$("#prompt-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); resolvePrompt(true); }
  if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); resolvePrompt(false); }
});

// Asks for a name and makes the folder. Returns its name (or null).
async function newFolder() {
  const name = await promptDialog("New folder", "A folder in your clips folder. Move clips into it from any page: select them, then Move to…", { yes: "Create", placeholder: "Funny moments" });
  if (!name) return null;
  try {
    const made = await invoke("create_folder", { name });
    await loadLibrary();
    renderSidebar();
    toast(`Made the folder ${made}`);
    return made;
  } catch (e) { toast(String(e), true); return null; }
}
$("#folder-new").onclick = async () => {
  const name = await newFolder();
  if (name) go({ name: "game", game: name });
};

async function renameFolder(from) {
  const to = await promptDialog("Rename folder", `The clips inside stay, with their favorites and tags.`, { yes: "Rename", value: from });
  if (!to || to === from) return;
  try {
    const made = await invoke("rename_folder", { from, to });
    await loadLibrary();
    renderSidebar();
    if (state.view.name === "game" && state.view.game === from) state.view = { name: "game", game: made };
    render();
    toast(`Renamed to ${made}`);
  } catch (e) { toast(String(e), true); }
}

async function deleteFolder(name) {
  if (!(await confirmDialog(`Delete the folder ${name}?`, "Only an empty folder can be deleted.", { yes: "Delete" }))) return;
  try {
    await invoke("delete_folder", { name });
    await loadLibrary();
    renderSidebar();
    if (state.view.name === "game" && state.view.game === name) go({ name: "home" });
    else render();
    toast(`Deleted the folder ${name}`);
  } catch (e) { toast(String(e), true); }
}

// ---------- back ----------

const navStack = [];
const sameView = (a, b) => a && b && a.name === b.name && a.game === b.game;
function updateBackBtn() {
  $("#nav-back").disabled = !state.player && !navStack.length;
}
function goBack() {
  if (!$("#prompt").classList.contains("hidden") || !$("#confirm").classList.contains("hidden")) return;
  if (state.player) { closePlayer(); return; }
  if (sel.paths.size) { clearSelection(); return; }
  const prev = navStack.pop();
  if (!prev) return updateBackBtn();
  go(prev.view, { back: true, scroll: prev.scroll });
}
$("#nav-back").onclick = goBack;
// The mouse's back button, Alt+Left.
window.addEventListener("mouseup", (e) => { if (e.button === 3) { e.preventDefault(); goBack(); } });
window.addEventListener("keydown", (e) => {
  if (!$("#prompt").classList.contains("hidden")) return; // its own keys
  const tag = e.target && e.target.tagName;
  const typing = tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA";
  if (e.altKey && e.key === "ArrowLeft") { e.preventDefault(); e.stopImmediatePropagation(); goBack(); return; }
  if (!$("#whats-new").classList.contains("hidden") && (e.key === "Escape" || e.key === "Enter")) {
    e.preventDefault(); e.stopImmediatePropagation(); closeWhatsNew(); return;
  }
  if (e.key === "Escape" && sel.paths.size && $("#confirm").classList.contains("hidden")) {
    e.preventDefault(); e.stopImmediatePropagation(); clearSelection(); return;
  }
  if (!typing && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && !state.player && main.querySelector(".clip")) {
    e.preventDefault(); e.stopImmediatePropagation();
    state.list.forEach((c) => sel.paths.add(c.path));
    syncSelection();
  }
}, true);

// Pages slide in (unless motion is reduced).
function animatePage(el = main) {
  el.classList.remove("page-in");
  void el.offsetWidth;
  el.classList.add("page-in");
}

// ---------- favorites ----------

function findClips(path) {
  const out = state.lib.clips.filter((c) => c.path === path);
  if (state.player) out.push(...state.player.list.filter((c) => c.path === path && !out.includes(c)));
  out.push(...state.list.filter((c) => c.path === path && !out.includes(c)));
  return out;
}

async function toggleFavorite(clip) {
  const favorite = !clip.favorite;
  const apply = (value) => {
    findClips(clip.path).forEach((c) => (c.favorite = value));
    main.querySelectorAll(".thumb").forEach((t) => {
      if (t.dataset.path === clip.path) t.closest(".clip").classList.toggle("is-fav", value);
    });
    const feature = main.querySelector(".feature");
    if (feature && feature.dataset.path === clip.path) {
      $("#feature-fav").classList.toggle("on", value);
      $("#feature-fav span").textContent = value ? "Favorited" : "Favorite";
    }
    syncPlayerFav();
    renderSidebar();
  };
  apply(favorite);
  try {
    await invoke("set_favorite", { path: clip.path, favorite });
    if (state.view.name === "favorites" && !favorite) rerenderKeepScroll();
  } catch (e) {
    apply(!favorite);
    toast("Couldn't update favorite: " + e, true);
  }
}

// ---------- review mode ----------

const REVIEW_RANGES = [
  ["24h", DAY_MS, "Last 24 hours"],
  ["3d", 3 * DAY_MS, "Last 3 days"],
  ["7d", 7 * DAY_MS, "Last 7 days"],
  ["all", Infinity, "Everything"],
];
let reviewRange = "24h";
try { reviewRange = localStorage.getItem("clipit.reviewRange") || "24h"; } catch {}
const rangeMs = () => (REVIEW_RANGES.find((r) => r[0] === reviewRange) || REVIEW_RANGES[0])[1];

// Clips still waiting for a decision. Favorites and your own edits count as kept.
function reviewQueue(ms) {
  const since = Date.now() - ms;
  return state.lib.clips
    .filter((c) => !c.reviewed && !c.favorite && !c.edited && (ms === Infinity || c.t >= since))
    .sort((a, b) => a.t - b.t);
}

function renderReview() {
  const queue = reviewQueue(rangeMs());
  const size = queue.reduce((s, c) => s + c.size, 0);
  const label = (REVIEW_RANGES.find((r) => r[0] === reviewRange) || REVIEW_RANGES[0])[2].toLowerCase();
  const s = state.reviewSummary;
  state.list = queue;
  main.innerHTML = `
    <div class="page">
      <h1>Review</h1>
      <p class="desc" style="color:var(--muted);margin:-8px 0 14px">Go through new clips one by one — keep the good ones, delete the rest to free up space.</p>
      <div class="chips" id="rv-range">
        ${REVIEW_RANGES.map(([k, , name]) => `<button class="chip${k === reviewRange ? " active" : ""}" data-range="${k}">${name}</button>`).join("")}
      </div>
      ${s ? `<div class="review-summary">Last review: kept <b>${s.kept}</b> · deleted <b>${s.deleted}</b>${s.deleted ? ` · freed <b>${fmtSize(s.freed)}</b>` : ""}${s.skipped ? ` · skipped ${s.skipped}` : ""}</div>` : ""}
      <div class="review-hero">
        <div>
          <div class="big">${queue.length}</div>
          <div class="sub">${queue.length ? `clip${queue.length === 1 ? "" : "s"} to review from the ${esc(label)} · ${fmtSize(size)}` : `All caught up — nothing new from the ${esc(label)} `}</div>
        </div>
        <button class="btn primary" id="rv-start" ${queue.length ? "" : "disabled"}>Start review</button>
      </div>
      <p class="desc" style="color:var(--faint);font-size:12px">Keys while reviewing: <kbd>K</kbd> keep · <kbd>D</kbd> delete · <kbd>S</kbd> skip · <kbd>F</kbd> favorite · <kbd>E</kbd> edit &amp; send · <kbd>Z</kbd> undo. Deleted clips go to the Recycle Bin.</p>
      ${queue.length ? `<h2>Up next</h2><div class="clip-area">${clipGrid(queue, true, true)}</div>` : ""}
    </div>`;
  main.querySelectorAll("#rv-range .chip").forEach((b) => (b.onclick = () => {
    reviewRange = b.dataset.range;
    try { localStorage.setItem("clipit.reviewRange", reviewRange); } catch {}
    renderReview();
  }));
  $("#rv-start").onclick = startReview;
  wireClips();
}

function startReview() {
  const queue = reviewQueue(rangeMs());
  if (!queue.length) return;
  state.reviewSummary = null;
  openPlayer(queue, 0);
  state.player.review = { kept: 0, deleted: 0, skipped: 0, freed: 0, history: [] };
  updateReviewBar();
}

function updateReviewBar() {
  const p = state.player;
  const r = p && p.review;
  $("#review-bar").classList.toggle("hidden", !r);
  updateHint();
  if (!r) return;
  $("#rv-pos").textContent = `Clip ${Math.min(p.index + 1, p.list.length)} of ${p.list.length}`;
  $("#rv-stats").textContent = `Kept ${r.kept} · Deleted ${r.deleted}${r.deleted ? ` (${fmtSize(r.freed)} freed)` : ""}${r.skipped ? ` · Skipped ${r.skipped}` : ""}`;
  $("#rv-undo").disabled = !r.history.length;
}

function reviewAdvance() {
  const p = state.player;
  if (p.index + 1 < p.list.length) { p.index += 1; showCurrentClip(); }
  else finishReview();
}

async function reviewKeep() {
  const p = state.player;
  if (!p || !p.review) return;
  const c = p.list[p.index];
  try {
    await invoke("set_reviewed", { paths: [c.path], reviewed: true });
  } catch (e) { return toast(String(e), true); }
  findClips(c.path).forEach((x) => (x.reviewed = true));
  p.review.kept += 1;
  p.review.history.push({ type: "keep", clip: c, index: p.index });
  renderSidebar();
  reviewAdvance();
}

function reviewSkip() {
  const p = state.player;
  if (!p || !p.review) return;
  p.review.skipped += 1;
  p.review.history.push({ type: "skip", index: p.index });
  reviewAdvance();
}

async function reviewDelete() {
  const p = state.player;
  if (!p || !p.review || ed.job) return;
  const c = p.list[p.index];
  releaseVideo();
  try {
    await invoke("delete_clip", { path: c.path });
  } catch (e) {
    toast("Couldn't delete: " + e, true);
    return showCurrentClip();
  }
  state.lib.clips = state.lib.clips.filter((x) => x.path !== c.path);
  recountGames();
  p.list.splice(p.index, 1);
  p.review.deleted += 1;
  p.review.freed += c.size;
  p.review.history.push({ type: "delete", clip: c, index: p.index });
  renderSidebar();
  if (p.index < p.list.length) showCurrentClip();
  else finishReview();
}

async function reviewUndo() {
  const p = state.player;
  if (!p || !p.review || !p.review.history.length) return;
  const h = p.review.history.pop();
  try {
    if (h.type === "keep") {
      await invoke("set_reviewed", { paths: [h.clip.path], reviewed: false });
      findClips(h.clip.path).forEach((x) => (x.reviewed = false));
      p.review.kept -= 1;
    } else if (h.type === "skip") {
      p.review.skipped -= 1;
    } else if (h.type === "delete") {
      await invoke("restore_clip", { path: h.clip.path });
      p.list.splice(h.index, 0, h.clip);
      if (!state.lib.clips.some((x) => x.path === h.clip.path)) state.lib.clips.push(h.clip);
      recountGames();
      p.review.deleted -= 1;
      p.review.freed -= h.clip.size;
      toast("Restored from the Recycle Bin");
    }
  } catch (e) {
    p.review.history.push(h);
    return toast("Couldn't undo: " + e, true);
  }
  p.index = h.index;
  renderSidebar();
  showCurrentClip();
}

function finishReview() {
  const r = state.player && state.player.review;
  closePlayer();
  if (r) {
    toast(r.deleted ? `Review done — freed ${fmtSize(r.freed)}` : "Review done");
    go({ name: "review" });
  }
}

$("#rv-keep").onclick = reviewKeep;
$("#rv-skip").onclick = reviewSkip;
$("#rv-delete").onclick = reviewDelete;
$("#rv-undo").onclick = reviewUndo;

function syncPlayerFav() {
  if (!state.player) return;
  const on = !!state.player.list[state.player.index].favorite;
  const btn = $("#p-fav");
  btn.classList.toggle("on", on);
  btn.querySelector("span").textContent = on ? "Favorited" : "Favorite";
}

// ---------- player ----------

const player = $("#player");
const video = $("#p-video");

try {
  const vol = parseFloat(localStorage.getItem("volume"));
  if (Number.isFinite(vol)) video.volume = vol;
  video.muted = localStorage.getItem("muted") === "1";
} catch {}
video.addEventListener("volumechange", () => {
  try {
    localStorage.setItem("volume", String(video.volume));
    localStorage.setItem("muted", video.muted ? "1" : "0");
  } catch {}
});

function openPlayer(list, index, opts) {
  queueMicrotask(updateBackBtn);
  if (!list.length) return;
  const preview = $("#feature-video");
  if (preview) preview.pause();
  state.player = { list: [...list], index: Math.max(0, Math.min(index, list.length - 1)) };
  player.classList.remove("hidden");
  showCurrentClip(opts);
}

// opts: { time: start position in seconds, play: start playing (default true) }
function showCurrentClip(opts = {}) {
  const { list, index } = state.player;
  const c = list[index];
  renderPlayerHeader();
  syncPlayerFav();
  cancelRename();
  $("#p-prev").disabled = index === 0;
  $("#p-next").disabled = index === list.length - 1;
  resetEditForClip();
  updateReviewBar();
  loadClipVideo(c, opts.time || 0, opts.play !== false);
  loadWave(c);
  renderKillMarks();
  ensureKillTimes();
}

// While the hand cam is being added the file is about to be replaced, and playing
// it fights the work for the disk: the player waits, then starts on its own.
let heldClip = null; // { path, time, play } waiting for the hand cam
function syncPlayerBusy() {
  const c = curClip();
  const waiting = !!c && busy.get(c.path) === "handcam";
  $("#p-busy").classList.toggle("hidden", !waiting);
  if (!waiting && heldClip && c && heldClip.path === c.path) {
    const h = heldClip;
    heldClip = null;
    loadClipVideo(c, h.time, h.play);
  }
}

// Loads a clip into the player, starting at `time`. Never plays while the overlay is hidden.
function loadClipVideo(c, time, play) {
  if (busy.get(c.path) === "handcam") {
    heldClip = { path: c.path, time, play };
    releaseVideo();
    syncPlayerBusy();
    updateProgress();
    return;
  }
  heldClip = null;
  $("#p-busy").classList.add("hidden");
  video.src = videoUrl(c.path);
  if (time > 0) {
    video.addEventListener("loadedmetadata", () => {
      video.currentTime = Math.min(time, Math.max(0, (video.duration || time) - 0.05));
    }, { once: true });
  }
  if (play && overlayVisible()) video.play().catch(() => {});
  updateProgress();
}

function syncPlayerKills() {
  const btn = $("#p-kills");
  const c = state.player && state.player.list[state.player.index];
  btn.classList.toggle("hidden", !c || !isValorant(c.game));
  if (!c) return;
  btn.disabled = !!c.counting;
  const known = c.kills != null;
  btn.querySelector("span").textContent = c.counting ? "Reading clip" : known ? killText(c.kills) : "Count kills";
  btn.title = known ? "Read the clip again (kills, map, agent)" : "Count kills and tag the map and agent (reads the saved file)";
}

// New info for a clip: update every copy of it and what's on screen.
function setInfo(path, info) {
  findClips(path).forEach((c) => {
    if (info.kills != null) c.kills = info.kills;
    if (info.map) c.map = info.map;
    if (info.agent) c.agent = info.agent;
    if (info.auto) c.auto = true;
    if (info.kill_times) c.kill_times = info.kill_times;
    c.counting = false;
  });
  if (state.player) { renderPlayerHeader(); renderKillMarks(); }
  if (state.view.name !== "settings") rerenderKeepScroll();
}

// ---------- kills on the timeline ----------

const KILL_LEAD = 2; // jump to a little before the kill
const curClip = () => state.player && state.player.list[state.player.index];
const killTimes = () => {
  const c = curClip();
  return (c && c.kill_times) || [];
};

function renderKillMarks() {
  const d = video.duration;
  const times = d > 0 ? killTimes().filter((t) => t <= d) : [];
  const total = times.length;
  $("#vc-kills").innerHTML = times.map((t, i) =>
    `<div class="kill-mark" data-t="${t}" style="left:${(t / d * 100).toFixed(3)}%" title="Kill ${i + 1}${total > 1 ? " of " + total : ""} · ${fmtClock(t)}"></div>`).join("");
  updateHint();
  $("#trim-kills").innerHTML = times.map((t) => `<div class="kill-mark" style="left:${(t / d * 100).toFixed(3)}%"></div>`).join("");
}

$("#vc-kills").addEventListener("pointerdown", (e) => {
  const m = e.target.closest(".kill-mark");
  if (!m) return;
  e.preventDefault();
  e.stopPropagation(); // not a scrub
  video.currentTime = Math.max(0, parseFloat(m.dataset.t) - KILL_LEAD);
  updateProgress();
  if (video.paused) video.play().catch(() => {});
});

// N / P: next or previous kill.
function jumpKill(dir) {
  const times = killTimes();
  if (!times.length) return;
  const now = video.currentTime;
  const targets = times.map((t) => Math.max(0, t - KILL_LEAD));
  const t = dir > 0 ? targets.find((x) => x > now + 0.3) : [...targets].reverse().find((x) => x < now - 0.6);
  if (t == null) return;
  video.currentTime = t;
  updateProgress();
}

// Clips counted before kill times existed (or saved with counting off) get them
// read when you open them — only the killfeed corner, at low priority.
const readingTimes = new Set();
let killTimesTimer = 0;
function ensureKillTimes() {
  clearTimeout(killTimesTimer);
  const c = curClip();
  if (!c || c.kill_times || !isValorant(c.game) || readingTimes.has(c.path)) return;
  const cap = (state.config && state.config.capture) || {};
  if (c.kills == null && !cap.count_kills) return; // never counted and counting is off
  killTimesTimer = setTimeout(() => {
    if (curClip() !== c) return;
    // Reading decodes the whole clip: wait while it plays, so playback stays smooth.
    if (!video.paused && !video.ended) {
      video.addEventListener("pause", () => { if (curClip() === c) ensureKillTimes(); }, { once: true });
      return;
    }
    readingTimes.add(c.path);
    invoke("read_kill_times", { path: c.path })
      .then((info) => setInfo(c.path, info))
      .catch(() => {})
      .finally(() => readingTimes.delete(c.path));
  }, 150);
}
["loadedmetadata", "durationchange"].forEach((ev) => video.addEventListener(ev, renderKillMarks));

$("#p-kills").onclick = async () => {
  const c = state.player && state.player.list[state.player.index];
  if (!c) return;
  findClips(c.path).forEach((x) => (x.counting = true));
  syncPlayerKills();
  try {
    setInfo(c.path, await invoke("analyze_clip", { path: c.path }));
  } catch (e) {
    findClips(c.path).forEach((x) => (x.counting = false));
    syncPlayerKills();
    toast("Couldn't read the clip: " + e, true);
  }
};

const tagText = (c) => [c.map, c.agent].filter(Boolean).join(" · ");

function renderPlayerHeader() {
  const c = state.player.list[state.player.index];
  syncPlayerKills();
  const d = new Date(c.t);
  $("#p-game").textContent = c.title || c.game;
  $("#p-game").title = c.title || "";
  const tags = [c.auto ? "Auto clipped" : "", tagText(c)].filter(Boolean).join(" · ");
  $("#p-sub").textContent = `${c.title ? c.game + " · " : ""}${tags ? tags + " · " : ""}${dayLabel(c.t)} at ${timeFmt.format(d)} · ${fmtSize(c.size)} · ${c.file_name}`;
}

// ---------- player controls + waveform ----------

const vc = {
  wrap: $("#video-wrap"),
  col: $("#video-col"),
  scrub: $("#vc-scrub"),
  wave: $("#vc-wave"),
  played: $("#vc-wave-played"),
  head: $("#vc-head"),
  tip: $("#vc-tip"),
  play: $("#vc-play"),
  cur: $("#vc-cur"),
  dur: $("#vc-dur"),
  speed: $("#vc-speed"),
  mute: $("#vc-mute"),
  vol: $("#vc-vol"),
  fs: $("#vc-fs"),
};

function fmtClock(s) {
  if (!Number.isFinite(s) || s < 0) s = 0;
  s = Math.floor(s);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

function updateProgress() {
  const d = video.duration;
  const t = video.currentTime;
  const f = d > 0 ? Math.min(1, Math.max(0, t / d)) : 0;
  vc.played.style.clipPath = `inset(0 ${((1 - f) * 100).toFixed(3)}% 0 0)`;
  vc.head.style.left = (f * 100).toFixed(3) + "%";
  vc.cur.textContent = fmtClock(t);
  vc.dur.textContent = fmtClock(d);
}

function syncPlayState() {
  const playing = !video.paused && !video.ended;
  vc.play.classList.toggle("playing", playing);
  vc.wrap.classList.toggle("paused", !playing);
}

// Smooth playhead while playing (timeupdate only fires ~4×/s).
let progressRaf = 0;
function tickProgress() {
  updateProgress();
  progressRaf = video.paused ? 0 : requestAnimationFrame(tickProgress);
}
video.addEventListener("play", () => {
  // Never make sound while the overlay is hidden or fading out.
  if (!overlayVisible()) { video.pause(); return; }
  syncPlayState();
  if (!progressRaf) progressRaf = requestAnimationFrame(tickProgress);
});
["pause", "ended", "emptied"].forEach((ev) => video.addEventListener(ev, () => { syncPlayState(); updateProgress(); }));
["seeked", "timeupdate", "loadedmetadata", "durationchange"].forEach((ev) => video.addEventListener(ev, updateProgress));

function togglePlay() {
  if (video.paused || video.ended) video.play().catch(() => {});
  else video.pause();
}

vc.play.onclick = togglePlay;
$("#vc-big").onclick = togglePlay;
video.addEventListener("click", togglePlay);
// The two clicks of a double-click already toggled play twice (no net change).
video.addEventListener("dblclick", toggleFullscreen);

function toggleFullscreen() {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else vc.col.requestFullscreen().catch(() => {});
}
vc.fs.onclick = toggleFullscreen;
let fullscreenExitAt = 0;
document.addEventListener("fullscreenchange", () => {
  if (!document.fullscreenElement) fullscreenExitAt = performance.now();
  requestAnimationFrame(drawAllWaves);
});

// --- seeking on the waveform ---
const scrubTime = (clientX) => {
  const r = vc.scrub.getBoundingClientRect();
  return Math.min(1, Math.max(0, (clientX - r.left) / r.width)) * (video.duration || 0);
};
onDrag(vc.scrub, (e) => {
  if (!(video.duration > 0)) return;
  video.currentTime = scrubTime(e.clientX);
  updateProgress();
  showTip(e.clientX);
});
function showTip(clientX) {
  if (!(video.duration > 0)) return;
  const r = vc.scrub.getBoundingClientRect();
  const x = Math.min(r.width, Math.max(0, clientX - r.left));
  vc.tip.textContent = fmtClock(scrubTime(clientX));
  vc.tip.style.left = x + "px";
  vc.tip.classList.remove("hidden");
}
vc.scrub.addEventListener("pointermove", (e) => showTip(e.clientX));
vc.scrub.addEventListener("pointerleave", () => vc.tip.classList.add("hidden"));

// --- volume + speed ---
function syncVolume() {
  const v = video.muted ? 0 : video.volume;
  vc.vol.value = String(v);
  vc.vol.style.setProperty("--v", v * 100 + "%");
  vc.mute.classList.toggle("muted", v === 0);
}
vc.vol.addEventListener("input", () => {
  const v = parseFloat(vc.vol.value);
  video.volume = v;
  video.muted = v === 0;
});
function toggleMute() {
  if (video.muted || video.volume === 0) {
    video.muted = false;
    if (video.volume === 0) video.volume = 0.6;
  } else {
    video.muted = true;
  }
}
vc.mute.onclick = toggleMute;
video.addEventListener("volumechange", syncVolume);
syncVolume();

// Speed: 0.1× to 2× in 0.1 steps. Changes ramp like a velocity edit (ease in, ease out)
// instead of jumping, and slow motion runs the sound through a leveller so it stays even.
const SPEED_MIN = 0.1, SPEED_MAX = 2;
const speedPop = $("#vc-speed-pop");
const speedRange = $("#vc-speed-range");
const roundRate = (r) => Math.round(Math.min(SPEED_MAX, Math.max(SPEED_MIN, r)) * 10) / 10;
const fmtRate = (r) => (r === Math.round(r) ? String(r) : r.toFixed(1)) + "×";
let speedTarget = 1;
let rampRaf = 0;

// Loudness levelling for slow motion: compressor + makeup gain + a limiter, blended in as the
// clip slows down (dry at 1× and above, fully levelled from 0.7× down). Built on first use,
// from a click, so the audio context is allowed to run.
let leveller = null;
function ensureLeveller() {
  if (leveller !== null) return leveller;
  leveller = false;
  try {
    const ctx = new AudioContext();
    const src = ctx.createMediaElementSource(video);
    const dry = ctx.createGain();
    const wet = ctx.createGain();
    const comp = ctx.createDynamicsCompressor();
    comp.threshold.value = -30; comp.knee.value = 12; comp.ratio.value = 4;
    comp.attack.value = 0.005; comp.release.value = 0.3;
    const makeup = ctx.createGain();
    makeup.gain.value = 2.2; // about +7 dB, brings quiet parts up to normal level
    const limit = ctx.createDynamicsCompressor();
    limit.threshold.value = -2; limit.knee.value = 0; limit.ratio.value = 20;
    limit.attack.value = 0.001; limit.release.value = 0.1;
    src.connect(dry).connect(ctx.destination);
    src.connect(comp).connect(makeup).connect(limit).connect(wet).connect(ctx.destination);
    wet.gain.value = 0;
    leveller = { ctx, dry, wet };
  } catch (e) {
    console.warn("audio leveller unavailable", e);
  }
  return leveller;
}
function setLevelling(rate) {
  if (!leveller) {
    if (rate >= 1) return;
    ensureLeveller();
    if (!leveller) return;
  }
  const { ctx, dry, wet } = leveller;
  if (ctx.state === "suspended") ctx.resume().catch(() => {});
  const amount = Math.min(1, Math.max(0, (1 - rate) / 0.3));
  // Equal-power blend, smoothed so it never clicks.
  wet.gain.setTargetAtTime(Math.sin((amount * Math.PI) / 2), ctx.currentTime, 0.05);
  dry.gain.setTargetAtTime(Math.cos((amount * Math.PI) / 2), ctx.currentTime, 0.05);
}
video.addEventListener("play", () => { if (leveller && leveller.ctx.state === "suspended") leveller.ctx.resume().catch(() => {}); });
video.preservesPitch = true;

// Velocity-style curve: slow start, fast middle, soft landing.
const easeInOut = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
function applyRate(r) {
  video.playbackRate = r;
  setLevelling(r);
}
function setSpeed(rate, { instant = false } = {}) {
  rate = roundRate(rate);
  speedTarget = rate;
  video.defaultPlaybackRate = rate;
  vc.speed.textContent = fmtRate(rate);
  vc.speed.classList.toggle("on", rate !== 1);
  $("#vc-speed-val").textContent = rate.toFixed(1) + "×";
  speedRange.value = String(rate);
  speedRange.style.setProperty("--v", ((rate - SPEED_MIN) / (SPEED_MAX - SPEED_MIN)) * 100 + "%");
  document.querySelectorAll("#vc-speed-chips button").forEach((b) => b.classList.toggle("on", Number(b.dataset.rate) === rate));
  cancelAnimationFrame(rampRaf);
  rampRaf = 0;
  const from = video.playbackRate;
  const still = video.paused || video.ended || document.body.classList.contains("reduce-motion");
  if (instant || still || Math.abs(from - rate) < 0.001) { applyRate(rate); return; }
  // Bigger changes take a little longer, like a hand-made ramp: 0.35 s to 0.9 s.
  const dur = 350 + 550 * Math.min(1, Math.abs(Math.log(rate / from)) / Math.log(10));
  const t0 = performance.now();
  // Ramp in log space so 1× → 0.1× feels as even as 0.1× → 1×.
  const a = Math.log(from), b = Math.log(rate);
  const tick = (now) => {
    const t = Math.min(1, (now - t0) / dur);
    applyRate(Math.exp(a + (b - a) * easeInOut(t)));
    rampRaf = t < 1 && !video.paused ? requestAnimationFrame(tick) : 0;
    if (!rampRaf && t < 1) applyRate(rate); // paused mid-ramp: land on the target
  };
  rampRaf = requestAnimationFrame(tick);
}

function openSpeedPop(open) {
  speedPop.classList.toggle("hidden", !open);
  vc.speed.setAttribute("aria-expanded", String(open));
}
const speedPopOpen = () => !speedPop.classList.contains("hidden");
vc.speed.onclick = (e) => { e.stopPropagation(); openSpeedPop(!speedPopOpen()); };
speedPop.addEventListener("click", (e) => e.stopPropagation());
document.addEventListener("pointerdown", (e) => {
  if (speedPopOpen() && !speedPop.contains(e.target) && e.target !== vc.speed) openSpeedPop(false);
});
speedRange.addEventListener("input", () => setSpeed(parseFloat(speedRange.value)));
document.querySelectorAll("#vc-speed-chips button").forEach((b) => { b.onclick = () => setSpeed(Number(b.dataset.rate)); });
vc.speed.addEventListener("wheel", (e) => { e.preventDefault(); setSpeed(speedTarget + (e.deltaY < 0 ? 0.1 : -0.1)); }, { passive: false });
speedPop.addEventListener("wheel", (e) => { e.preventDefault(); setSpeed(speedTarget + (e.deltaY < 0 ? 0.1 : -0.1)); }, { passive: false });
setSpeed(1, { instant: true });
// A new src resets the rate; keep the chosen speed across clips.
video.addEventListener("loadstart", () => {
  cancelAnimationFrame(rampRaf);
  rampRaf = 0;
  if (video.playbackRate !== speedTarget) applyRate(speedTarget);
});

// --- waveform ---
const waves = new Map(); // clip path -> peaks (0–255), small cache
let wavePeaks = null;
let wavePath = null;
let waveTimer = null;

function loadWave(c) {
  wavePath = c.path;
  wavePeaks = waves.get(c.path) || null;
  drawAllWaves();
  clearTimeout(waveTimer);
  if (wavePeaks) return;
  // Give the video a head start on the hard drive before reading the audio.
  waveTimer = setTimeout(() => {
    const path = c.path;
    invoke("get_waveform", { path })
      .then((peaks) => {
        waves.set(path, peaks);
        if (waves.size > 60) waves.delete(waves.keys().next().value);
        if (wavePath === path) { wavePeaks = peaks; drawAllWaves(); }
      })
      .catch(() => {});
  }, 350);
}

// Mirrored bars, like a voice-memo waveform. No peaks yet = a flat dotted line.
function drawWave(canvas, peaks, fill) {
  const dpr = window.devicePixelRatio || 1;
  const w = Math.round(canvas.clientWidth * dpr);
  const h = Math.round(canvas.clientHeight * dpr);
  if (!w || !h) return;
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const ctx = canvas.getContext("2d");
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = typeof fill === "function" ? fill(ctx, w) : fill;
  const bar = Math.max(2, Math.round(2 * dpr));
  const gap = Math.max(1, Math.round(1.5 * dpr));
  const pitch = bar + gap;
  const n = Math.max(1, Math.floor((w + gap) / pitch));
  const minH = Math.max(2, Math.round(2 * dpr));
  const has = peaks && peaks.length;
  for (let i = 0; i < n; i++) {
    let v = 0;
    if (has) {
      const a = Math.floor((i * peaks.length) / n);
      const b = Math.max(a + 1, Math.floor(((i + 1) * peaks.length) / n));
      for (let j = a; j < b && j < peaks.length; j++) if (peaks[j] > v) v = peaks[j];
    }
    const bh = Math.max(minH, (v / 255) * h);
    ctx.fillRect(i * pitch, Math.round((h - bh) / 2), bar, Math.round(bh));
  }
}

const cssVar = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
const playedFill = () => cssVar("--accent"); // the part you've watched
const unplayedFill = () => `rgba(${cssVar("--ink")}, .3)`;

function drawAllWaves() {
  if (!state.player) return;
  drawWave(vc.wave, wavePeaks, unplayedFill());
  drawWave(vc.played, wavePeaks, playedFill());
  if (ed.open) drawWave($("#trim-wave"), wavePeaks, unplayedFill());
}
new ResizeObserver(() => drawAllWaves()).observe(vc.scrub);
new ResizeObserver(() => drawAllWaves()).observe($("#trim-track"));

// ---------- rename ----------

function startRename() {
  if (!state.player || ed.job) return;
  const c = state.player.list[state.player.index];
  const input = $("#p-rename-input");
  input.value = c.file_name.replace(/\.[^.]+$/, "");
  $("#p-name-row").classList.add("hidden");
  $("#p-sub").classList.add("hidden");
  $("#p-rename-form").classList.remove("hidden");
  input.focus();
  input.select();
}

function cancelRename() {
  $("#p-rename-form").classList.add("hidden");
  $("#p-name-row").classList.remove("hidden");
  $("#p-sub").classList.remove("hidden");
}

let renaming = false;
async function saveRename() {
  if (!state.player || renaming) return;
  const c = state.player.list[state.player.index];
  const name = $("#p-rename-input").value.trim();
  if (!name || name === c.file_name.replace(/\.[^.]+$/, "")) return cancelRename();
  renaming = true;
  const resumeAt = video.currentTime;
  const wasPlaying = !video.paused;
  releaseVideo(); // Windows won't rename a file that's open
  try {
    const updated = await invoke("rename_clip", { path: c.path, name });
    const oldPath = c.path;
    const oldThumb = state.thumbs.get(oldPath);
    for (const x of findClips(oldPath)) {
      Object.assign(x, updated, { t: x.t }); // keep the original recording time
    }
    if (oldThumb) state.thumbs.set(updated.path, oldThumb);
    cancelRename();
    renderPlayerHeader();
    toast("Renamed");
    rerenderKeepScroll();
  } catch (e) {
    toast(String(e), true);
    $("#p-rename-input").focus();
  } finally {
    renaming = false;
    const cur = state.player && state.player.list[state.player.index];
    if (cur) {
      loadClipVideo(cur, resumeAt, wasPlaying);
      loadWave(cur);
    }
  }
}

$("#p-rename").onclick = startRename;
$("#p-rename-cancel").onclick = cancelRename;
$("#p-rename-form").onsubmit = (e) => { e.preventDefault(); saveRename(); };
$("#p-rename-input").addEventListener("keydown", (e) => {
  if (e.key === "Escape") { e.preventDefault(); cancelRename(); }
});
$("#p-fav").onclick = () => { if (state.player) toggleFavorite(state.player.list[state.player.index]); };

function releaseVideo() {
  video.pause();
  video.removeAttribute("src");
  video.load(); // lets Windows release the file
}

function closePlayer() {
  queueMicrotask(updateBackBtn);
  const review = state.player && state.player.review;
  if (review) {
    const { kept, deleted, skipped, freed } = review;
    state.reviewSummary = { kept, deleted, skipped, freed };
  }
  setEditor(false);
  releaseVideo();
  player.classList.add("hidden");
  state.player = null;
  updateReviewBar();
  if (review && state.view.name === "review") render();
}

function step(delta) {
  if (!state.player) return;
  const next = state.player.index + delta;
  if (next < 0 || next >= state.player.list.length) return;
  state.player.index = next;
  showCurrentClip();
}

async function deleteCurrent() {
  if (!state.player) return;
  const c = state.player.list[state.player.index];
  video.pause();
  const ok = await confirmDialog("Delete this clip?", `“${c.file_name}” will be moved to the Recycle Bin.`);
  if (!ok) return;
  releaseVideo();
  try {
    await invoke("delete_clip", { path: c.path });
  } catch (e) {
    toast("Couldn't delete: " + e, true);
    showCurrentClip();
    return;
  }
  toast("Moved to Recycle Bin");
  // Update the library locally right away (the folder watcher will also refresh).
  state.lib.clips = state.lib.clips.filter((x) => x.path !== c.path);
  recountGames();
  const p = state.player;
  p.list.splice(p.index, 1);
  if (!p.list.length) { closePlayer(); render(); renderSidebar(); return; }
  if (p.index >= p.list.length) p.index = p.list.length - 1;
  showCurrentClip();
  renderSidebar();
  rerenderKeepScroll();
}

function recountGames() {
  const by = new Map();
  for (const c of state.lib.clips) {
    const g = by.get(c.game) || { name: c.game, clip_count: 0, total_size: 0, latest_ms: 0 };
    g.clip_count++; g.total_size += c.size; g.latest_ms = Math.max(g.latest_ms, c.modified_ms);
    by.set(c.game, g);
  }
  // Empty game folders stay (every folder is a game).
  state.lib.games = state.lib.games.map((g) => by.get(g.name) || { ...g, clip_count: 0, total_size: 0, latest_ms: 0 });
}

$("#p-close").onclick = closePlayer;
$("#p-prev").onclick = () => step(-1);
$("#p-next").onclick = () => step(1);
const inReview = () => !!(state.player && state.player.review);
$("#p-delete").onclick = () => (inReview() ? reviewDelete() : deleteCurrent());
$("#p-reveal").onclick = () => {
  if (!state.player) return;
  invoke("reveal_clip", { path: state.player.list[state.player.index].path }).catch((e) => toast(String(e), true));
};
$("#p-copy").onclick = () => {
  if (!state.player) return;
  invoke("copy_file", { path: state.player.list[state.player.index].path })
    .then(() => toast("Copied — paste it anywhere with Ctrl+V"))
    .catch((e) => toast(String(e), true));
};
$("#p-edit").onclick = () => setEditor(!ed.open);

window.addEventListener("keydown", (e) => {
  if (keyCaptureActive) return; // the hotkey recorder has the keyboard
  if (!$("#wizard").classList.contains("hidden")) return;
  if (!$("#confirm").classList.contains("hidden")) {
    if (e.key === "Escape") { e.preventDefault(); resolveConfirm(false); }
    if (e.key === "Enter") { e.preventDefault(); resolveConfirm(true); }
    return;
  }
  if (e.key === "Escape") {
    // Esc first leaves fullscreen (the browser does that itself).
    if (document.fullscreenElement || performance.now() - fullscreenExitAt < 400) return;
    if (!state.player) {
      e.preventDefault();
      requestClose();
      return;
    }
  }
  const tag = e.target && e.target.tagName;
  const typing = tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA";
  // A single-key quick-view hotkey: Clipit has the keyboard now, so close it here.
  const qk = state.config && state.config.hotkey;
  if (!typing && !e.ctrlKey && !e.altKey && !e.metaKey && isSingleKey(qk) && keyCodeName(e.code).toLowerCase() === qk.toLowerCase() && !isAppMode()) {
    e.preventDefault();
    requestClose();
    return;
  }
  if (!state.player) return;
  // The rename box handles its own Enter/Escape.
  if (typing && (e.key !== "Escape" || e.target.id === "p-rename-input")) return;
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  const handled = {
    Escape: () => (speedPopOpen() ? openSpeedPop(false) : ed.open ? setEditor(false) : closePlayer()),
    "[": () => setSpeed(speedTarget - 0.1),
    "]": () => setSpeed(speedTarget + 0.1),
    ArrowLeft: () => { video.currentTime = Math.max(0, video.currentTime - 5); },
    ArrowRight: () => { video.currentTime = Math.min(video.duration || 0, video.currentTime + 5); },
    ArrowUp: () => step(-1),
    ArrowDown: () => step(1),
    " ": togglePlay,
    m: toggleMute,
    ",": () => { video.pause(); video.currentTime = Math.max(0, video.currentTime - 1 / 30); },
    ".": () => { video.pause(); video.currentTime = Math.min(video.duration || 0, video.currentTime + 1 / 30); },
    Delete: () => (inReview() ? reviewDelete() : deleteCurrent()),
    k: () => { if (inReview()) reviewKeep(); },
    d: () => { if (inReview()) reviewDelete(); },
    s: () => { if (inReview()) reviewSkip(); },
    z: () => { if (inReview()) reviewUndo(); },
    e: () => setEditor(!ed.open),
    i: () => { if (ed.open) setTrimPoint("start", video.currentTime); },
    o: () => { if (ed.open) setTrimPoint("end", video.currentTime); },
    f: () => toggleFavorite(state.player.list[state.player.index]),
    n: () => jumpKill(1),
    p: () => jumpKill(-1),
    F2: () => startRename(),
  }[key];
  if (handled && (e.ctrlKey || e.altKey || e.metaKey)) return;
  if (handled) {
    e.preventDefault();
    e.stopPropagation();
    handled();
  }
}, true);

// ---------- editor: trim / crop / compress ----------

const ed = {
  open: false,
  start: 0,
  end: 0,
  aspect: "",
  crop: null,      // { x, y, w, h } normalized to the source frame
  job: null,       // running export job id
  lastOutput: null,
};

const editorEl = $("#editor");
const trimEl = $("#trim");
const track = $("#trim-track");
const wrap = $("#video-wrap");
const cropLayer = $("#crop-layer");
const cropBox = $("#crop-box");

const even = (v) => { v = Math.max(2, Math.round(v)); return v - (v % 2); };

function fmtTime(s) {
  if (!Number.isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
}

function updateHint() {
  $("#player-hint").innerHTML = ed.open
    ? `<kbd>I</kbd> / <kbd>O</kbd> set start / end · drag the handles to trim · <kbd>Space</kbd> play/pause · <kbd>Esc</kbd> close editor`
    : inReview()
      ? `<kbd>K</kbd> keep · <kbd>D</kbd> delete · <kbd>S</kbd> skip · <kbd>Z</kbd> undo · <kbd>F</kbd> favorite · <kbd>E</kbd> edit · <kbd>Space</kbd> play/pause · <kbd>Esc</kbd> stop reviewing`
      : `<kbd>Space</kbd> play/pause · <kbd>←</kbd><kbd>→</kbd> seek 5s · <kbd>,</kbd><kbd>.</kbd> frame · <kbd>↑</kbd><kbd>↓</kbd> previous/next · ${killTimes().length ? "<kbd>N</kbd><kbd>P</kbd> next/previous kill · " : ""}<kbd>M</kbd> mute · <kbd>F</kbd> favorite · <kbd>F2</kbd> rename · <kbd>E</kbd> edit · <kbd>Del</kbd> delete · <kbd>Esc</kbd> close`;
}
updateHint();

function setEditor(open) {
  ed.open = open;
  editorEl.classList.toggle("hidden", !open);
  trimEl.classList.toggle("hidden", !open);
  updateHint();
  if (open) {
    ensureTrimRange();
    renderTrim();
    renderEdTimes();
    updateEstimate();
    requestAnimationFrame(tickPlayhead);
    requestAnimationFrame(drawAllWaves);
  }
  layoutCrop();
}

function resetEditForClip() {
  ed.start = 0;
  ed.end = 0;
  ed.crop = null;
  setAspectChip("");
  ed.aspect = "";
  $("#ed-result").classList.add("hidden");
  renderTrim();
  renderEdTimes();
  layoutCrop();
}

function ensureTrimRange() {
  const d = video.duration;
  if (!Number.isFinite(d) || d <= 0) return;
  if (!(ed.end > 0) || ed.end > d) ed.end = d;
  if (ed.start >= ed.end) ed.start = 0;
}

video.addEventListener("loadedmetadata", () => {
  ensureTrimRange();
  renderTrim();
  renderEdTimes();
  if (ed.aspect && !ed.crop) setAspect(ed.aspect);
  layoutCrop();
  updateEstimate();
});

// --- trim ---

const minLen = () => Math.min(0.5, (video.duration || 1) / 2);
const pct = (t) => (video.duration > 0 ? (t / video.duration) * 100 : 0);

function renderTrim() {
  const s = pct(ed.start);
  const e = ed.end > 0 ? pct(ed.end) : 100;
  $("#trim-h-start").style.left = s + "%";
  $("#trim-h-end").style.left = e + "%";
  $("#trim-range").style.left = s + "%";
  $("#trim-range").style.width = Math.max(0, e - s) + "%";
  $("#trim-dim-l").style.width = s + "%";
  $("#trim-dim-r").style.width = Math.max(0, 100 - e) + "%";
}

function renderEdTimes() {
  $("#ed-start").textContent = fmtTime(ed.start);
  $("#ed-end").textContent = fmtTime(ed.end);
  const len = ed.end - ed.start;
  const full = video.duration > 0 && ed.start <= 0.05 && Math.abs(ed.end - video.duration) < 0.05;
  $("#ed-length").textContent = len > 0 ? `${len.toFixed(1)}s selected${full ? " (whole clip)" : ""}` : "";
}

function setTrimPoint(which, t) {
  const d = video.duration;
  if (!(d > 0)) return;
  if (which === "start") ed.start = Math.max(0, Math.min(t, ed.end - minLen()));
  else ed.end = Math.min(d, Math.max(t, ed.start + minLen()));
  renderTrim();
  renderEdTimes();
  updateEstimate();
}

function timeAt(clientX) {
  const r = track.getBoundingClientRect();
  const f = Math.min(1, Math.max(0, (clientX - r.left) / r.width));
  return f * (video.duration || 0);
}

function onDrag(el, onMove) {
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    el.setPointerCapture(e.pointerId);
    onMove(e, true);
    const move = (ev) => onMove(ev, false);
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  });
}

onDrag($("#trim-h-start"), (e) => {
  video.pause();
  setTrimPoint("start", timeAt(e.clientX));
  video.currentTime = ed.start;
});
onDrag($("#trim-h-end"), (e) => {
  video.pause();
  setTrimPoint("end", timeAt(e.clientX));
  video.currentTime = ed.end;
});
onDrag(track, (e) => { video.currentTime = timeAt(e.clientX); });

function tickPlayhead() {
  if (!ed.open || !state.player) return;
  $("#trim-playhead").style.left = pct(video.currentTime) + "%";
  // Loop the selected part while editing.
  if (!video.paused && ed.end > 0 && video.currentTime >= ed.end - 0.03) {
    video.currentTime = ed.start;
  }
  requestAnimationFrame(tickPlayhead);
}

$("#ed-set-start").onclick = () => setTrimPoint("start", video.currentTime);
$("#ed-set-end").onclick = () => setTrimPoint("end", video.currentTime);
$("#ed-trim-reset").onclick = () => {
  ed.start = 0;
  ed.end = video.duration || 0;
  renderTrim();
  renderEdTimes();
  updateEstimate();
};

// --- crop ---

function setAspectChip(a) {
  document.querySelectorAll("#ed-aspect .chip").forEach((c) => c.classList.toggle("active", c.dataset.aspect === a));
}

function setAspect(a) {
  ed.aspect = a;
  setAspectChip(a);
  const vw = video.videoWidth;
  const vh = video.videoHeight;
  if (!a || !vw || !vh) {
    ed.crop = null;
  } else {
    const [aw, ah] = a.split(":").map(Number);
    const A = aw / ah;
    let w, h;
    if (A < vw / vh) { h = 1; w = (A * vh) / vw; } else { w = 1; h = vw / (A * vh); }
    ed.crop = { x: (1 - w) / 2, y: (1 - h) / 2, w, h };
  }
  layoutCrop();
  updateEstimate();
}

document.querySelectorAll("#ed-aspect .chip").forEach((c) => (c.onclick = () => setAspect(c.dataset.aspect)));

function layoutCrop() {
  const show = ed.open && !!ed.crop && video.videoWidth > 0;
  cropLayer.classList.toggle("hidden", !show);
  if (!show) return;
  const W = wrap.clientWidth;
  const H = wrap.clientHeight;
  const va = video.videoWidth / video.videoHeight;
  let w, h;
  if (W / H > va) { h = H; w = H * va; } else { w = W; h = W / va; }
  Object.assign(cropLayer.style, { left: (W - w) / 2 + "px", top: (H - h) / 2 + "px", width: w + "px", height: h + "px" });
  Object.assign(cropBox.style, {
    left: ed.crop.x * 100 + "%", top: ed.crop.y * 100 + "%",
    width: ed.crop.w * 100 + "%", height: ed.crop.h * 100 + "%",
  });
}
new ResizeObserver(layoutCrop).observe(wrap);

let cropDrag = null;
onDrag(cropBox, (e, first) => {
  if (!ed.crop) return;
  if (first) {
    cropDrag = { x: e.clientX, y: e.clientY, cx: ed.crop.x, cy: ed.crop.y, w: cropLayer.clientWidth, h: cropLayer.clientHeight };
    return;
  }
  const d = cropDrag;
  ed.crop.x = Math.min(1 - ed.crop.w, Math.max(0, d.cx + (e.clientX - d.x) / d.w));
  ed.crop.y = Math.min(1 - ed.crop.h, Math.max(0, d.cy + (e.clientY - d.y) / d.h));
  layoutCrop();
});

// --- compress options ---

const PREFS_KEY = "clipit.export";
try {
  const p = JSON.parse(localStorage.getItem(PREFS_KEY) || "{}");
  if (p.size !== undefined) $("#ed-size").value = p.size;
  if (p.custom) $("#ed-custom").value = p.custom;
  if (p.res !== undefined) $("#ed-res").value = p.res;
  if (p.fps !== undefined) $("#ed-fps").value = p.fps;
  if (p.audio !== undefined) $("#ed-audio").checked = !!p.audio;
} catch {}
if ($("#ed-size").selectedIndex < 0) $("#ed-size").value = "10";

function saveEdPrefs() {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify({
      size: $("#ed-size").value, custom: $("#ed-custom").value,
      res: $("#ed-res").value, fps: $("#ed-fps").value, audio: $("#ed-audio").checked,
    }));
  } catch {}
}

["#ed-size", "#ed-custom", "#ed-res", "#ed-fps", "#ed-audio"].forEach((sel) => {
  $(sel).addEventListener("input", () => { saveEdPrefs(); updateEstimate(); });
  $(sel).addEventListener("change", () => { saveEdPrefs(); updateEstimate(); });
});

function autoShort(kbps) {
  if (kbps >= 5000) return 1080;
  if (kbps >= 1200) return 720;
  if (kbps >= 600) return 480;
  return 360;
}

// Mirrors the backend's plan() so the panel can show what you'll get.
function estimate() {
  const sizeSel = $("#ed-size").value;
  $("#ed-custom-wrap").classList.toggle("hidden", sizeSel !== "custom");
  let target = null;
  if (sizeSel === "custom") target = parseFloat($("#ed-custom").value);
  else if (sizeSel) target = parseFloat(sizeSel);
  const res = $("#ed-res").value ? Number($("#ed-res").value) : null;
  const fps = $("#ed-fps").value ? Number($("#ed-fps").value) : null;
  const audio = $("#ed-audio").checked;

  const dur = ed.end - ed.start;
  const vw = video.videoWidth;
  const vh = video.videoHeight;
  if (!vw || !(dur > 0)) return null;
  let cw = vw, ch = vh;
  if (ed.crop) { cw = even(ed.crop.w * vw); ch = even(ed.crop.h * vh); }
  let short = Math.min(cw, ch);
  let vk = null;
  let ak = 0;
  if (target !== null) {
    if (!(target >= 1)) return { invalid: "Enter a size of at least 1 MB" };
    const total = (target * 8000 * 0.92) / dur;
    ak = !audio ? 0 : total < 1000 ? 64 : 128;
    vk = Math.max(100, total - ak);
    // Same cap as the backend: never more bits than the source clip had.
    const clip = state.player && state.player.list[state.player.index];
    if (clip && video.duration > 0.5) vk = Math.min(vk, Math.max(1000, (clip.size * 8) / 1000 / video.duration));
    short = Math.min(short, res || autoShort(vk));
  } else if (res) {
    short = Math.min(short, res);
  }
  let ow = even(cw), oh = even(ch);
  if (short < Math.min(cw, ch)) {
    if (cw <= ch) { ow = even(short); oh = even((ch * short) / cw); }
    else { oh = even(short); ow = even((cw * short) / ch); }
  }
  const approxMb = vk !== null ? ((vk + ak) * dur) / 8000 : null;
  return { dur, ow, oh, vk, target, res, fps, audio, approxMb };
}

function updateEstimate() {
  if (!ed.open) return;
  const est = estimate();
  const box = $("#ed-estimate");
  const busy = !!ed.job;
  $("#ed-send").disabled = $("#ed-save").disabled = busy || !est || !!(est && est.invalid);
  if (!est) { box.textContent = "Loading clip…"; return; }
  if (est.invalid) { box.innerHTML = `<span class="warn">${esc(est.invalid)}</span>`; return; }
  let html = `<b>${est.ow}×${est.oh}</b> · ${est.fps ? est.fps + " fps" : "original fps"} · ${est.dur.toFixed(1)}s`;
  if (est.target !== null) {
    html += `<br>About <b>${est.approxMb < 1 ? est.approxMb.toFixed(2) : est.approxMb.toFixed(1)} MB</b> (limit ${est.target} MB) · ${(est.vk / 1000).toFixed(1)} Mbps video`;
    if (est.vk < 500) html += `<br><span class="warn">Low quality at this length — trim it shorter for a sharper clip.</span>`;
  } else {
    html += `<br>Best quality, no size limit`;
  }
  box.innerHTML = html;
}

// --- export ---

async function runExport(copy) {
  if (ed.job || !state.player) return;
  const est = estimate();
  if (!est || est.invalid) return;
  const clip = state.player.list[state.player.index];
  const job = `job-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
  ed.job = job;
  video.pause();
  $("#ed-result").classList.add("hidden");
  $("#ed-actions").classList.add("hidden");
  $("#ed-progress").classList.remove("hidden");
  $("#ed-bar").style.transform = "scaleX(0)";
  $("#ed-progress-text").textContent = "Starting…";
  updateEstimate();
  try {
    const res = await invoke("export_clip", {
      job,
      copy,
      request: {
        path: clip.path,
        start: ed.start,
        end: ed.end,
        crop: ed.crop,
        src_width: video.videoWidth,
        src_height: video.videoHeight,
        target_mb: est.target,
        resolution: est.res,
        fps: est.fps,
        audio: est.audio,
        src_duration: Number.isFinite(video.duration) ? video.duration : null,
      },
    });
    ed.lastOutput = res.output;
    ed.lastSource = clip;
    // Pick up the new file right away (the folder watcher would too, a bit later).
    await refreshLibrary();
    const editClip = state.lib.clips.find((c) => c.path === res.output) || null;
    const where = editClip ? "in your library, next to the original" : (state.status && state.status.export_dir) || "your exports folder";
    const info = `${fmtSize(res.size)} · ${res.width}×${res.height} · ${res.encoder}`;
    if (copy && res.copied) {
      $("#ed-result-title").textContent = "Saved and copied to the clipboard";
      $("#ed-result-sub").textContent = `${info}. Saved ${where}. Paste it into Discord with Ctrl+V.`;
      toast(`Copied (${fmtSize(res.size)}) — paste it into Discord with Ctrl+V`);
    } else {
      $("#ed-result-title").textContent = "Saved";
      $("#ed-result-sub").textContent = `${info}. Saved ${where}.` + (res.copy_error ? ` ${res.copy_error}` : "");
      toast(`Saved ${fmtSize(res.size)}`);
    }
    // Favorite / open only make sense when the edit is in the library.
    $("#ed-result-open").classList.toggle("hidden", !editClip);
    $("#ed-result-fav").classList.toggle("hidden", !editClip);
    $("#ed-result-del-orig").classList.remove("hidden");
    syncResultFav();
    $("#ed-result").classList.remove("hidden");
  } catch (e) {
    if (String(e) === "Cancelled") toast("Export cancelled");
    else toast("Export failed: " + e, true);
  } finally {
    ed.job = null;
    $("#ed-progress").classList.add("hidden");
    $("#ed-actions").classList.remove("hidden");
    updateEstimate();
  }
}

$("#ed-send").onclick = () => runExport(true);
$("#ed-save").onclick = () => runExport(false);
$("#ed-cancel").onclick = () => { if (ed.job) invoke("cancel_export", { job: ed.job }); };
$("#ed-result-copy").onclick = () => {
  if (!ed.lastOutput) return;
  invoke("copy_file", { path: ed.lastOutput })
    .then(() => toast("Copied — paste it with Ctrl+V"))
    .catch((e) => toast(String(e), true));
};
$("#ed-result-reveal").onclick = () => {
  if (ed.lastOutput) invoke("reveal_clip", { path: ed.lastOutput }).catch((e) => toast(String(e), true));
};

const lastEditClip = () => state.lib && state.lib.clips.find((c) => c.path === ed.lastOutput);

function syncResultFav() {
  const c = lastEditClip();
  const on = !!(c && c.favorite);
  $("#ed-result-fav").classList.toggle("on", on);
  $("#ed-result-fav").textContent = on ? "Favorited" : "Favorite edit";
}

$("#ed-result-fav").onclick = async () => {
  const c = lastEditClip();
  if (!c) return;
  await toggleFavorite(c);
  syncResultFav();
};

// Shows `clip` in the player right after the current one and switches to it.
function openInPlayer(clip) {
  const p = state.player;
  if (!p) return openPlayer([clip], 0);
  const existing = p.list.findIndex((c) => c.path === clip.path);
  if (existing >= 0) p.index = existing;
  else { p.list.splice(p.index + 1, 0, clip); p.index += 1; }
  showCurrentClip();
}

$("#ed-result-open").onclick = () => {
  const c = lastEditClip();
  if (c) { setEditor(false); openInPlayer(c); }
};

$("#ed-result-del-orig").onclick = async () => {
  const orig = ed.lastSource;
  if (!orig) return;
  const ok = await confirmDialog("Delete the original?", `“${orig.file_name}” (${fmtSize(orig.size)}) goes to the Recycle Bin. Your edited version stays.`);
  if (!ok) return;
  const showingOriginal = state.player && state.player.list[state.player.index].path === orig.path;
  if (showingOriginal) releaseVideo();
  try {
    await invoke("delete_clip", { path: orig.path });
  } catch (e) {
    toast("Couldn't delete: " + e, true);
    if (showingOriginal) showCurrentClip();
    return;
  }
  const edit = lastEditClip();
  // Keep the star: if the original was a favorite, the edit becomes one.
  if (orig.favorite && edit && !edit.favorite) await toggleFavorite(edit);
  state.lib.clips = state.lib.clips.filter((c) => c.path !== orig.path);
  recountGames();
  const p = state.player;
  if (p) {
    const i = p.list.findIndex((c) => c.path === orig.path);
    if (i >= 0) {
      if (edit) p.list[i] = edit;
      else p.list.splice(i, 1);
      if (!p.list.length) closePlayer();
      else { p.index = Math.min(i, p.list.length - 1); if (showingOriginal) { setEditor(false); showCurrentClip(); } }
    }
  }
  ed.lastSource = null;
  $("#ed-result-del-orig").classList.add("hidden");
  toast(`Original deleted — freed ${fmtSize(orig.size)}`);
  renderSidebar();
  rerenderKeepScroll();
};

async function refreshLibrary() {
  await loadLibrary();
  renderSidebar();
  rerenderKeepScroll();
}

listen("export-progress", (e) => {
  const p = e.payload;
  if (p.job !== ed.job) return;
  $("#ed-bar").style.transform = `scaleX(${Math.min(1, Math.max(0, p.percent / 100)).toFixed(4)})`;
  $("#ed-progress-text").textContent = p.pass > 1
    ? `Squeezing it under the limit… ${Math.round(p.percent)}%`
    : `Compressing… ${Math.round(p.percent)}%`;
});

// ---------- confirm dialog ----------

let confirmResolve = null;
function confirmDialog(title, text, opts = {}) {
  $("#confirm-title").textContent = title;
  $("#confirm-text").textContent = text;
  $("#confirm-yes").textContent = opts.yes || "Delete";
  $("#confirm-no").textContent = opts.no || "Cancel";
  $("#confirm-yes").className = "btn " + (opts.danger === false ? "primary" : "danger");
  $("#confirm").classList.remove("hidden");
  $("#confirm-yes").focus();
  return new Promise((res) => (confirmResolve = res));
}
function resolveConfirm(value) {
  $("#confirm").classList.add("hidden");
  if (confirmResolve) { confirmResolve(value); confirmResolve = null; }
}
$("#confirm-yes").onclick = () => resolveConfirm(true);
$("#confirm-no").onclick = () => resolveConfirm(false);

// ---------- settings ----------

// Saves a partial settings change, e.g. { capture: { fps: 60 } }.
async function patchConfig(patch) {
  try {
    state.status = await invoke("save_config", { patch });
    state.config = await invoke("get_config");
    renderHotkeyChip();
    return true;
  } catch (e) {
    toast("Couldn't save: " + e, true);
    return false;
  }
}

async function loadCaptureInfo() {
  try { state.capInfo = await invoke("capture_info"); } catch { state.capInfo = { devices: { outputs: [], inputs: [] }, monitors: [], encoders: [] }; }
  return state.capInfo;
}

// --- hotkey recorder ---

let keyCaptureActive = false;

function fmtKey(k) {
  if (!k) return "";
  return k.split("+").map((p) => p
    .replace(/^Digit(\d)$/, "$1").replace(/^Key([A-Z])$/, "$1").replace(/^Numpad(\w+)$/, "Num $1")
    .replace(/^Backquote$/, "`").replace(/^Control$/, "Ctrl").replace(/^Super$/, "Win")
    .replace(/^(Slash|BracketLeft|BracketRight|Backslash|IntlBackslash|Semicolon|Quote|Comma|Period|Minus|Equal)$/, (m) => KEY_SYMBOLS[m])).join(" + ");
}
const KEY_SYMBOLS = { Slash: "/", BracketLeft: "[", BracketRight: "]", Backslash: "\\", IntlBackslash: "\\", Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Minus: "-", Equal: "=" };

// The name a key is saved under: "L", "5", "Slash".
const keyCodeName = (code) => (/^Digit\d$/.test(code) ? code.slice(5) : /^Key[A-Z]$/.test(code) ? code.slice(3) : code);
// Keys that work alone without blocking typing elsewhere (see keys.rs).
const SINGLE_KEY = /^(Key[A-Z]|Digit\d|Backquote|Minus|Equal|BracketLeft|BracketRight|Backslash|IntlBackslash|Semicolon|Quote|Comma|Period|Slash)$/;
const isSingleKey = (k) => !!k && !k.includes("+") && SINGLE_KEY.test(/^\d$/.test(k) ? "Digit" + k : /^[A-Z]$/.test(k) ? "Key" + k : k === "`" ? "Backquote" : k);

// Turns a keydown into an accelerator the hotkey library understands ("Alt+0", "F9").
function acceleratorFor(e) {
  const code = e.code;
  if (/^(Control|Alt|Shift|Meta|OS)(Left|Right)?$/.test(code)) return null;
  const key = keyCodeName(code);
  const mods = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  // Single letters, digits and punctuation work too: they only fire while a game
  // is in front and still reach the game (see keys.rs).
  const standalone = SINGLE_KEY.test(code) || /^F\d{1,2}$/.test(code) || /^(Pause|ScrollLock|PrintScreen|Insert|Home|End|PageUp|PageDown|Numpad\w+)$/.test(code);
  if (!mods.length && !standalone) return { error: "This key needs Ctrl, Alt or Shift with it." };
  return { key: [...mods, key].join("+") };
}

const SINGLE_NOTE = "A single key (like L, / or ]) works only while a game is in front, and the game still gets the key too, so pick one your game doesn't use. Typing in other apps never triggers it.";

function keyButton(name, value) {
  return `<button type="button" class="keycap${value ? "" : " unset"}" data-key="${name}" data-value="${esc(value || "")}">${value ? esc(fmtKey(value)) : "Not set"}</button>`;
}

// Click a keycap, press the combination. Esc cancels, Backspace clears.
function wireKeyButtons(root, onSet) {
  root.querySelectorAll(".keycap").forEach((btn) => {
    btn.onclick = () => {
      if (keyCaptureActive) return;
      keyCaptureActive = true;
      invoke("set_hotkeys_paused", { paused: true }).catch(() => {});
      const before = btn.textContent;
      btn.classList.add("listening");
      btn.textContent = "Press keys…";
      const done = (value) => {
        window.removeEventListener("keydown", onKey, true);
        document.removeEventListener("pointerdown", onAway, true);
        btn.classList.remove("listening");
        keyCaptureActive = false;
        invoke("set_hotkeys_paused", { paused: false }).catch(() => {});
        if (value === undefined) { btn.textContent = before; return; }
        btn.dataset.value = value;
        btn.classList.toggle("unset", !value);
        btn.textContent = value ? fmtKey(value) : "Not set";
        onSet(btn.dataset.key, value);
      };
      const onKey = (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey) return done(undefined);
        if ((e.key === "Backspace" || e.key === "Delete") && !e.ctrlKey && !e.altKey && !e.shiftKey) return done("");
        const r = acceleratorFor(e);
        if (!r) return; // only a modifier so far
        if (r.error) { btn.textContent = r.error; return; }
        done(r.key);
      };
      const onAway = (e) => { if (e.target !== btn) done(undefined); };
      window.addEventListener("keydown", onKey, true);
      document.addEventListener("pointerdown", onAway, true);
    };
  });
}

// --- recording settings helpers ---

let APP_VERSION = ""; // from the app (see init)
const RES_OPTIONS = [["native", "Native"], ["1440", "1440p"], ["1080", "1080p"], ["720", "720p"]];
const FPS_OPTIONS = [30, 60, 120, 144];
const LENGTHS = [[15, "15 s"], [30, "30 s"], [60, "1 min"], [120, "2 min"], [180, "3 min"], [300, "5 min"], [600, "10 min"]];

// Encoders by what they run on, never by brand: "Graphics card", "Graphics card 2", "Processor".
function encoderLabel(list, id) {
  const codec = /hevc|265/.test(id) ? "HEVC" : /av1/.test(id) ? "AV1" : "H.264";
  if (/^lib/.test(id)) return `Processor · ${codec} (slow, last resort)`;
  const gpus = [...new Set(list.filter((e) => !/^lib/.test(e)).map((e) => e.replace(/^(h264|hevc|av1)_/, "")))];
  const n = gpus.indexOf(id.replace(/^(h264|hevc|av1)_/, ""));
  return `Graphics card${n > 0 ? " " + (n + 1) : ""} · ${codec}`;
}

// Mirrors the backend: 1080p60 H.264 high = 20 Mbps.
function estimateKbps(c, srcH) {
  const lim = { "1440": 1440, "1080": 1080, "720": 720 }[c.resolution] || srcH;
  const h = Math.min(srcH, lim);
  const w = Math.round((h * 16) / 9);
  const base = { small: 9000, balanced: 14000 }[c.quality] || 20000;
  const codecF = { hevc: 0.7, av1: 0.6 }[c.codec] || 1;
  const k = base * Math.pow((w * h) / (1920 * 1080), 0.75) * Math.pow(c.fps / 60, 0.6) * codecF;
  return Math.max(1500, Math.round(k / 500) * 500);
}

function ramText(c) {
  const mon = (state.capInfo && state.capInfo.monitors[0]) || { height: 1080 };
  const kbps = estimateKbps(c, mon.height || 1080);
  const mb = (kbps * 1000 / 8) * c.replay_secs / 1e6;
  return `About ${mb < 1000 ? Math.round(mb) + " MB" : (mb / 1000).toFixed(1) + " GB"} of RAM at ${(kbps / 1000).toFixed(0)} Mbps. Nothing is written to disk until you save.`;
}

function deviceOptions(list, selected) {
  const def = list.find((d) => d.default);
  const opts = [`<option value="">Windows default${def ? " (" + esc(def.name) + ")" : ""}</option>`];
  for (const d of list) opts.push(`<option value="${esc(d.id)}"${d.id === selected ? " selected" : ""}>${esc(d.name)}</option>`);
  return opts.join("");
}

const seg = (name, options, value) =>
  `<div class="seg" data-seg="${name}">${options.map(([v, label]) => `<button type="button" data-v="${esc(String(v))}" class="${String(v) === String(value) ? "on" : ""}">${esc(label)}</button>`).join("")}</div>`;

// ---------- themes ----------

const THEMES = [
  ["tally", "Tally", "Vermilion, like a camera's recording light", ["#ff5b37", "#f2a93b", "#8fae5d"], "#1b1a18"],
  ["volt", "Volt", "Electric chartreuse with sun and coral", ["#c6f135", "#ffd23f", "#ff7a59"], "#191c16"],
  ["bubblegum", "Bubblegum", "Pink with tangerine and lemon", ["#ff6fae", "#ffa04d", "#ffe066"], "#1d191b"],
];
const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");
let shownTheme = null; // what's on screen (a settings/setup draft can preview another)

function applyTheme(mode, theme) {
  mode = ["dark", "light", "system"].includes(mode) ? mode : "dark";
  theme = THEMES.some((t) => t[0] === theme) ? theme : "tally";
  shownTheme = { mode, theme };
  const resolved = mode === "system" ? (darkQuery.matches ? "dark" : "light") : mode;
  // Swap colours at once instead of fading every control through the change.
  const root = document.documentElement;
  root.classList.add("theme-swap");
  requestAnimationFrame(() => requestAnimationFrame(() => root.classList.remove("theme-swap")));
  document.documentElement.dataset.mode = resolved;
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem("clipit.theme", JSON.stringify(shownTheme)); } catch {}
  invoke("set_window_theme", { dark: resolved === "dark" }).catch(() => {});
  if (typeof drawAllWaves === "function") drawAllWaves();
}
const applySavedTheme = () => {
  applyTheme(state.config && state.config.theme_mode, state.config && state.config.theme);
  document.body.classList.toggle("reduce-motion", !!(state.config && state.config.reduce_motion));
};
darkQuery.addEventListener("change", () => { if (shownTheme && shownTheme.mode === "system") applyTheme(shownTheme.mode, shownTheme.theme); });

function themeCards(selected, attr) {
  return `<div class="themes">${THEMES.map(([id, name, sub, sw, bg]) => `
    <button type="button" class="theme-card${id === selected ? " on" : ""}" ${attr}="${id}">
      <span class="theme-swatch" style="background:${bg}"><i style="background:${sw[0]}"></i><i style="background:${sw[1]}"></i><i style="background:${sw[2]}"></i><b style="background:${sw[0]}"></b></span>
      <span class="theme-name">${name}</span><span class="theme-sub">${sub}</span>
    </button>`).join("")}</div>`;
}

// ---------- settings: a draft you save, grouped like iOS Settings ----------

const ICON = {
  general: `<svg viewBox="0 0 24 24"><path d="M4 7h10M18 7h2M4 17h4M12 17h8"/><circle cx="16" cy="7" r="2"/><circle cx="10" cy="17" r="2"/></svg>`,
  recording: `<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="8"/><circle cx="12" cy="12" r="3" fill="currentColor"/></svg>`,
  audio: `<svg viewBox="0 0 24 24"><path d="M4 9h4l5-4v14l-5-4H4z"/><path d="M16.5 9a4 4 0 0 1 0 6"/></svg>`,
  auto: `<svg viewBox="0 0 24 24"><path d="M13 3 5 14h6l-1 7 8-11h-6z"/></svg>`,
  hotkeys: `<svg viewBox="0 0 24 24"><rect x="3" y="6" width="18" height="12" rx="2"/><path d="M7 10h.01M11 10h.01M15 10h.01M8 14h8"/></svg>`,
  appearance: `<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="8"/><path d="M12 4a8 8 0 0 1 0 16z" fill="currentColor"/></svg>`,
  storage: `<svg viewBox="0 0 24 24"><path d="M3 6h7l2 2h9v11H3z"/></svg>`,
  background: `<svg viewBox="0 0 24 24"><path d="M4 14h6v6M20 10h-6V4M14 10l7-7M3 21l7-7"/></svg>`,
  handcam: `<svg viewBox="0 0 24 24"><rect x="3" y="7" width="13" height="10" rx="2"/><path d="m16 11 5-3v8l-5-3"/></svg>`,
  about: `<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8h.01"/></svg>`,
};
const SET_TABS = [
  ["general", "General", "var(--c4)"],
  ["recording", "Recording", "var(--accent)"],
  ["auto", "Auto clips", "var(--c3)"],
  ["handcam", "Hand cam", "var(--c2)"],
  ["audio", "Audio", "var(--c2)"],
  ["hotkeys", "Hotkeys", "var(--c3)"],
  ["appearance", "Appearance", "var(--accent)"],
  ["storage", "Storage", "var(--c4)"],
  ["background", "Space saver", "var(--c3)"],
  ["about", "About", "var(--faint)"],
];

const setUI = { tab: "general", base: null, draft: null };
const clone = (o) => JSON.parse(JSON.stringify(o));
const getPath = (o, path) => path.split(".").reduce((v, k) => (v == null ? v : v[k]), o);
function setPath(o, path, value) {
  const keys = path.split(".");
  const last = keys.pop();
  keys.reduce((v, k) => v[k], o)[last] = value;
}

// Only what changed, e.g. { capture: { fps: 120 } } (arrays are replaced whole).
function diffConfig(a, b) {
  const out = {};
  for (const k of Object.keys(b)) {
    const x = a ? a[k] : undefined;
    const y = b[k];
    if (y && typeof y === "object" && !Array.isArray(y) && x && typeof x === "object") {
      const d = diffConfig(x, y);
      if (Object.keys(d).length) out[k] = d;
    } else if (JSON.stringify(x) !== JSON.stringify(y)) {
      out[k] = y;
    }
  }
  return out;
}

function startSettingsDraft() {
  if (setUI.draft || !state.config) return;
  setUI.base = { ...clone(state.config), autostart: !!(state.status && state.status.autostart) };
  setUI.draft = clone(setUI.base);
}
const settingsDirty = () => !!(setUI.draft && JSON.stringify(setUI.draft) !== JSON.stringify(setUI.base));
function dropSettingsDraft() {
  setUI.base = setUI.draft = null;
  applySavedTheme();
}

// Leaving Settings with unsaved changes asks first. Resolves true when it's fine to go.
async function confirmLeaveSettings() {
  if (!settingsDirty()) return true;
  const leave = await confirmDialog("Leave without saving?", "Your changes to settings haven't been saved.", { yes: "Leave", no: "Keep editing", danger: true });
  return leave;
}

// Row builders.
const row = (label, sub, control, cls = "") =>
  `<div class="row ${cls}"><div class="row-text"><div class="row-label">${label}</div>${sub ? `<div class="row-sub">${sub}</div>` : ""}</div>${control ? `<div class="row-control">${control}</div>` : ""}</div>`;
const group = (title, rows, foot) =>
  `<div class="group">${title ? `<div class="group-title">${title}</div>` : ""}<div class="list">${rows.filter(Boolean).join("")}</div>${foot ? `<div class="group-foot" ${foot.id ? `id="${foot.id}"` : ""}>${foot.text || foot}</div>` : ""}</div>`;
const dv = (path) => getPath(setUI.draft, path);
const toggle = (path) => `<label class="switch"><input type="checkbox" data-bind="${path}" ${dv(path) ? "checked" : ""}></label>`;
const selectB = (path, options, num) =>
  `<select class="select" data-bind="${path}"${num ? ' data-num="1"' : ""}>${options.map(([v, l]) => `<option value="${esc(String(v))}"${String(v) === String(dv(path)) ? " selected" : ""}>${esc(l)}</option>`).join("")}</select>`;
const segB = (path, options, num) =>
  `<div class="seg" data-seg-bind="${path}"${num ? ' data-num="1"' : ""}>${options.map(([v, l]) => `<button type="button" data-v="${esc(String(v))}" class="${String(v) === String(dv(path)) ? "on" : ""}">${esc(l)}</button>`).join("")}</div>`;
const rangeB = (path, min, max, step) =>
  `<input type="range" class="range" min="${min}" max="${max}" step="${step}" value="${dv(path)}" data-bind="${path}" data-num="1"><span class="range-val">${dv(path)}%</span>`;
const fieldB = (path, placeholder, browse, type = "text") =>
  `<div class="field"><input type="${type}" data-bind="${path}" value="${esc(dv(path) || "")}" placeholder="${esc(placeholder)}" spellcheck="false">${browse ? `<button type="button" class="btn" data-browse="${path}">Browse</button>` : ""}</div>`;

// ffmpeg install, run in the background with winget.
// phase: null | "download" | "install" | "check" | "done" | "error"
const ffi = { phase: null, percent: null, error: null, version: "" };
const wizardOpen = () => !$("#wizard").classList.contains("hidden");

function ffmpegFixRow() {
  const s = state.status || {};
  const old = s.ffmpeg && !s.ffmpeg_capture;
  const wrap = (html) => `<div class="ffmpeg-row">${html}</div>`;
  if (ffi.phase === "done") {
    return wrap(row("ffmpeg is ready", wizardOpen() ? `Installed ffmpeg ${esc(ffi.version)}. Clipit restarts by itself when you finish setup.` : `Installed ffmpeg ${esc(ffi.version)}. Restarting Clipit…`,
      `<span class="status-badge on">Ready</span>`));
  }
  if (ffi.phase && ffi.phase !== "error") {
    const what = { download: "Downloading ffmpeg", install: "Installing ffmpeg", check: "Checking ffmpeg" }[ffi.phase] || "Installing ffmpeg";
    return wrap(row(`${what}${ffi.percent != null ? ` · ${ffi.percent}%` : "…"}`, "This runs in the background. You can keep going.",
      `<span class="ffmpeg-bar"><span style="width:${ffi.percent || 4}%"></span></span>`));
  }
  if (ffi.phase === "error") {
    return wrap(row("ffmpeg didn't install", esc(ffi.error || ""),
      `<button type="button" class="btn primary" data-ffmpeg="update">Try again</button><button type="button" class="btn" data-ffmpeg="window">Install in a window</button>`));
  }
  return wrap(row(old ? "Update ffmpeg" : "Install ffmpeg",
    old ? `Clipit found ffmpeg ${esc(s.ffmpeg_version || "")}, which can't record games. Recording needs ffmpeg 8 or newer.` : "Clipit records with ffmpeg, which isn't installed yet.",
    `<button type="button" class="btn primary" data-ffmpeg="update">${old ? "Update" : "Install"}</button>`));
}

function refreshFfmpegRows() {
  document.querySelectorAll(".ffmpeg-row").forEach((el) => {
    const t = document.createElement("div");
    t.innerHTML = ffmpegFixRow();
    const fresh = t.firstElementChild;
    el.replaceWith(fresh);
    wireFfmpegButtons(fresh);
  });
}

async function startFfmpegInstall() {
  if (ffi.phase && ffi.phase !== "error") return;
  Object.assign(ffi, { phase: "download", percent: null, error: null });
  refreshFfmpegRows();
  try {
    ffi.version = await invoke("install_ffmpeg");
    ffi.phase = "done";
    refreshFfmpegRows();
    // Recording starts with the new ffmpeg after a restart. During setup that
    // waits until setup is finished.
    if (!wizardOpen()) restartForFfmpeg();
  } catch (e) {
    Object.assign(ffi, { phase: "error", error: String(e) });
    refreshFfmpegRows();
  }
}

function restartForFfmpeg() {
  toast("ffmpeg is ready. Restarting Clipit to start recording…");
  setTimeout(() => invoke("restart_app").catch(() => {}), 2500);
}

listen("ffmpeg-install", (e) => {
  const { phase, percent } = e.payload || {};
  if (!ffi.phase || ffi.phase === "done" || ffi.phase === "error") return;
  ffi.phase = phase;
  ffi.percent = percent;
  refreshFfmpegRows();
});

function capStatusRow(st) {
  if (!st) return row("Recorder", "Checking…", "");
  if (st.state === "no-ffmpeg") return `<div id="cap-status">${row(esc(st.message), "Clipit installs it for you with Windows' own package manager (winget).", `<span class="status-badge bad">Stopped</span>`)}${ffmpegFixRow()}</div>`;
  const badge = st.recording ? `<span class="status-badge bad">Recording</span>` : st.state === "buffering" ? `<span class="status-badge on">Ready</span>` : st.state === "error" || st.state === "no-ffmpeg" ? `<span class="status-badge bad">Stopped</span>` : `<span class="status-badge">${esc({ waiting: "Waiting", off: "Off", starting: "Starting" }[st.state] || st.state)}</span>`;
  const sub = st.state === "buffering" ? `${esc(st.encoder)} · ${esc(st.detail)} · ${fmtSize(st.ram_bytes)} in RAM` : "";
  return `<div class="row" id="cap-status"><div class="row-text"><div class="row-label">${esc(st.message)}</div>${sub ? `<div class="row-sub">${sub}</div>` : ""}</div><div class="row-control">${badge}</div></div>`;
}

// One sentence saying what the auto-clip settings will do.
function autoSummary(c) {
  if (!c.enabled) return "Turn on instant replay (Recording) to use auto clips.";
  if (!c.auto_clip_kills) return "Auto clips are off. Clipit still counts kills and tags map and agent in clips you save.";
  const min = c.auto_min_kills;
  const what = min >= 5 ? "aces" : `${multiText(min)}s or better`;
  const close = c.auto_max_gap_secs >= 999 ? "anywhere in a round" : `with at most ${c.auto_max_gap_secs} s between kills`;
  const whole = c.auto_whole_round_from === 4 ? " Every 4K and ace is clipped from the whole round, however spread out." : c.auto_whole_round_from === 5 ? " Every ace is clipped from the whole round, however spread out." : "";
  return `Clips ${what} ${close}.${whole} Each clip starts ${c.kill_before_secs} s before the first kill, ends ${c.kill_after_secs} s after the last and is at most ${c.auto_max_mb >= 1000 ? c.auto_max_mb / 1000 + " GB" : c.auto_max_mb + " MB"}.${c.auto_skip_deathmatch ? " Deathmatch and Team Deathmatch are skipped." : ""}`;
}

// Longest auto clip (seconds) for the size limit at the recording bitrate.
function autoMaxSecs(c) {
  const kbps = estimateKbps(c, (state.capInfo && state.capInfo.monitors[0] && state.capInfo.monitors[0].height) || 1080);
  return Math.max(10, Math.floor(c.auto_max_mb * 8000 / kbps));
}

function autoRam(c) {
  const kbps = estimateKbps(c, (state.capInfo && state.capInfo.monitors[0] && state.capInfo.monitors[0].height) || 1080);
  const secs = Math.min(autoMaxSecs(c), c.kill_before_secs + 150 + c.kill_after_secs) + 45;
  return Math.round(kbps * 1000 / 8 * secs / 1e6);
}

function settingsPane(tab) {
  const d = setUI.draft;
  const c = d.capture;
  const st = state.status || {};
  const info = state.capInfo || { devices: { outputs: [], inputs: [] }, monitors: [], encoders: [] };
  const keyErr = (h, saved) => (h && h.hotkey && !h.registered && h.hotkey === saved ? `<span class="row-sub" style="color:var(--danger)">Taken by another app</span>` : "");
  if (tab === "general") {
    return `<h2>General</h2><p class="lede">Starting up and how Clipit opens.</p>
      ${group("Startup", [row("Start with Windows", "Starts quietly in the tray at sign-in, so hotkeys and instant replay work right away.", toggle("autostart"))])}
      ${group("Opening Clipit", [
        row("How Clipit opens", d.window_mode === "quick" ? "The hotkey, the tray and launching Clipit all open the quick view." : d.window_mode === "app" ? "Everything opens the normal app window." : `${esc(fmtKey(d.hotkey))} opens the quick view over your game. The tray icon and Settings open the app window.`, "", "stack"),
        `<div class="row" style="padding-top:0">${segB("window_mode", [["both", "Quick view + app"], ["quick", "Quick view only"], ["app", "App only"]])}</div>`,
        row("Quick view entrance", "", segB("animation", [["blur", "Blur"], ["dim", "Dim"], ["none", "None"]])),
        d.animation === "blur" ? row("Blur strength", "", rangeB("blur_strength", 10, 100, 5)) : "",
      ])}
      ${group("Setup", [row("First-time setup", "Go through hotkeys, audio, capture and theme again.", `<button type="button" class="btn" id="set-wizard">Run setup</button>`)])}`;
  }
  if (tab === "recording") {
    const codecs = [["h264", "H.264 · plays everywhere"], ["hevc", "HEVC · smaller"], ["av1", "AV1 · smallest"]].filter(([k]) => info.encoders.some((e) => (k === "h264" ? /h264|x264/ : k === "hevc" ? /hevc|x265/ : /av1/).test(e)));
    const encoders = info.encoders.filter((e) => (c.codec === "hevc" ? /hevc|265/.test(e) : c.codec === "av1" ? /av1/.test(e) : /h264|264/.test(e)));
    const games = c.games || [];
    return `<h2>Recording</h2><p class="lede">Instant replay stays in RAM, so your drive isn't written to while you play.</p>
      ${group("", [capStatusRow(state.capStatus)])}
      ${group("Instant replay", [
        row("Instant replay", "Always keep the last moments ready to save.", toggle("capture.enabled")),
        row("Recorder", c.engine === "classic" ? "The older recorder, using ffmpeg. Use it only if the new one gives you trouble." : "Clipit's own engine: captures and encodes on your graphics card and saves clips instantly. Long recordings still use the classic recorder for now.", segB("capture.engine", [["native", "Clipit engine"], ["classic", "Classic"]])),
        row("Replay length", "", segB("capture.replay_secs", LENGTHS.filter((l) => l[0] >= 30 && l[0] <= 300), true)),
        row("Don't repeat footage", "Clip twice in a row and the second clip starts where the first one ended (at least 5 s), instead of saving the whole replay again.", toggle("capture.no_repeat")),
      ], { id: "ram-hint", text: esc(ramText(c)) })}
      ${group("What to capture", [
        row("Source", c.target === "monitor" ? "Everything on one screen." : "Only the game. Nothing runs until a game is in focus.", segB("capture.target", [["game", "Game window"], ["monitor", "Whole monitor"]])),
        c.target === "monitor"
          ? row("Monitor", "", selectB("capture.monitor", info.monitors.map((m) => [m.id, m.label])))
          : row("Games", "", segB("capture.games_mode", [["all", "Every game"], ["selected", "Only ticked"]])),
        ...(c.target === "monitor" ? [] : games.map((g, i) => row(esc(g.name), esc(g.exe), `${toggle(`capture.games.${i}.enabled`)}<button type="button" class="icon-btn" data-game-del="${i}" title="Forget this game"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>`, "indent"))),
        c.target !== "monitor" && !games.length ? row("No games yet", "Games show up here the first time you play them.", "", "indent") : "",
        row("Show the mouse cursor", "", toggle("capture.cursor")),
      ])}
      ${group("Video", [
        row("Resolution", "", selectB("capture.resolution", RES_OPTIONS)),
        row("Frame rate", "", selectB("capture.fps", FPS_OPTIONS.map((f) => [f, f + " fps"]), true)),
        row("Quality", "", selectB("capture.quality", [["high", "High"], ["balanced", "Balanced"], ["small", "Smaller files"]])),
        row("Format", "", selectB("capture.codec", codecs)),
        row("Encoder", "", selectB("capture.encoder", [["auto", "Automatic (best on this PC)"], ...encoders.map((e) => [e, encoderLabel(info.encoders, e)])])),
      ], "H.264 plays in Discord and everywhere else. HEVC and AV1 make smaller files, but some apps can't play them.")}
      ${group("Sounds", [row("Chimes", "A short sound when a clip is saved or a recording starts or stops.", toggle("capture.sounds"))])}
      ${group("Troubleshooting", [row("Restart recorder", "If clipping lags or stops working, this starts capture, audio and the hand cam fresh, like reopening Clipit. A recording in progress is saved first, and the replay starts over.", `<button type="button" class="btn" id="set-restart-rec">Restart</button>`)])}`;
  }
  if (tab === "auto") {
    const on = c.auto_clip_kills;
    const fmtSecs = (x) => (x >= 60 ? `${Math.floor(x / 60)} min${x % 60 ? " " + (x % 60) + " s" : ""}` : `${x} s`);
    return `<h2>Auto clips</h2><p class="lede">VALORANT highlights, saved for you while you play.</p>
      <div class="group"><div class="list"><div class="row"><div class="row-text"><div class="row-label">What happens</div><div class="row-sub" id="auto-summary">${esc(autoSummary(c))}</div></div></div></div></div>
      ${group("", [row("Auto clip highlights", "3Ks, 4Ks and aces in VALORANT, each saved as its own clip and tagged Auto clipped.", toggle("capture.auto_clip_kills"))])}
      ${on ? group("What counts", [
        row("Fewest kills", "A streak needs at least this many of your kills.", segB("capture.auto_min_kills", [[2, "2K"], [3, "3K"], [4, "4K"], [5, "Ace"]], true)),
        row("Kills close together", "Kills further apart than this start a new streak.", segB("capture.auto_max_gap_secs", [[10, "10 s"], [15, "15 s"], [20, "20 s"], [30, "30 s"], [999, "Any"]], true)),
        row("Whole round for big rounds", "Clip these from the first kill to the last, however spread out.", segB("capture.auto_whole_round_from", [[4, "4K and aces"], [5, "Aces"], [0, "Off"]], true)),
        row("Skip Deathmatch", "No auto clips in Deathmatch, Team Deathmatch and other modes without rounds.", toggle("capture.auto_skip_deathmatch")),
      ]) : ""}
      ${on ? group("Clip length and size", [
        row("Before the first kill", "", segB("capture.kill_before_secs", [[3, "3 s"], [6, "6 s"], [10, "10 s"], [15, "15 s"]], true)),
        row("After the last kill", "", segB("capture.kill_after_secs", [[1, "1 s"], [3, "3 s"], [5, "5 s"], [8, "8 s"]], true)),
        row("Largest clip", `At your quality that's about ${fmtSecs(autoMaxSecs(c))}. Longer highlights keep their end.`, segB("capture.auto_max_mb", [[100, "100 MB"], [200, "200 MB"], [300, "300 MB"], [500, "500 MB"], [1000, "1 GB"]], true)),
      ], `While VALORANT is being recorded, the replay keeps enough to finish a highlight: up to about ${autoRam(c)} MB of RAM. A highlight is saved once you've gone 40 seconds without a kill, so the clip arrives a little after the round.`) : ""}
      ${group("Tags", [row("Count kills and tag map and agent", "For every VALORANT clip, saved by you or automatically.", toggle("capture.count_kills"))],
        "Kills are read from Clipit's own recording, never from your screen or the game, so it stays out of Vanguard's way. Map and agent are read from the saved clip: the minimap (even zoomed in or rotating) and your ability icons.")}`;
  }
  if (tab === "audio") {
    return `<h2>Audio</h2><p class="lede">Both follow your Windows default device unless you pick one.</p>
      ${group("Desktop audio", [
        row("Record desktop audio", "Game, music and voice chat.", toggle("capture.desktop_audio")),
        c.desktop_audio ? row("Device", "", `<select class="select" data-bind="capture.desktop_device">${deviceOptions(info.devices.outputs, c.desktop_device)}</select>`) : "",
        c.desktop_audio ? row("Volume", "", rangeB("capture.desktop_volume", 0, 200, 5)) : "",
      ])}
      ${group("Microphone", [
        row("Record your mic", "", toggle("capture.mic")),
        c.mic ? row("Device", "", `<select class="select" data-bind="capture.mic_device">${deviceOptions(info.devices.inputs, c.mic_device)}</select>`) : "",
        c.mic ? row("Volume", "", rangeB("capture.mic_volume", 0, 300, 5)) : "",
        c.mic ? row("Noise suppression", "Removes fan hum and keyboard hiss.", toggle("capture.noise_suppression")) : "",
      ])}
      ${c.desktop_audio && c.mic ? group("Tracks", [row("Separate tracks", "Also save desktop and mic on their own tracks.", toggle("capture.separate_tracks"))], "Track 1 is always the mix, which is what Discord plays. Tracks 2 and 3 let video editors adjust them separately.") : ""}`;
  }
  if (tab === "hotkeys") {
    const saved = state.config || {};
    return `<h2>Hotkeys</h2><p class="lede">They work everywhere, even in-game.</p>
      ${group("", [
        row("Save clip", `Saves the last ${esc(LENGTHS.find((l) => l[0] === c.replay_secs)?.[1] || c.replay_secs + " s")}`, keyButton("capture.clip_hotkey", c.clip_hotkey) + keyErr(st.clip_hotkey, saved.capture && saved.capture.clip_hotkey)),
        row("Start or stop recording", "Records until you press it again.", keyButton("capture.record_hotkey", c.record_hotkey) + keyErr(st.record_hotkey, saved.capture && saved.capture.record_hotkey)),
        row("Open quick view", "Pops Clipit over whatever you're doing.", keyButton("hotkey", d.hotkey) + keyErr(st.hotkey, saved.hotkey)),
      ], "Click a key, then press a key or a combination. Backspace clears it, Esc cancels.")}
      <div class="group-foot">${SINGLE_NOTE}</div>`;
  }
  if (tab === "appearance") {
    return `<h2>Appearance</h2><p class="lede">Changes show right away. Save to keep them.</p>
      ${group("Mode", [row("Mode", "", segB("theme_mode", [["dark", "Dark"], ["light", "Light"], ["system", "Match Windows"]]))])}
      ${group("Motion", [row("Reduce motion", "Turns off the small animations inside the app (buttons, pages, cards). Also on when Windows' animation effects are off.", toggle("reduce_motion"))])}
      <div class="group"><div class="group-title">Theme</div>${themeCards(d.theme, "data-theme-pick")}</div>`;
  }
  if (tab === "handcam") return handcamPane(d);
  if (tab === "background") return backgroundPane(d);
  if (tab === "storage") {
    return `<h2>Storage</h2><p class="lede">Where clips live.</p>
      ${group("Clips folder", [`<div class="row stack">${fieldB("clips_root", "Choose a folder", true)}<div class="row-sub">${st.root_exists ? "Folder found. New clips go here, each game in its own folder." : "Folder not found yet. It's created when the first clip is saved."}</div></div>`], "Every folder inside it shows up as a game, even before it has clips. Clips in folders inside those count for that game.")}
      ${yourFoldersGroup()}
      ${foldersGroup(d)}
      ${group("Exports", [`<div class="row stack">${fieldB("export_dir", "Next to the original clip (recommended)", true)}</div>`], "Edited clips are saved next to the original so they show up in your library. Pick a folder only if you'd rather keep them somewhere else; they won't appear in the library then.")}
      ${group("Game artwork", [`<div class="row stack">${fieldB("steamgriddb_key", "SteamGridDB API key", false, "password")}</div>`, row("Download artwork again", st.has_art_key ? "Uses the saved key." : "Save a key first.", `<button type="button" class="btn" id="set-refresh-art">Refresh</button>`)], "Covers and logos come from SteamGridDB. Make a free account and copy your API key from steamgriddb.com/profile/preferences/api.")}
      ${group("Tools", [
        row("ffmpeg", st.ffmpeg ? `${esc(st.ffmpeg_version || "")} · <code>${esc(st.ffmpeg)}</code>` : "Not found.", st.ffmpeg && st.ffmpeg_capture ? `<span class="status-badge on">Ready</span>` : st.ffmpeg ? `<span class="status-badge bad">Too old</span>` : `<span class="status-badge bad">Missing</span>`),
        !(st.ffmpeg && st.ffmpeg_capture) ? ffmpegFixRow() : "",
      ])}`;
  }
  const u = upd;
  const status = u.checking ? "Checking…" : u.installing ? (u.progress != null ? `Downloading ${Math.round(u.progress * 100)}%` : "Installing…") : u.available ? `Version ${esc(u.available.version)} is ready to install.` : u.error ? `Couldn't check: ${esc(u.error)}` : u.checked ? "You're on the latest version." : "";
  return `<h2>About</h2><p class="lede">A lightweight game clipper and clip library. Open source under the MIT license.</p>
    ${group("Updates", [
      row(`Clipit ${esc(APP_VERSION)}`, status, u.available
        ? `<button type="button" class="btn primary" id="upd-install" ${u.installing ? "disabled" : ""}>Update and restart</button>`
        : `<button type="button" class="btn" id="upd-check" ${u.checking ? "disabled" : ""}>Check for updates</button>`),
      u.available && u.available.notes ? `<div class="row stack"><div class="row-sub" style="white-space:pre-wrap">${esc(u.available.notes)}</div></div>` : "",
      row("Check automatically", "Looks for a new version when Clipit starts. Installing always waits for you.", toggle("auto_update")),
    ], "Updates come from github.com/MoathByte/clipit and are signed; Clipit only installs files signed by the project. A recording in progress is finished and saved first.")}
    ${group("", [row("Replay buffer", "Kept in RAM; clips are written once, front to back.", ""), row("VALORANT", "Kills are read from Clipit's own recording and saved clips, never from your screen or the game.", "")])}`;
}

function wireFfmpegButtons(root) {
  root.querySelectorAll("[data-ffmpeg]").forEach((b) => (b.onclick = async () => {
    try {
      if (b.dataset.ffmpeg === "update") startFfmpegInstall();
      else if (b.dataset.ffmpeg === "window") {
        await invoke("update_ffmpeg");
        toast("A window is installing ffmpeg. When it says Done, Clipit restarts by itself.");
        waitForWindowInstall();
      } else await invoke("restart_app");
    } catch (e) { toast(String(e), true); }
  }));
}

// After "Install in a window": look for the new ffmpeg every few seconds.
let ffWatch = 0;
function waitForWindowInstall() {
  clearInterval(ffWatch);
  let tries = 0;
  ffWatch = setInterval(async () => {
    if (++tries > 200) return clearInterval(ffWatch);
    try {
      if (!(await invoke("ffmpeg_ready"))) return;
    } catch { return; }
    clearInterval(ffWatch);
    ffi.phase = null;
    startFfmpegInstall(); // finds it right away and reports "ready"
  }, 3000);
}

// ---------- hand cam settings ----------

const hcState = { cams: null, loadingCams: false, img: null, imgDevice: null, imgError: null, loadingImg: false, stage: null };
const HC_ASPECT = { "16:9": 16 / 9, "4:3": 4 / 3, "1:1": 1, "3:4": 3 / 4, "9:16": 9 / 16 };
const hcAspect = (h) => (h.shape === "circle" ? 1 : HC_ASPECT[h.aspect] || 16 / 9);

// Box in stage fractions, kept inside the frame (same rules as the saved clip).
function hcBox(h) {
  const a = hcAspect(h);
  let w = Math.min(1, Math.max(0.05, h.size));
  let hh = (w * 16 / 9) / a;
  if (hh > 1) { hh = 1; w = a * 9 / 16; }
  const x = Math.min(Math.max(0, h.x), 1 - w);
  const y = Math.min(Math.max(0, h.y), 1 - hh);
  return { x, y, w, h: hh };
}

function hcStatusText(h) {
  const cs = state.capStatus || {};
  if (!h.device) return "Pick a camera to start.";
  if (cs.handcam_error) return `<span class="bad-text">${esc(cs.handcam_error)}</span>`;
  if (cs.handcam_on) return "Recording the camera with the game.";
  return "The camera turns on with the game capture (save first if you just changed it).";
}

function hcPictureStyle(h) {
  const t = [];
  if (h.rotate) t.push(`rotate(${h.rotate}deg)`);
  if (h.flip_h) t.push("scaleX(-1)");
  if (h.flip_v) t.push("scaleY(-1)");
  t.push(`scale(${Math.max(1, h.zoom || 1)})`);
  const sideways = h.rotate === 90 || h.rotate === 270;
  return `transform:${t.join(" ")};${sideways ? "width:var(--hc-h);height:var(--hc-w);" : ""}`;
}

function handcamPane(d) {
  const h = d.capture.handcam;
  const intro = group("", [row("Add a hand cam to clips", "Records a camera next to the game and puts it on every clip you save, auto clips too. It's only added to saved clips, and the camera is only on while a game is captured.", toggle("capture.handcam.enabled"))]);
  if (!h.enabled) return `<h2>Hand cam</h2><p class="lede">Show your hands or face in your clips.</p>${intro}`;
  const cams = hcState.cams;
  const list = cams ? [...cams] : [];
  if (h.device && !list.includes(h.device)) list.unshift(h.device);
  const camControl = cams === null && !h.device
    ? `<span class="row-sub">Looking for cameras…</span>`
    : `${selectB("capture.handcam.device", [["", list.length ? "Choose a camera" : "No camera found"], ...list.map((c) => [c, c])])}<button type="button" class="icon-btn" id="hc-rescan" title="Look for cameras again"><svg viewBox="0 0 24 24"><path d="M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6"/></svg></button>`;
  const b = hcBox(h);
  const pic = hcState.img && hcState.imgDevice === h.device
    ? `<img src="${hcState.img}" alt="" draggable="false">`
    : `<span>${h.device ? (hcState.loadingImg ? "Getting a picture…" : esc(hcState.imgError || "No picture yet")) : "Camera"}</span>`;
  const corners = [["tl", "Top left"], ["tr", "Top right"], ["bl", "Bottom left"], ["br", "Bottom right"]];
  return `<h2>Hand cam</h2><p class="lede">Show your hands or face in your clips.</p>${intro}
    ${group("Camera", [
      row("Camera", "Phone apps like Iriun Webcam show up here too.", camControl),
      h.device ? row("Camera settings", "Exposure, white balance, focus… from the camera itself, like Configure Video in OBS.", `<button type="button" class="btn" id="hc-dialog">Open</button>`) : "",
      row("Quality", "", segB("capture.handcam.height", [[480, "480p"], [720, "720p"], [1080, "1080p"]], true)),
      row("Frame rate", "", segB("capture.handcam.fps", [[30, "30 fps"], [60, "60 fps"]], true)),
    ], { text: hcStatusText(h), id: "hc-status" })}
    <div class="group">
      <div class="group-title">Placement</div>
      <div class="hc-stage" id="hc-stage" style="${hcState.stage ? `background-image:url('${hcState.stage}')` : ""}">
        <div class="hc-box shape-${esc(h.shape)} border-${esc(h.border)}" id="hc-box" style="left:${b.x * 100}%;top:${b.y * 100}%;width:${b.w * 100}%;height:${b.h * 100}%">
          <div class="hc-clip"><div class="hc-pic" id="hc-pic" style="${hcPictureStyle(h)}">${pic}</div></div>
          <div class="hc-resize" id="hc-resize" title="Drag to resize"></div>
        </div>
      </div>
      <div class="hc-under">
        <div class="seg" id="hc-corners">${corners.map(([v, l]) => `<button type="button" data-corner="${v}">${l}</button>`).join("")}</div>
        <button type="button" class="btn ghost" id="hc-refresh"${h.device ? "" : " disabled"}>New picture</button>
        <div class="hc-size"><span>Size</span><input type="range" class="range" id="hc-size" min="8" max="60" step="1" value="${Math.round(h.size * 100)}"><span class="range-val" id="hc-size-val">${Math.round(h.size * 100)}%</span></div>
      </div>
      <div class="group-foot">Drag the camera to move it, or its corner to resize. Mind the killfeed (top right), minimap (top left) and abilities (bottom middle).</div>
    </div>
    ${group("Look", [
      row("Shape", "", segB("capture.handcam.shape", [["rect", "Rectangle"], ["rounded", "Rounded"], ["circle", "Circle"]])),
      h.shape === "circle" ? "" : row("Crop", "The camera picture is cropped to this shape.", segB("capture.handcam.aspect", [["16:9", "16:9"], ["4:3", "4:3"], ["1:1", "1:1"], ["3:4", "3:4"], ["9:16", "9:16"]])),
      row("Border", "", segB("capture.handcam.border", [["none", "None"], ["white", "White"], ["black", "Black"]])),
    ])}
    ${group("Picture", [
      row("Mirror", "Flip left and right.", toggle("capture.handcam.flip_h")),
      row("Upside down", "Flip top and bottom.", toggle("capture.handcam.flip_v")),
      row("Rotate", "For a phone held sideways.", segB("capture.handcam.rotate", [[0, "0°"], [90, "90°"], [180, "180°"], [270, "270°"]], true)),
      row("Zoom", "Zooms into the middle of the picture.", `<input type="range" class="range" id="hc-zoom" min="100" max="300" step="5" value="${Math.round((h.zoom || 1) * 100)}"><span class="range-val" id="hc-zoom-val">${Math.round((h.zoom || 1) * 100)}%</span>`),
    ])}
    ${group("Adjustments", hcAdjustRows(h), "Applied by Clipit, so they work with any camera. The preview shows the result.")}
    ${group("Timing", [
      row("Camera delay", "Phone cameras arrive a little late. If your hands move after the game does, move this left.", `<input type="range" class="range" id="hc-delay" min="-1000" max="1000" step="20" value="${h.delay_ms}"><span class="range-val" id="hc-delay-val">${h.delay_ms > 0 ? "+" : ""}${h.delay_ms} ms</span>`),
    ], "Clips save instantly. The camera is added a few seconds later in the background, on the graphics card when it can, and the clip keeps its size. Kills, map and agent are read from the game alone.")}`;
}

async function hcLoadCams(force) {
  if (hcState.loadingCams || (hcState.cams && !force)) return;
  hcState.loadingCams = true;
  try { hcState.cams = await invoke("list_cameras"); } catch { hcState.cams = []; }
  hcState.loadingCams = false;
  const d = setUI.draft && setUI.draft.capture.handcam;
  if (d && !d.device && hcState.cams.length === 1) onDraftChange("capture.handcam.device", hcState.cams[0]);
  else if (setUI.tab === "handcam") rerenderSettings();
}

async function hcLoadPicture(fresh = true) {
  const h = setUI.draft && setUI.draft.capture.handcam;
  if (!h || !h.enabled || !h.device || hcState.loadingImg) return;
  hcState.loadingImg = true;
  hcState.imgError = null;
  const device = h.device;
  try {
    hcState.img = await invoke("camera_preview", { handcam: h, fresh });
    hcState.imgDevice = device;
  } catch (e) {
    hcState.img = null;
    hcState.imgError = String(e);
  }
  hcState.loadingImg = false;
  if (state.view.name === "settings" && setUI.tab === "handcam") rerenderSettings();
}

// Sliders: re-adjust the last picture (fast) and swap it in without rebuilding.
let hcAdjustTimer = 0;
let hcAdjustBusy = false;
let hcAdjustAgain = false;
function hcAdjustPreview() {
  clearTimeout(hcAdjustTimer);
  hcAdjustTimer = setTimeout(async () => {
    const h = setUI.draft && setUI.draft.capture.handcam;
    if (!h || !h.device || !hcState.img) return;
    if (hcAdjustBusy) { hcAdjustAgain = true; return; }
    hcAdjustBusy = true;
    try {
      hcState.img = await invoke("camera_preview", { handcam: h, fresh: false });
      const img = $("#hc-pic img");
      if (img) img.src = hcState.img;
    } catch {}
    hcAdjustBusy = false;
    if (hcAdjustAgain) { hcAdjustAgain = false; hcAdjustPreview(); }
  }, 120);
}

const HC_ADJ = [
  ["brightness", "Brightness", -100, 100, 5, 0, (v) => (v > 0 ? "+" : "") + v],
  ["contrast", "Contrast", 25, 200, 5, 100, (v) => v + "%"],
  ["saturation", "Saturation", 0, 200, 5, 100, (v) => v + "%"],
  ["gamma", "Gamma", 50, 200, 5, 100, (v) => v + "%"],
  ["temperature", "Colour temperature", 3000, 9000, 100, 6500, (v) => (v === 6500 ? "Neutral" : v < 6500 ? `Warmer · ${v} K` : `Cooler · ${v} K`)],
  ["sharpness", "Sharpness", 0, 100, 5, 0, (v) => (v ? v + "%" : "Off")],
];
const HC_ADJ_SUB = {
  gamma: "Brightens dark rooms without blowing out bright parts.",
  temperature: "Fixes orange or blue light.",
};

function hcAdjustRows(h) {
  return HC_ADJ.map(([k, label, min, max, step, def, fmt]) =>
    row(label, HC_ADJ_SUB[k] || "", `<input type="range" class="range" data-hc-adj="${k}" min="${min}" max="${max}" step="${step}" value="${h[k]}"><span class="range-val wide" id="hc-adj-${k}">${fmt(h[k])}</span><button type="button" class="icon-btn${h[k] === def ? " invisible" : ""}" data-hc-reset="${k}" title="Reset"><svg viewBox="0 0 24 24"><path d="M4 11a8 8 0 1 1 2.3 5.7M4 5v6h6"/></svg></button>`));
}

// A frame from your newest clip behind the camera box.
function hcLoadStage() {
  if (hcState.stage !== null || !state.lib || !state.lib.clips.length) return;
  hcState.stage = "";
  const c = [...state.lib.clips].sort((a, b) => b.t - a.t)[0];
  invoke("get_thumbnail", { path: c.path }).then((p) => {
    hcState.stage = fileUrl(p);
    const st = $("#hc-stage");
    if (st) st.style.backgroundImage = `url('${hcState.stage}')`;
  }).catch(() => {});
}

function wireHandcam() {
  const h = setUI.draft.capture.handcam;
  if (!h.enabled) return;
  hcLoadCams(false);
  hcLoadStage();
  if (h.device && hcState.imgDevice !== h.device && !hcState.loadingImg && !hcState.imgError) hcLoadPicture();
  const dlg = $("#hc-dialog");
  if (dlg) dlg.onclick = async () => {
    dlg.disabled = true;
    dlg.textContent = "Open in another window";
    try {
      await invoke("camera_settings", { device: h.device });
      hcState.imgDevice = null;
      hcLoadPicture(true);
    } catch (e) { toast(String(e), true); }
    dlg.disabled = false;
    dlg.textContent = "Open";
  };
  const fresh = $("#hc-refresh");
  if (fresh) fresh.onclick = () => { hcState.imgError = null; hcLoadPicture(true); };
  main.querySelectorAll("[data-hc-adj]").forEach((r) => {
    const k = r.dataset.hcAdj;
    const spec = HC_ADJ.find((a) => a[0] === k);
    const show = () => r.style.setProperty("--v", (r.value - r.min) / (r.max - r.min) * 100 + "%");
    show();
    r.addEventListener("input", () => {
      const v = Number(r.value);
      setPath(setUI.draft, "capture.handcam." + k, v);
      $("#hc-adj-" + k).textContent = spec[6](v);
      main.querySelector(`[data-hc-reset="${k}"]`).classList.toggle("invisible", v === spec[5]);
      show();
      updateSaveBar();
      hcAdjustPreview();
    });
  });
  main.querySelectorAll("[data-hc-reset]").forEach((b) => (b.onclick = () => {
    const k = b.dataset.hcReset;
    const spec = HC_ADJ.find((a) => a[0] === k);
    const r = main.querySelector(`[data-hc-adj="${k}"]`);
    r.value = spec[5];
    r.dispatchEvent(new Event("input"));
  }));
  const rescan = $("#hc-rescan");
  if (rescan) rescan.onclick = () => { hcState.imgError = null; hcState.imgDevice = null; hcLoadCams(true).then(hcLoadPicture); };
  const stage = $("#hc-stage");
  const box = $("#hc-box");
  if (!stage || !box) return;
  const place = () => {
    const b = hcBox(h);
    Object.assign(box.style, { left: b.x * 100 + "%", top: b.y * 100 + "%", width: b.w * 100 + "%", height: b.h * 100 + "%" });
    const r = box.getBoundingClientRect();
    box.style.setProperty("--hc-w", r.width + "px");
    box.style.setProperty("--hc-h", r.height + "px");
    $("#hc-size").value = Math.round(h.size * 100);
    $("#hc-size-val").textContent = Math.round(h.size * 100) + "%";
    updateSaveBar();
  };
  place();
  const set = (k, v) => { setPath(setUI.draft, "capture.handcam." + k, v); };
  // Move: keep the grab point under the pointer; snap to the edges.
  box.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.id === "hc-resize") return;
    e.preventDefault();
    box.setPointerCapture(e.pointerId);
    const sr = stage.getBoundingClientRect();
    const b = hcBox(h);
    const gx = (e.clientX - sr.left) / sr.width - b.x;
    const gy = (e.clientY - sr.top) / sr.height - b.y;
    box.classList.add("dragging");
    const move = (ev) => {
      const bb = hcBox(h);
      const snap = (v, max) => (v < 0.02 ? 0.02 : v > max - 0.02 ? max - 0.02 : v);
      set("x", Math.round(snap((ev.clientX - sr.left) / sr.width - gx, 1 - bb.w) * 1000) / 1000);
      set("y", Math.round(snap((ev.clientY - sr.top) / sr.height - gy, 1 - bb.h) * 1000) / 1000);
      place();
    };
    const up = () => { box.classList.remove("dragging"); box.removeEventListener("pointermove", move); box.removeEventListener("pointerup", up); };
    box.addEventListener("pointermove", move);
    box.addEventListener("pointerup", up);
  });
  // Resize from the corner (keeps the top-left where it is).
  $("#hc-resize").addEventListener("pointerdown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    const sr = stage.getBoundingClientRect();
    const move = (ev) => {
      const b = hcBox(h);
      const w = (ev.clientX - sr.left) / sr.width - b.x;
      set("size", Math.round(Math.min(0.6, Math.max(0.08, w)) * 1000) / 1000);
      place();
    };
    const up = () => { el.removeEventListener("pointermove", move); el.removeEventListener("pointerup", up); };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  });
  $("#hc-corners").querySelectorAll("button").forEach((btn) => (btn.onclick = () => {
    const b = hcBox(h);
    const m = 0.02;
    const c = btn.dataset.corner;
    set("x", c[1] === "l" ? m : Math.round((1 - b.w - m) * 1000) / 1000);
    set("y", c[0] === "t" ? m : Math.round((1 - b.h - m) * 1000) / 1000);
    place();
  }));
  $("#hc-size").addEventListener("input", (e) => { set("size", Number(e.target.value) / 100); place(); });
  const zoom = $("#hc-zoom");
  zoom.addEventListener("input", () => {
    set("zoom", Number(zoom.value) / 100);
    $("#hc-zoom-val").textContent = zoom.value + "%";
    $("#hc-pic").setAttribute("style", hcPictureStyle(h));
    updateSaveBar();
  });
  const delay = $("#hc-delay");
  delay.addEventListener("input", () => {
    const v = Number(delay.value);
    set("delay_ms", v);
    $("#hc-delay-val").textContent = `${v > 0 ? "+" : ""}${v} ms`;
    updateSaveBar();
  });
  [zoom, delay, $("#hc-size")].forEach((r) => {
    const show = () => r.style.setProperty("--v", (r.value - r.min) / (r.max - r.min) * 100 + "%");
    show();
    r.addEventListener("input", show);
  });
}

// ---------- storage: extra folders ----------

// The folders in your clips folder (the same ones as in the sidebar).
function yourFoldersGroup() {
  const games = state.lib ? state.lib.games : [];
  const rows = games.map((g) => row(`${gameIcon(g.name)}<span>${esc(g.name)}</span>`, `${g.clip_count} clip${g.clip_count === 1 ? "" : "s"} · ${fmtSize(g.total_size)}`,
    `<button type="button" class="btn sm ghost" data-folder-rename="${esc(g.name)}">Rename</button><button type="button" class="btn sm ghost" data-folder-delete="${esc(g.name)}" ${g.clip_count ? "disabled title=\"Only empty folders can be deleted\"" : ""}>Delete</button>`, "folder-row"));
  rows.push(row("New folder", "Group clips your way: funny moments, clutches, a friend's plays…", `<button type="button" class="btn" id="set-folder-new">New folder</button>`));
  return group("Your folders", rows, "These are real folders inside your clips folder, so they look the same in File Explorer. To move clips, select them on any page and choose Move to….");
}

function foldersGroup(d) {
  const lib = state.lib || {};
  const info = (p) => (lib.folders || []).find((f) => f.path.toLowerCase() === p.toLowerCase().replace(/[\\/]+$/, ""));
  const rows = (d.extra_folders || []).map((p, i) => {
    const f = info(p);
    const sub = !f ? "Save to add it to your library." : !f.exists ? `<span class="bad-text">Folder not found</span>` : `${f.clip_count} clip${f.clip_count === 1 ? "" : "s"}`;
    return row(`<span class="path-text">${esc(p)}</span>`, sub, `<button type="button" class="icon-btn" data-folder-del="${i}" title="Stop showing this folder"><svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg></button>`);
  });
  rows.push(row("Add a folder", "Another app's clips folder, an old drive… Its sub-folders show up as games.", `<button type="button" class="btn" id="folder-add">Add folder</button>`));
  return group("Also show clips from", rows, "Clipit only saves into your clips folder. These are just shown in your library.");
}

function wireFolders() {
  const nf = $("#set-folder-new");
  if (nf) nf.onclick = async () => { if (await newFolder()) rerenderSettings(); };
  const add = $("#folder-add");
  if (add) add.onclick = async () => {
    try {
      const p = await invoke("pick_folder", { title: "Choose a folder with clips" });
      if (!p) return;
      const all = [setUI.draft.clips_root, ...(setUI.draft.extra_folders || [])].map((x) => (x || "").toLowerCase());
      if (all.includes(p.toLowerCase())) return toast("That folder is already in your library");
      onDraftChange("extra_folders", [...(setUI.draft.extra_folders || []), p]);
      rerenderSettings();
    } catch (e) { toast(String(e), true); }
  };
  main.querySelectorAll("[data-folder-del]").forEach((b) => (b.onclick = () => {
    onDraftChange("extra_folders", setUI.draft.extra_folders.filter((_, i) => i !== Number(b.dataset.folderDel)));
    rerenderSettings();
  }));
}

// ---------- updates ----------

const upd = { checking: false, checked: false, available: null, installing: false, progress: null, error: null };

// ---------- what's new ----------
// Shown once, the first time Clipit opens after an update: sections of
// [title, one line]. Keep it short and plain.
const WHATS_NEW = {
  "0.15.0": [
    ["New", [
      ["Clipit's own recording engine", "Clips are now recorded by Clipit itself on your graphics card, instead of a separate ffmpeg program."],
    ]],
    ["Faster and more reliable", [
      ["Instant saves", "Clips are written straight from memory: about three times faster than before."],
      ["Never an old clip", "The buffer always reaches the moment you press, even in menus. If capture gets stuck, it restarts on its own and keeps your replay."],
      ["Lighter", "No ffmpeg running while you play: less memory and fewer moving parts."],
    ]],
    ["Good to know", [
      ["Classic recorder", "Settings → Recording → Recorder switches back if you ever need it. Long recordings still use it for now."],
    ]],
  ],
  "0.13.0": [
    ["New", [
      ["Folders", "Press + next to Folders to make your own (\u201cFunny moments\u201d, \u201cClutches\u201d\u2026). Rename or delete them from the folder's page."],
      ["Select several clips", "Tick clips (or Ctrl/Shift+click) to favorite, move, copy, compress or delete them all at once."],
      ["Space saver", "VALORANT clips without kills are made smaller while you're away from the PC. You get a notice with what was cleared, and can undo it. Settings \u2192 Space saver."],
      ["Back button", "Top left, or your mouse's back button, or Alt+Left."],
      ["Tray menu", "Right-click the tray icon: save a clip, record, open your clips or settings, and see Clipit's CPU and RAM."],
      ["No repeated footage", "Clip twice in a row and the second clip starts where the first one ended."],
    ]],
    ["Faster", [
      ["Kill marks show up instantly", "Kills are noted the moment they happen while you play, so new clips have their marks right away. Older clips get theirs while your PC is idle."],
      ["Clips end where you pressed", "If your PC records a bit behind the game, Clipit waits a moment for it, so you never have to press twice."],
      ["ffmpeg installs by itself", "First-time setup gets ffmpeg in the background and restarts Clipit when it's ready."],
    ]],
    ["Fixed", [
      ["Alt+Z with the window open", "No more Settings inside the quick view: Alt+Z brings the window forward."],
    ]],
    ["Feels better", [
      ["Animations", "Buttons, cards and pages move a little. Turn it off in Settings \u2192 Appearance \u2192 Reduce motion."],
    ]],
  ],
};

function maybeShowWhatsNew() {
  const c = state.config;
  if (!c || !APP_VERSION || c.seen_version === APP_VERSION) return;
  const items = WHATS_NEW[APP_VERSION];
  // A fresh install (setup not done) just remembers the version.
  if (c.setup_done && items) {
    $("#wn-version").textContent = `Clipit ${APP_VERSION}`;
    $("#wn-list").innerHTML = items.map(([section, list]) => `<li class="wn-section">${esc(section)}</li>${list.map(([t, d]) => `<li><b>${esc(t)}</b><span>${esc(d)}</span></li>`).join("")}`).join("");
    $("#whats-new").classList.remove("hidden", "closing");
  }
  patchConfig({ seen_version: APP_VERSION });
}
function closeWhatsNew() {
  const m = $("#whats-new");
  if (m.classList.contains("hidden")) return;
  m.classList.add("closing");
  setTimeout(() => m.classList.add("hidden"), document.body.classList.contains("reduce-motion") ? 0 : 220);
}
$("#wn-ok").onclick = closeWhatsNew;
$("#whats-new").addEventListener("click", (e) => { if (e.target.id === "whats-new") closeWhatsNew(); });

// ---------- space saver (compression) ----------

const comp = { entries: [], busy: null, queued: 0, picked: new Set() };

async function loadCompress() {
  try { Object.assign(comp, await invoke("compress_state")); } catch {}
}

function compressNotice() {
  const unseen = comp.entries.filter((e) => !e.seen);
  const box = $("#compress-notice");
  if (!unseen.length || state.view.name === "compressed") { box.classList.add("hidden"); return; }
  const saved = unseen.reduce((s, e) => s + (e.original - e.size), 0);
  $("#cn-title").textContent = `Cleared ${fmtSize(saved)}`;
  $("#cn-sub").textContent = `${unseen.length} VALORANT clip${unseen.length === 1 ? "" : "s"} without kills ${unseen.length === 1 ? "was" : "were"} made smaller.`;
  box.classList.remove("hidden");
}
$("#cn-close").onclick = () => { $("#compress-notice").classList.add("hidden"); invoke("compress_seen").catch(() => {}); comp.entries.forEach((e) => (e.seen = true)); };
$("#cn-see").onclick = () => { $("#compress-notice").classList.add("hidden"); go({ name: "compressed" }); };

listen("compress-done", async (e) => {
  const p = e.payload || {};
  if (!p.count && p.failed && p.failed.length) toast("Couldn't compress: " + p.failed.join("; "), true);
  await loadCompress();
  compressNotice();
  if (p.count) refreshLibrary();
  if (state.view.name === "compressed") renderCompressed();
});
listen("compress-progress", (e) => {
  Object.assign(comp, { busy: e.payload.busy, queued: e.payload.queued });
  const s = $("#comp-busy");
  if (s) s.textContent = comp.busy || "";
});

function renderCompressed() {
  const list = [...comp.entries].sort((a, b) => b.at_ms - a.at_ms);
  const saved = list.reduce((s, e) => s + (e.original - e.size), 0);
  const byPath = new Map((state.lib ? state.lib.clips : []).map((c) => [c.path.toLowerCase(), c]));
  const bg = (state.config && state.config.background) || {};
  const day = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
  main.innerHTML = `
    <div class="page compressed-page">
      <h1>Compressed clips</h1>
      <p class="lede">${list.length ? `${list.length} clip${list.length === 1 ? "" : "s"}, ${fmtSize(saved)} smaller in total.` : "Nothing compressed yet."} ${bg.compress ? "VALORANT clips without kills are made smaller while you're away." : "Automatic compression is off."} <button class="link" id="comp-settings">Settings</button></p>
      <div class="comp-tools">
        <button class="btn sm ghost" id="comp-all">${comp.picked.size && comp.picked.size === list.length ? "Select none" : "Select all"}</button>
        <button class="btn sm" id="comp-undo" ${comp.picked.size ? "" : "disabled"}>Undo compression (${comp.picked.size})</button>
        <span class="faint" id="comp-busy">${esc(comp.busy || "")}</span>
      </div>
      <div class="list comp-list">
        ${list.map((e) => {
          const c = byPath.get(e.path.toLowerCase());
          const name = e.path.split(/[\\/]/).pop();
          const keptTill = e.backup ? `Original kept until ${day.format(new Date(e.at_ms + (bg.keep_days || 0) * 86400000))}` : "Original no longer kept";
          return `<label class="row comp-row${e.backup ? "" : " no-undo"}">
            <input type="checkbox" data-comp="${esc(e.path)}" ${comp.picked.has(e.path) ? "checked" : ""} ${e.backup ? "" : "disabled"}>
            <div class="row-text"><div class="row-label">${esc(c ? c.title || name : name)}</div><div class="row-sub">${c ? esc(c.game) + " · " : ""}${keptTill}</div></div>
            <div class="comp-size"><s>${fmtSize(e.original)}</s> \u2192 <b>${fmtSize(e.size)}</b></div>
          </label>`;
        }).join("")}
      </div>
    </div>`;
  invoke("compress_seen").catch(() => {});
  comp.entries.forEach((e) => (e.seen = true));
  $("#compress-notice").classList.add("hidden");
  $("#comp-settings").onclick = () => { setUI.tab = "background"; go({ name: "settings" }); };
  $("#comp-all").onclick = () => {
    const undoable = list.filter((e) => e.backup).map((e) => e.path);
    if (comp.picked.size === undoable.length) comp.picked.clear(); else undoable.forEach((p) => comp.picked.add(p));
    renderCompressed();
  };
  main.querySelectorAll("[data-comp]").forEach((b) => (b.onchange = () => {
    if (b.checked) comp.picked.add(b.dataset.comp); else comp.picked.delete(b.dataset.comp);
    $("#comp-undo").disabled = !comp.picked.size;
    $("#comp-undo").textContent = `Undo compression (${comp.picked.size})`;
  }));
  $("#comp-undo").onclick = async () => {
    const paths = [...comp.picked];
    try {
      const n = await invoke("compress_revert", { paths });
      comp.picked.clear();
      await loadCompress();
      await refreshLibrary();
      renderCompressed();
      toast(`Put back the original of ${n} clip${n === 1 ? "" : "s"}`);
    } catch (e) { toast(String(e), true); }
  };
}

function backgroundPane(d) {
  const b = d.background || {};
  const count = comp.entries.length;
  const saved = comp.entries.reduce((s, e) => s + (e.original - e.size), 0);
  return `<h2>Space saver</h2><p class="lede">Work Clipit does only while you're away from the PC and no game is running. It stops the moment you come back or start a game.</p>
    ${group("Kill marks for older clips", [
      row("Find kills in older clips", "Reads VALORANT clips saved before kill marks existed, so their timeline shows kills instantly too. New clips get them while you play.", toggle("background.index_kills")),
    ])}
    ${group("Compress clips without kills", [
      row("Compress automatically", "VALORANT clips with no kills of yours are re-encoded smaller. Favorites and edited clips are left alone.", toggle("background.compress")),
      row("How small", "", segB("background.quality", [["light", "Light · ~40% smaller"], ["balanced", "Balanced · ~60%"], ["small", "Small · ~75%"]])),
      row("Only clips older than", "Gives you time to watch and favorite new clips first.", selectB("background.min_age_hours", [[1, "1 hour"], [24, "1 day"], [72, "3 days"], [168, "1 week"], [720, "30 days"]], true)),
      row("Keep the originals", "So you can undo it. They sit in a hidden folder on the same drive and are deleted after this.", selectB("background.keep_days", [[0, "Don't keep"], [1, "1 day"], [3, "3 days"], [7, "7 days"], [30, "30 days"]], true)),
      row("Skip favorites", "", toggle("background.skip_favorites")),
      row("Wait until I'm away for", "No mouse or keyboard for this long.", selectB("background.away_minutes", [[0, "Don't wait (only no game)"], [2, "2 minutes"], [5, "5 minutes"], [15, "15 minutes"], [30, "30 minutes"]], true)),
      row("Notify me", "A Windows notification saying how much space was cleared.", toggle("background.notify")),
    ], "Compression keeps the resolution, frame rate and all audio tracks; the date stays the same, so clips don't move around in your library.")}
    ${group("", [row("Compressed clips", count ? `${count} clip${count === 1 ? "" : "s"}, ${fmtSize(saved)} smaller` : "None yet.", `<button type="button" class="btn" id="set-comp-open">See and undo</button>`)])}`;
}

async function checkForUpdate(quiet) {
  if (upd.checking || upd.installing) return;
  upd.checking = true;
  upd.error = null;
  refreshUpdateUI();
  try {
    upd.available = await invoke("check_update");
    upd.checked = true;
  } catch (e) {
    upd.error = String(e);
    if (!quiet) toast("Couldn't check for updates: " + e, true);
  }
  upd.checking = false;
  refreshUpdateUI();
}

async function installUpdate() {
  if (!upd.available || upd.installing) return;
  upd.installing = true;
  upd.progress = null;
  refreshUpdateUI();
  try {
    await invoke("install_update"); // Clipit restarts when this works
  } catch (e) {
    upd.installing = false;
    toast("Update failed: " + e, true);
    refreshUpdateUI();
  }
}

listen("update-progress", (e) => {
  const { done, total } = e.payload || {};
  upd.progress = total ? done / total : null;
  refreshUpdateUI();
});

function refreshUpdateUI() {
  const card = $("#update-card");
  if (card) {
    card.classList.toggle("hidden", !upd.available);
    if (upd.available) {
      $("#update-text").textContent = upd.installing ? (upd.progress != null ? `Downloading ${Math.round(upd.progress * 100)}%` : "Installing…") : `Clipit ${upd.available.version} is ready`;
      $("#update-btn").disabled = upd.installing;
    }
  }
  if (state.view.name === "settings" && setUI.tab === "about") rerenderSettings();
}

function renderSettings() {
  startSettingsDraft();
  if (!setUI.draft) { main.innerHTML = ""; return; }
  main.innerHTML = `
    <div class="settings-shell">
      <nav class="set-tabs" aria-label="Settings sections">
        <h1>Settings</h1>
        ${SET_TABS.map(([id, label, color]) => `<button type="button" class="set-tab${id === setUI.tab ? " on" : ""}" data-tab="${id}"><span class="tile" style="--t:${color}">${ICON[id]}</span>${label}</button>`).join("")}
      </nav>
      <div class="set-pane" id="set-pane">
        ${settingsPane(setUI.tab)}
        <div class="save-bar${settingsDirty() ? " on" : ""}" id="save-bar"><span>Unsaved changes</span><button type="button" class="btn ghost" id="set-discard">Discard</button><button type="button" class="btn primary" id="set-save">Save</button></div>
      </div>
    </div>`;
  wireSettings();
}

function rerenderSettings() {
  if (state.view.name !== "settings") return;
  const top = main.scrollTop;
  renderSettings();
  main.scrollTop = top;
}

function updateSaveBar() {
  const bar = $("#save-bar");
  if (bar) bar.classList.toggle("on", settingsDirty());
}

// Changes that add or remove rows rebuild the pane.
const REBUILD = /^(capture\.handcam\.(enabled|device|shape|aspect|border|flip_h|flip_v|rotate)|window_mode|animation|theme_mode|capture\.(enabled|engine|target|games_mode|codec|desktop_audio|mic|replay_secs|auto_clip_kills|auto_max_mb|auto_min_kills|auto_max_gap_secs|auto_whole_round_from|kill_before_secs|kill_after_secs|auto_skip_deathmatch|quality|fps|resolution))$/;

function onDraftChange(path, value) {
  setPath(setUI.draft, path, value);
  if (path === "capture.handcam.device" || path === "capture.handcam.enabled") { hcState.imgDevice = null; hcState.imgError = null; hcState.img = null; }
  if (path === "theme_mode" || path === "theme") applyTheme(setUI.draft.theme_mode, setUI.draft.theme);
  if (path === "reduce_motion") document.body.classList.toggle("reduce-motion", !!value);
  if (REBUILD.test(path) || path === "theme") rerenderSettings();
  else {
    updateSaveBar();
    const hint = $("#ram-hint");
    if (hint) hint.textContent = ramText(setUI.draft.capture);
  }
}

function wireSettings() {
  main.querySelectorAll(".set-tab").forEach((b) => (b.onclick = () => { setUI.tab = b.dataset.tab; renderSettings(); main.scrollTop = 0; animatePage($("#set-pane")); $("#set-pane").classList.add("enter"); }));
  main.querySelectorAll("[data-bind]").forEach((el) => {
    const path = el.dataset.bind;
    const read = () => (el.type === "checkbox" ? el.checked : el.dataset.num ? Number(el.value) : el.value);
    if (el.type === "range") {
      const show = () => { el.style.setProperty("--v", (el.value - el.min) / (el.max - el.min) * 100 + "%"); el.nextElementSibling.textContent = el.value + "%"; };
      show();
      el.addEventListener("input", () => { show(); onDraftChange(path, read()); });
    } else if (el.tagName === "INPUT" && el.type !== "checkbox") {
      el.addEventListener("input", () => onDraftChange(path, read()));
    } else {
      el.addEventListener("change", () => onDraftChange(path, read()));
    }
  });
  main.querySelectorAll("[data-seg-bind]").forEach((g) => g.querySelectorAll("button").forEach((b) => (b.onclick = () => {
    onDraftChange(g.dataset.segBind, g.dataset.num ? Number(b.dataset.v) : b.dataset.v);
  })));
  main.querySelectorAll("[data-theme-pick]").forEach((b) => (b.onclick = () => onDraftChange("theme", b.dataset.themePick)));
  main.querySelectorAll("[data-game-del]").forEach((b) => (b.onclick = () => {
    const games = setUI.draft.capture.games.filter((_, i) => i !== Number(b.dataset.gameDel));
    setUI.draft.capture.games = games;
    rerenderSettings();
  }));
  main.querySelectorAll("[data-browse]").forEach((b) => (b.onclick = async () => {
    try {
      const p = await invoke("pick_folder", { title: b.dataset.browse === "clips_root" ? "Choose your clips folder" : "Choose where Clipit saves exports" });
      if (p) { onDraftChange(b.dataset.browse, p); rerenderSettings(); }
    } catch (e) { toast(String(e), true); }
  }));
  wireKeyButtons(main, (name, value) => {
    onDraftChange(name, name === "hotkey" ? value || "Alt+Z" : value);
    rerenderSettings();
  });
  if ($("#set-wizard")) $("#set-wizard").onclick = async () => { if (await confirmLeaveSettings()) { dropSettingsDraft(); openWizard(); } };
  if ($("#set-restart-rec")) $("#set-restart-rec").onclick = async (e) => {
    const b = e.currentTarget;
    b.disabled = true;
    try {
      await invoke("restart_capture");
      toast("Restarting the recorder…");
    } catch (err) { toast(String(err), true); }
    setTimeout(() => { b.disabled = false; pollCapture(); }, 3000);
  };
  if ($("#set-refresh-art")) $("#set-refresh-art").onclick = async () => {
    await invoke("refresh_art");
    state.art = {};
    artErrorShown = false;
    await loadLibrary();
    renderSidebar();
    toast("Refreshing artwork");
  };
  wireFfmpegButtons(main);
  if (setUI.tab === "handcam") wireHandcam();
  if (setUI.tab === "storage") wireFolders();
  if ($("#set-comp-open")) $("#set-comp-open").onclick = () => go({ name: "compressed" });
  if ($("#upd-check")) $("#upd-check").onclick = () => checkForUpdate(false);
  if ($("#upd-install")) $("#upd-install").onclick = installUpdate;
  $("#set-save").onclick = saveSettingsDraft;
  $("#set-discard").onclick = () => {
    setUI.draft = clone(setUI.base);
    applySavedTheme();
    rerenderSettings();
  };
}

async function saveSettingsDraft() {
  const { autostart, ...draft } = setUI.draft;
  const { autostart: wasAuto, ...base } = setUI.base;
  const patch = diffConfig(base, draft);
  if (autostart !== wasAuto) {
    try {
      const on = await invoke("set_autostart", { enabled: autostart });
      if (state.status) state.status.autostart = on;
    } catch (e) { toast("Couldn't change the startup setting: " + e, true); return; }
  }
  if (Object.keys(patch).length && !(await patchConfig(patch))) return;
  if (patch.steamgriddb_key !== undefined) {
    for (const k of Object.keys(state.art)) {
      const a = state.art[k];
      if (!a.grid && !a.hero && !a.logo && !a.icon) delete state.art[k];
    }
    artErrorShown = false;
  }
  if (patch.clips_root !== undefined || patch.extra_folders !== undefined || patch.steamgriddb_key !== undefined) {
    await loadLibrary();
    renderSidebar();
  }
  setUI.draft = null;
  startSettingsDraft();
  applySavedTheme();
  updateRecWidget();
  rerenderSettings();
  const failed = [state.status.hotkey, state.status.clip_hotkey, state.status.record_hotkey].find((h) => h && h.hotkey && !h.registered);
  toast(failed ? `${fmtKey(failed.hotkey)} is taken by another app` : "Settings saved", !!failed);
}

// Live bits (recorder status) without rebuilding the page.
function refreshSettingsBits() {
  if (state.view.name !== "settings") return;
  const box = $("#cap-status");
  if (box && state.capStatus) {
    box.outerHTML = capStatusRow(state.capStatus);
    wireFfmpegButtons(main);
  }
  const hcs = $("#hc-status");
  if (hcs && setUI.draft) hcs.innerHTML = hcStatusText(setUI.draft.capture.handcam);
  const sum = $("#auto-summary");
  if (sum && setUI.draft) sum.textContent = autoSummary(setUI.draft.capture);
}

async function openSettings() {
  await Promise.all([loadCaptureInfo(), loadStatus()]);
  if (state.view.name === "settings") rerenderSettings();
}

// ---------- recorder widget + events ----------

function fmtDur(secs) {
  secs = Math.max(0, Math.floor(secs));
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

function updateRecWidget() {
  const st = state.capStatus;
  const w = $("#rec-widget");
  if (!w) return;
  const c = (state.config && state.config.capture) || {};
  const rec = st && st.recording;
  const ready = !!st && st.state === "buffering" && !rec;
  const error = !!st && (st.state === "error" || st.state === "no-ffmpeg");
  w.className = "recorder" + (rec ? " recording" : error ? " error" : ready ? "" : " idle");
  const label = rec ? `Recording ${fmtDur(rec.secs)}` : !st ? "Starting" : ({
    buffering: "Replay ready", waiting: "Waiting for a game", off: "Replay off", starting: "Starting", error: "Capture stopped", "no-ffmpeg": state.status && state.status.ffmpeg ? "Update ffmpeg" : "ffmpeg missing",
  }[st.state] || st.message);
  $("#rec-text").textContent = label;
  $("#rec-text").title = st ? st.message : "";
  $("#rec-src").textContent = rec ? "" : st && st.state === "buffering" ? st.source : "";
  $("#rec-src").title = $("#rec-src").textContent;
  const secs = st ? Math.min(st.buffered_secs || 0, c.replay_secs || 60) : 0;
  const frac = rec ? 1 : ready ? secs / (c.replay_secs || 60) : 0;
  $("#rec-meter-fill").style.transform = `scaleX(${frac.toFixed(3)})`;
  $("#rec-meter").title = ready ? `${Math.round(secs)} of ${c.replay_secs} s in the buffer` : "";
  const len = LENGTHS.find((l) => l[0] === c.replay_secs);
  $("#rec-save-label").textContent = len ? `Save ${len[1]}` : "Save clip";
  $("#rec-save").disabled = !ready && !rec;
  $("#rec-save").title = c.clip_hotkey ? `Save the last ${len ? len[1] : "moments"} (${fmtKey(c.clip_hotkey)})` : "Save the last moments as a clip";
  $("#rec-toggle-label").textContent = rec ? "Stop" : "Record";
  $("#rec-toggle").title = c.record_hotkey ? `Start or stop recording (${fmtKey(c.record_hotkey)})` : "Start or stop a recording";
}

$("#update-btn").onclick = installUpdate;
$("#rec-save").onclick = () => {
  toast("Saving clip…"); // "Clip saved" follows in a moment
  invoke("save_replay").catch((e) => toast(String(e), true));
};
$("#rec-toggle").onclick = async () => {
  $("#rec-toggle").disabled = true;
  try { await invoke("toggle_recording"); } catch (e) { toast(String(e), true); }
  $("#rec-toggle").disabled = false;
  pollCapture();
};

async function pollCapture() {
  try { state.capStatus = await invoke("capture_status"); } catch { return; }
  updateRecWidget();
  refreshSettingsBits();
}
// Live numbers (buffer, REC timer) only while Clipit is on screen.
setInterval(() => { if (overlayVisible()) pollCapture(); }, 1000);

listen("capture-status", (e) => { state.capStatus = e.payload; updateRecWidget(); refreshSettingsBits(); });
listen("capture-error", (e) => toast(String(e.payload), true));
listen("recording", (e) => {
  const p = e.payload || {};
  if (p.active) toast("Recording started");
  else if (p.path) toast(`Recording saved · ${fmtSize(p.size || 0)}`);
  pollCapture();
});
let lastSavedClip = null;
listen("clip-saved", (e) => {
  lastSavedClip = e.payload.path;
  if (e.payload.kill_times) setTimeout(() => setInfo(e.payload.path, { kills: e.payload.kills, kill_times: e.payload.kill_times, auto: e.payload.auto }), 1500);
  const k = e.payload.kills;
  toast(k ? `Auto clipped ${k === 5 ? "an ace" : "a " + multiText(k)} · ${fmtSize(e.payload.size || 0)}` : `Clip saved · ${fmtSize(e.payload.size || 0)}`);
});
// The hand cam was added to a clip saved a moment ago: show the new version.
listen("clip-updated", (e) => {
  const { path, size, quiet } = e.payload;
  findClips(path).forEach((c) => { c.size = size; c.modified_ms = Date.now(); });
  state.thumbs.delete(path);
  const cur = state.player && state.player.list[state.player.index];
  if (cur && cur.path === path && !heldClip) {
    const t = video.currentTime;
    const playing = !video.paused;
    loadClipVideo(cur, t, playing);
  }
  if (state.view.name !== "settings" && state.view.name !== "compressed") rerenderKeepScroll();
  if (!quiet) toast("Hand cam added");
});
// Read from the saved file a few seconds after a VALORANT clip is saved.
listen("clip-info", (e) => {
  const { path, info, quiet } = e.payload;
  // Read in the background while you were away: just keep the data.
  if (quiet) {
    findClips(path).forEach((c) => { if (info.kills != null) c.kills = info.kills; if (info.kill_times) c.kill_times = info.kill_times; });
    if (curClip() && curClip().path === path) { renderPlayerHeader(); renderKillMarks(); }
    return;
  }
  setInfo(path, info);
  const bits = [info.map, info.agent, info.kills != null ? killText(info.kills) : null].filter(Boolean);
  if (bits.length) toast(bits.join(" · "));
});
listen("games-changed", async () => {
  state.config = await invoke("get_config");
  if (setUI.draft) {
    // Keep the draft's own edits; take the newly found games.
    const untouched = JSON.stringify(setUI.draft.capture.games) === JSON.stringify(setUI.base.capture.games);
    setUI.base.capture.games = clone(state.config.capture.games);
    if (untouched) setUI.draft.capture.games = clone(state.config.capture.games);
  }
  rerenderSettings();
});

// ---------- first-run setup ----------

const QUALITY_PRESETS = [
  ["1080-60", "1080p · 60 fps", "Smooth and Discord-friendly (recommended)"],
  ["1440-60", "1440p · 60 fps", "Sharpest, bigger files"],
  ["1080-120", "1080p · 120 fps", "Silky slow motion"],
  ["1080-30", "1080p · 30 fps", "Smallest files"],
];

const wiz = { step: 0, d: null };

async function openWizard() {
  const cfg = state.config || {};
  const c = cfg.capture || {};
  wiz.step = 0;
  wiz.d = {
    record: c.enabled !== false,
    clip_hotkey: c.clip_hotkey || "",
    record_hotkey: c.record_hotkey || "",
    hotkey: cfg.hotkey || "Alt+Z",
    desktop_audio: c.desktop_audio !== false,
    desktop_device: c.desktop_device || "",
    mic: c.mic !== false,
    mic_device: c.mic_device || "",
    separate_tracks: c.separate_tracks !== false,
    target: c.target || "game",
    monitor: c.monitor || "",
    replay_secs: c.replay_secs || 60,
    preset: `${c.resolution || "1080"}-${c.fps || 60}`,
    clips_root: cfg.clips_root || "",
    theme_mode: cfg.theme_mode || "dark",
    theme: cfg.theme || "tally",
  };
  $("#wizard").classList.remove("hidden");
  await loadCaptureInfo();
  renderWizard();
}

function wizSteps() {
  return wiz.d.record ? ["look", "keys", "audio", "capture", "done"] : ["look", "keys", "capture", "done"];
}

function renderWizard() {
  const steps = wizSteps();
  const step = steps[wiz.step];
  const d = wiz.d;
  const info = state.capInfo || { devices: { outputs: [], inputs: [] }, monitors: [] };
  $("#wiz-dots").innerHTML = steps.map((_, i) => `<span class="${i <= wiz.step ? "on" : ""}"></span>`).join("");
  const body = $("#wiz-body");
  const choice = (key, value, title, text) => `<button type="button" class="choice${d[key] === value ? " on" : ""}" data-k="${key}" data-v="${esc(JSON.stringify(value))}"><b>${title}</b><span>${text}</span></button>`;
  if (step === "look") {
    body.innerHTML = `
      <h2 id="wiz-title">Welcome to Clipit</h2>
      <p>Save the last minute of any game with one key, then trim and share it. First, pick how it looks.</p>
      <div class="row-between"><span>Mode</span>${seg("w_mode", [["dark", "Dark"], ["light", "Light"], ["system", "Match Windows"]], d.theme_mode)}</div>
      <div style="margin-top:12px">${themeCards(d.theme, "data-wtheme")}</div>
      ${state.status && !state.status.ffmpeg_capture ? `<div class="list" style="margin-top:16px">${ffmpegFixRow()}</div>` : ""}`;
    wireFfmpegButtons(body);
    // Setup needs ffmpeg: start getting it right away, in the background.
    if (state.status && !state.status.ffmpeg_capture && !ffi.phase) startFfmpegInstall();
  } else if (step === "keys") {
    body.innerHTML = `
      <h2 id="wiz-title">Pick your hotkeys</h2>
      <p>Click a key, then press a key (like L or /) or a combination. They work everywhere, even in-game.</p>
      ${d.record ? `
        <div class="key-row"><div class="what"><b>Save clip</b><span>Keeps the last moments</span></div>${keyButton("clip_hotkey", d.clip_hotkey)}</div>
        <div class="key-row"><div class="what"><b>Start or stop recording</b><span>Optional</span></div>${keyButton("record_hotkey", d.record_hotkey)}</div>` : ""}
      <div class="key-row"><div class="what"><b>Open quick view</b><span>Clipit over whatever you're doing</span></div>${keyButton("hotkey", d.hotkey)}</div>
      ${d.record ? `<p class="hint">If another app already uses a combination you pick, turn it off there first so it's free.</p>` : ""}
      <p class="hint">${SINGLE_NOTE}</p>`;
    wireKeyButtons(body, (name, value) => { d[name] = value; renderWizardNav(); });
  } else if (step === "audio") {
    body.innerHTML = `
      <h2 id="wiz-title">What should clips sound like?</h2>
      <p>Both use your Windows default device unless you pick one.</p>
      <label class="switch"><span><b>Desktop audio</b>: game, music, voice chat</span><input type="checkbox" data-w="desktop_audio" ${d.desktop_audio ? "checked" : ""}></label>
      <label class="field-col" style="margin:8px 0 16px">Device<select class="select" data-w="desktop_device" ${d.desktop_audio ? "" : "disabled"}>${deviceOptions(info.devices.outputs, d.desktop_device)}</select></label>
      <label class="switch"><span><b>Microphone</b></span><input type="checkbox" data-w="mic" ${d.mic ? "checked" : ""}></label>
      <label class="field-col" style="margin:8px 0 16px">Device<select class="select" data-w="mic_device" ${d.mic ? "" : "disabled"}>${deviceOptions(info.devices.inputs, d.mic_device)}</select></label>
      <label class="switch"><span>Save desktop and mic as separate tracks too (for video editing)</span><input type="checkbox" data-w="separate_tracks" ${d.separate_tracks ? "checked" : ""}></label>`;
  } else if (step === "capture") {
    body.innerHTML = `
      <h2 id="wiz-title">Recording</h2>
      <label class="switch"><span><b>Record clips with Clipit</b><br><span class="muted" style="font-size:12px">Off: Clipit only shows clips that are already in your folder.</span></span><input type="checkbox" data-w="record" ${d.record ? "checked" : ""}></label>
      ${d.record ? `
        <div class="choices" style="margin-top:16px">
          ${choice("target", "game", "Game window only", "Just the game. Nothing runs until a game is in focus.")}
          ${choice("target", "monitor", "Whole monitor", "Everything on one screen.")}
        </div>
        ${d.target === "monitor" ? `<label class="field-col" style="margin-top:12px">Monitor<select class="select" data-w="monitor">${info.monitors.map((m) => `<option value="${esc(m.id)}"${m.id === d.monitor || (!d.monitor && m.primary) ? " selected" : ""}>${esc(m.label)}</option>`).join("")}</select></label>` : ""}
        <div class="row-between" style="margin-top:16px"><span>Replay length</span>${seg("w_replay", [[30, "30 s"], [60, "1 min"], [120, "2 min"], [300, "5 min"]], d.replay_secs)}</div>
        <label class="field-col">Quality<select class="select" data-w="preset">${QUALITY_PRESETS.map(([v, l, t]) => `<option value="${v}"${v === d.preset ? " selected" : ""}>${l}, ${t}</option>`).join("")}</select></label>
        <p class="hint" id="wiz-ram"></p>` : ""}
      <label class="field-col" style="margin-top:16px">Clips folder
        <span class="field"><input type="text" data-w="clips_root" value="${esc(d.clips_root)}" spellcheck="false"><button type="button" class="btn" id="wiz-browse">Browse</button></span>
      </label>
      <p class="hint">Each game gets its own sub-folder.</p>`;
    $("#wiz-browse").onclick = async () => {
      try {
        const p = await invoke("pick_folder", { title: "Choose your clips folder" });
        if (p) { d.clips_root = p; renderWizard(); }
      } catch (e) { toast(String(e), true); }
    };
  } else {
    const keys = [d.clip_hotkey && d.record ? `<kbd>${esc(fmtKey(d.clip_hotkey))}</kbd> saves a clip` : "", d.record_hotkey && d.record ? `<kbd>${esc(fmtKey(d.record_hotkey))}</kbd> starts or stops recording` : "", `<kbd>${esc(fmtKey(d.hotkey))}</kbd> opens the quick view`].filter(Boolean);
    body.innerHTML = `
      <h2 id="wiz-title">You're set</h2>
      <p>${d.record ? "Clipit keeps the last moments ready whenever you play." : "Clipit will show the clips in your folder."}</p>
      <ul class="wiz-facts">${keys.map((k) => `<li>${k}</li>`).join("")}<li>Everything here can be changed later in Settings</li></ul>`;
  }
  body.querySelectorAll(".choice").forEach((b) => (b.onclick = () => { d[b.dataset.k] = JSON.parse(b.dataset.v); renderWizard(); }));
  body.querySelectorAll("[data-w]").forEach((el) => {
    const update = () => {
      d[el.dataset.w] = el.type === "checkbox" ? el.checked : el.value;
      if (el.dataset.w === "record") wiz.step = wizSteps().indexOf("capture");
      if (el.type === "checkbox") renderWizard();
      else { updateWizRam(); renderWizardNav(); }
    };
    el.addEventListener(el.type === "text" ? "input" : "change", update);
  });
  body.querySelectorAll("[data-seg]").forEach((g) => g.querySelectorAll("button").forEach((b) => (b.onclick = () => {
    if (g.dataset.seg === "w_mode") { d.theme_mode = b.dataset.v; applyTheme(d.theme_mode, d.theme); }
    else d.replay_secs = Number(b.dataset.v);
    renderWizard();
  })));
  body.querySelectorAll("[data-wtheme]").forEach((b) => (b.onclick = () => { d.theme = b.dataset.wtheme; applyTheme(d.theme_mode, d.theme); renderWizard(); }));
  updateWizRam();
  renderWizardNav();
}

function wizCaptureDraft() {
  const d = wiz.d;
  const [resolution, fps] = d.preset.split("-");
  return { resolution, fps: Number(fps), quality: "high", codec: "h264", replay_secs: d.replay_secs };
}

function updateWizRam() {
  const el = $("#wiz-ram");
  if (el) el.textContent = ramText(wizCaptureDraft());
}

function renderWizardNav() {
  const steps = wizSteps();
  const step = steps[wiz.step];
  $("#wiz-back").style.visibility = wiz.step === 0 ? "hidden" : "visible";
  const last = wiz.step === steps.length - 1;
  $("#wiz-next").textContent = last ? "Finish" : "Next";
  const needKey = step === "keys" && wiz.d.record && !wiz.d.clip_hotkey;
  const needFolder = step === "capture" && !wiz.d.clips_root.trim();
  $("#wiz-next").disabled = needKey || needFolder;
  $("#wiz-next").title = needKey ? "Pick a key for saving clips first" : needFolder ? "Choose a clips folder" : "";
}

$("#wiz-back").onclick = () => { if (wiz.step > 0) { wiz.step--; renderWizard(); } };
$("#wiz-next").onclick = async () => {
  if (keyCaptureActive) return;
  const steps = wizSteps();
  if (wiz.step < steps.length - 1) { wiz.step++; renderWizard(); return; }
  const d = wiz.d;
  const cap = d.record ? {
    enabled: true, clip_hotkey: d.clip_hotkey, record_hotkey: d.record_hotkey,
    desktop_audio: d.desktop_audio, desktop_device: d.desktop_device, mic: d.mic, mic_device: d.mic_device,
    separate_tracks: d.separate_tracks, target: d.target, monitor: d.monitor, ...wizCaptureDraft(),
  } : { enabled: false };
  const rootChanged = state.config && state.config.clips_root !== d.clips_root.trim();
  if (await patchConfig({ setup_done: true, hotkey: d.hotkey || "Alt+Z", theme_mode: d.theme_mode, theme: d.theme, clips_root: d.clips_root.trim(), capture: cap })) {
    $("#wizard").classList.add("hidden");
    applySavedTheme();
    const failed = [state.status.hotkey, state.status.clip_hotkey, state.status.record_hotkey].find((h) => h && h.hotkey && !h.registered);
    toast(failed ? `${fmtKey(failed.hotkey)} is taken by another app. Pick another in Settings.` : "All set", !!failed);
    pollCapture();
    if (rootChanged) { await loadLibrary(); renderSidebar(); render(); }
    if (state.view.name === "settings") { setUI.draft = null; rerenderSettings(); }
    if (ffi.phase === "done") restartForFfmpeg();
  }
};

// ---------- live updates ----------

function rerenderKeepScroll() {
  if (state.view.name === "settings") return;
  const top = main.scrollTop;
  const searchFocused = document.activeElement && document.activeElement.id === "search";
  if (["home", "all", "game", "favorites", "auto", "review"].includes(state.view.name)) {
    if ($("#clip-area") && state.view.name !== "home") {
      if (state.view.name === "game") renderHero();
      renderClipArea();
    } else {
      render();
    }
  }
  main.scrollTop = top;
  if (searchFocused && $("#search")) $("#search").focus();
}

let refreshTimer = null;
listen("library-changed", () => {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(async () => {
    const before = state.lib ? state.lib.clips.length : 0;
    await loadLibrary();
    renderSidebar();
    if (!state.lib.games.some((g) => g.name === state.view.game) && state.view.name === "game") go({ name: "home" });
    else rerenderKeepScroll();
    const added = state.lib.clips.length - before;
    if (added > 0) toast(`${added} new clip${added === 1 ? "" : "s"} added`);
  }, 1500);
});

// ---------- start ----------

// ---------- hidden in tray: free memory ----------

// ---------- overlay: show / hide animation ----------
//
// Alt+Z: Rust snapshots the screen, covers it with this window and sends
// "app-shown" with the snapshot. The page shows the snapshot sharp (so nothing
// visibly changes), then blurs/dims it while the panel springs in. Closing does
// the reverse and melts back into the live screen before the window hides.

const ov = { phase: "closed", gen: 0 };
const overlayVisible = () => ov.phase === "open" || ov.phase === "settled";

function setPhase(p) {
  document.body.classList.remove("ov-closed", "ov-open", "ov-settled", "ov-closing");
  document.body.classList.add("ov-" + p);
  ov.phase = p;
}

const isAppMode = () => document.body.classList.contains("mode-app");

async function loadImage(img, url) {
  try {
    img.src = url;
    await img.decode();
    return true;
  } catch {
    img.removeAttribute("src");
    return false;
  }
}

// payload: { mode: "quick" | "app", backdrop, blurred, animation: "blur" | "dim" | "none", gen }
async function openOverlay(payload) {
  payload = payload || {};
  const gen = ++ov.gen;
  const app = payload.mode === "app";
  const instant = app || payload.animation === "none";
  document.body.classList.toggle("mode-app", app);
  document.body.classList.toggle("anim-none", instant);
  setPhase("closed");
  let shot = false;
  let blur = false;
  if (!app && payload.backdrop && state.streamBase) {
    const n = "&n=" + payload.gen;
    [shot, blur] = await Promise.all([
      loadImage($("#bd-img"), fileUrl(payload.backdrop) + n),
      payload.blurred ? loadImage($("#bd-blur"), fileUrl(payload.blurred) + n) : Promise.resolve(false),
    ]);
  }
  if (gen !== ov.gen) return; // closed or re-opened meanwhile
  $("#backdrop").classList.toggle("no-shot", !shot);
  $("#backdrop").classList.toggle("no-blur", !blur);
  void document.body.offsetWidth; // start from the sharp, un-dimmed snapshot
  setPhase(instant ? "settled" : "open");
  if (!instant) setTimeout(() => { if (gen === ov.gen && ov.phase === "open") setPhase("settled"); }, 700);
  resumeSession();
  // The quick view is for your clips: it never opens on Settings (unless there
  // are unsaved changes there).
  if (!app && state.view.name === "settings" && !settingsDirty()) go({ name: "home" });
  // Opened from the tray on a page ("View clips", "Settings").
  if (payload.view && state.view.name !== payload.view) go({ name: payload.view });
}

// Fades the quick view out and hides it. The app window doesn't close this way.
function requestClose() {
  if (!overlayVisible() || isAppMode()) return;
  const gen = ++ov.gen;
  wasPlaying = !!(state.player && !video.paused && !video.ended);
  video.pause(); // silence right away, before the fade
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  const instant = document.body.classList.contains("anim-none");
  setPhase("closing");
  setTimeout(() => {
    if (gen !== ov.gen) return; // re-opened during the fade
    setPhase("closed"); // fully transparent, so a re-show never flashes the old frame
    requestAnimationFrame(() => requestAnimationFrame(() => {
      if (gen === ov.gen) invoke("hide_window");
    }));
  }, instant ? 0 : 300);
}

// Click anywhere outside the panel to close.
let downOnBackdrop = false;
$("#backdrop").addEventListener("pointerdown", (e) => { downOnBackdrop = e.button === 0; });
$("#backdrop").addEventListener("click", () => { if (downOnBackdrop) requestClose(); downOnBackdrop = false; });

// ---------- remembering where you left off ----------
//
// Saved when the overlay hides — also to disk, because after two minutes in the
// tray the whole page is closed to save memory and rebuilt on the next Alt+Z.

const SESSION_KEY = "clipit.session";
// Hidden longer than this: the next open starts on Home (with the newest clip)
// instead of the clip you were watching. An export that's still running is kept.
const IDLE_RESET_MS = 3 * 60 * 1000;
const isStale = (s) => !!(s && s.hiddenAt && Date.now() - s.hiddenAt >= IDLE_RESET_MS && !ed.job && !settingsDirty());
let wasPlaying = false;    // was a clip playing when the overlay closed
let session = null;        // snapshot taken at hide time
let playOnShow = false;    // resume playback once the overlay is visible again
let scrollToRedo = null;   // scroll position to re-apply once the overlay is visible

function captureSession() {
  const s = {
    view: state.view,
    search: state.search,
    sort: state.sort,
    reviewSummary: state.reviewSummary || null,
    scroll: main.scrollTop,
    anchor: null,
    hiddenAt: Date.now(),
  };
  // The first visible card + its offset: more exact than scrollTop once cards are rebuilt.
  const top = main.getBoundingClientRect().top;
  for (const card of main.querySelectorAll(".clip")) {
    const r = card.getBoundingClientRect();
    if (r.bottom > top + 4) {
      s.anchor = { path: card.querySelector(".thumb").dataset.path, offset: r.top - top };
      break;
    }
  }
  const p = state.player;
  if (p) {
    s.player = {
      paths: p.list.map((c) => c.path),
      index: p.index,
      time: video.currentTime || 0,
      playing: wasPlaying,
      review: p.review ? { kept: p.review.kept, deleted: p.review.deleted, skipped: p.review.skipped, freed: p.review.freed } : null,
      editor: ed.open ? { start: ed.start, end: ed.end, aspect: ed.aspect, crop: ed.crop } : null,
    };
  }
  return s;
}

function saveSession(s) {
  try { localStorage.setItem(SESSION_KEY, JSON.stringify(s)); } catch {}
}

function loadSavedSession() {
  try { return JSON.parse(localStorage.getItem(SESSION_KEY) || "null"); } catch { return null; }
}

function restoreScroll(s) {
  main.scrollTop = s.scroll || 0;
  if (!s.anchor) return;
  const fix = () => {
    const thumb = [...main.querySelectorAll(".thumb")].find((t) => t.dataset.path === s.anchor.path);
    if (!thumb) return;
    const card = thumb.closest(".clip");
    main.scrollTop += card.getBoundingClientRect().top - main.getBoundingClientRect().top - s.anchor.offset;
  };
  fix();
  requestAnimationFrame(fix); // again once the cards around it have their real size
}

// Rebuilds the page from a snapshot. `fresh` = the page was just (re)loaded.
function applySession(s, fresh) {
  if (s && fresh) {
    state.view = s.view || { name: "home" };
    state.search = s.search || "";
    state.sort = s.sort || "new";
    state.reviewSummary = s.reviewSummary || null;
  }
  render();
  if (s) {
    restoreScroll(s);
    scrollToRedo = s; // once more when shown: the window may have changed size
  }
  const sp = s && s.player;
  if (!sp) return;
  if (fresh) {
    // Re-open the player on the same clip list (minus clips deleted meanwhile).
    const byPath = new Map(state.lib.clips.map((c) => [c.path, c]));
    const current = sp.paths[sp.index];
    const list = sp.paths.map((p) => byPath.get(p)).filter(Boolean);
    if (!list.length) return;
    let index = list.findIndex((c) => c.path === current);
    const sameClip = index >= 0;
    if (!sameClip) index = Math.min(sp.index, list.length - 1);
    openPlayer(list, index, { time: sameClip ? sp.time : 0, play: false });
    if (sp.review) {
      state.player.review = { ...sp.review, history: [] };
      updateReviewBar();
    }
    if (sp.editor && sameClip) restoreEditor(sp.editor);
  } else if (state.player) {
    loadClipVideo(state.player.list[state.player.index], sp.time, false);
  }
  playOnShow = !!sp.playing;
}

function restoreEditor(e) {
  video.addEventListener("loadedmetadata", () => {
    setEditor(true);
    ed.start = e.start;
    ed.end = Math.min(e.end, video.duration || e.end);
    if (e.aspect) { ed.aspect = e.aspect; setAspectChip(e.aspect); ed.crop = e.crop || null; }
    renderTrim();
    renderEdTimes();
    layoutCrop();
    updateEstimate();
  }, { once: true });
}

// Called when the overlay opens: put back whatever was dropped at hide time.
function resumeSession() {
  if (isStale(session)) {
    session = null;
    scrollToRedo = null;
    playOnShow = false;
    if (state.player) closePlayer();
    state.view = { name: "home" };
    state.search = "";
    render();
    main.scrollTop = 0;
    return;
  }
  if (session) {
    const s = session;
    session = null;
    applySession(s, false);
  }
  if (scrollToRedo) {
    const s = scrollToRedo;
    scrollToRedo = null;
    requestAnimationFrame(() => restoreScroll(s));
  }
  if (playOnShow && state.player) video.play().catch(() => {});
  playOnShow = false;
}

listen("app-shown", (e) => openOverlay(e.payload));
listen("overlay-close", () => requestClose());
listen("app-hidden", () => {
  if (ov.phase !== "closed") { // hidden without our fade (app window closed, fallback path)
    wasPlaying = !!(state.player && !video.paused);
    ov.gen++;
    setPhase("closed");
  }
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  if (session) return; // already hidden
  session = captureSession();
  saveSession(session);
  if (state.player) releaseVideo();
  releaseMainVideos();
  // Drop every card and thumbnail until we're shown again (memory).
  thumbObserver.disconnect();
  main.innerHTML = "";
});

(async function init() {
  try { state.streamBase = await invoke("stream_base"); } catch (e) { toast("Video server failed: " + e, true); }
  await loadStatus();
  applySavedTheme();
  await loadLibrary();
  renderSidebar();
  const saved = loadSavedSession();
  applySession(isStale(saved) ? null : saved, true);
  invoke("ui_ready").catch(() => {});
  pollCapture();
  if (state.config && !state.config.setup_done) openWizard();
  try { APP_VERSION = await invoke("app_version"); } catch {}
  maybeShowWhatsNew();
  loadCompress().then(compressNotice);
  updateBackBtn();
  // Look for a new version a little after start (installing always waits for you).
  if (state.config && state.config.auto_update !== false) setTimeout(() => checkForUpdate(true), 20000);
  runAutotest();
})();

// Debug builds only: scripted walkthrough used to measure memory.
async function runAutotest() {
  let mode = null;
  try { mode = await invoke("autotest"); } catch {}
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  if (mode === "play-many") {
    go({ name: "all" });
    await wait(1000);
    openPlayer(state.list, 0);
    for (let i = 0; i < 8; i++) { await wait(4000); step(1); }
    await wait(4000);
    closePlayer();
    await wait(5000);
    invoke("hide_window");
    return;
  }
  // A clip being worked on: spinner on its card, the player waits, then plays.
  if (mode === "busy") {
    const log = (m) => invoke("debug_log", { msg: m });
    const check = (name, ok) => log(`${ok ? "PASS" : "FAIL"} ${name}`);
    go({ name: "all" });
    await wait(1500);
    const c = state.list[0];
    setBusy(c.path, "handcam");
    await wait(200);
    const thumb = [...main.querySelectorAll(".thumb")].find((t) => t.dataset.path === c.path);
    check("card dims and shows 'Adding hand cam…'", thumb && thumb.classList.contains("busy") && /Adding hand cam/.test(thumb.textContent));
    openPlayer(state.list, 0);
    await wait(1500);
    check("player shows the waiting cover", !$("#p-busy").classList.contains("hidden"));
    check("video isn't loaded while held", !video.getAttribute("src"));
    setBusy(c.path, null);
    await wait(2500);
    check("cover hides when done", $("#p-busy").classList.contains("hidden"));
    check("video loads when done", !!video.getAttribute("src") && video.readyState >= 1);
    check("card spinner gone", !document.querySelector(".thumb.busy") && !document.querySelector(".busy-badge"));
    setBusy(c.path, "reading");
    await wait(200);
    check("reading: badge but still playable", !!document.querySelector(".busy-badge") && $("#p-busy").classList.contains("hidden"));
    setBusy(c.path, null);
    closePlayer();
    setUI.tab = "recording";
    go({ name: "settings" });
    await wait(1500);
    check("Restart recorder button in Recording", !!$("#set-restart-rec"));
    const before = Date.now();
    $("#set-restart-rec").click();
    let seen = "";
    while (Date.now() - before < 20000) {
      await wait(500);
      const st = await invoke("capture_status");
      if (st.message !== seen) { seen = st.message; log(`status: ${st.state} · ${st.message}`); }
      if (Date.now() - before > 3000 && st.state === "buffering" && st.buffered_secs > 1) break;
    }
    const st = await invoke("capture_status");
    check("recorder back up after restart", st.state === "buffering");
    log("DONE");
    return;
  }
  // Speed slider: 0.1 steps, eased ramp, and slow-motion audio levelling that really carries sound.
  if (mode === "speed") {
    const log = (m) => invoke("debug_log", { msg: m });
    const check = (name, ok, extra = "") => log(`${ok ? "PASS" : "FAIL"} ${name}${extra ? " · " + extra : ""}`);
    go({ name: "all" });
    await wait(1500);
    openPlayer(state.list, 0);
    await wait(2500);
    if (video.paused) await video.play().catch((e) => log("play failed: " + e));
    await wait(1000);
    const t1 = video.currentTime;
    await wait(1000);
    check("clip plays (cross-origin load works)", video.currentTime > t1 + 0.5 && !video.error, `t ${t1.toFixed(2)} → ${video.currentTime.toFixed(2)}`);
    vc.speed.click();
    await wait(100);
    check("speed button opens the slider", speedPopOpen());
    check("slider steps by 0.1", speedRange.step === "0.1" && speedRange.min === "0.1" && speedRange.max === "2");
    speedRange.value = "0.3";
    speedRange.dispatchEvent(new Event("input"));
    const samples = [];
    const t0 = performance.now();
    while (performance.now() - t0 < 1200) { samples.push(+video.playbackRate.toFixed(3)); await wait(60); }
    log("ramp 1 → 0.3: " + samples.join(" "));
    const mids = samples.filter((r) => r < 0.98 && r > 0.32);
    check("rate ramps through in-between speeds", mids.length >= 4, mids.length + " in-between samples");
    check("ramp only goes down (no overshoot)", samples.every((r, i) => i === 0 || r <= samples[i - 1] + 0.001));
    check("lands on 0.3×", Math.abs(video.playbackRate - 0.3) < 0.001 && vc.speed.textContent === "0.3×", vc.speed.textContent);
    check("leveller built", !!leveller, leveller ? leveller.ctx.state : "none");
    if (leveller) {
      if (leveller.ctx.state !== "running") await leveller.ctx.resume().catch(() => {});
      const an = leveller.ctx.createAnalyser();
      an.fftSize = 2048;
      leveller.wet.connect(an);
      const buf = new Float32Array(an.fftSize);
      let peak = 0;
      for (let i = 0; i < 30; i++) { an.getFloatTimeDomainData(buf); for (const v of buf) peak = Math.max(peak, Math.abs(v)); await wait(100); }
      check("levelled sound is coming through", peak > 0.001, "peak " + peak.toFixed(4) + " · ctx " + leveller.ctx.state);
      check("fully levelled at 0.3×", leveller.wet.gain.value > 0.95 && leveller.dry.gain.value < 0.05, `wet ${leveller.wet.gain.value.toFixed(2)} dry ${leveller.dry.gain.value.toFixed(2)}`);
      leveller.wet.disconnect(an);
      leveller.wet.connect(leveller.ctx.destination);
    }
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "]", bubbles: true }));
    await wait(1200);
    check("] adds 0.1", Math.abs(speedTarget - 0.4) < 1e-9, String(speedTarget));
    setSpeed(1);
    await wait(1200);
    check("back to 1× with dry sound", video.playbackRate === 1 && (!leveller || leveller.dry.gain.value > 0.95));
    openPlayer(state.list, 1);
    setSpeed(0.5);
    await wait(1500);
    step(-1);
    await wait(1500);
    check("speed kept on the next clip", Math.abs(video.playbackRate - 0.5) < 0.001, String(video.playbackRate));
    setSpeed(1, { instant: true });
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await wait(200);
    check("Esc closes the slider first", !speedPopOpen() && !!state.player);
    closePlayer();
    log("DONE");
    return;
  }
  if (mode === "scroll-stop") {
    go({ name: "all" });
    await wait(1000);
    while (main.scrollTop + main.clientHeight < main.scrollHeight - 5) {
      main.scrollTop += main.clientHeight * 0.8;
      await wait(100);
    }
    main.scrollTop = main.scrollHeight / 2;
    await wait(4000);
    const r = main.getBoundingClientRect();
    const inView = [...main.querySelectorAll(".clip")].filter((c) => { const b = c.getBoundingClientRect(); return b.bottom > r.top && b.top < r.bottom; });
    const ok = inView.filter((c) => { const i = c.querySelector("img"); return i.complete && i.naturalWidth > 0; }).length;
    invoke("debug_log", { msg: `scroll-stop: ${ok}/${inView.length} visible thumbnails loaded, ${main.querySelectorAll(".thumb img[src]").length} decoded in total` });
    return;
  }
  if (mode !== "scroll-all") return;
  go({ name: "all" });
  await wait(1000);
  while (main.scrollTop + main.clientHeight < main.scrollHeight - 5) {
    main.scrollTop += main.clientHeight * 0.8;
    await wait(250);
  }
  await wait(3000);
  openPlayer(state.list, 0);
  await wait(8000);
  closePlayer();
  await wait(5000);
  invoke("hide_window");
}

// While the hotkey is held by another app, keep retrying.
setInterval(async () => {
  if (!state.status || state.status.hotkey.registered) return;
  try {
    state.status = await invoke("get_status");
    renderHotkeyChip();
    if (state.status.hotkey.registered) {
      toast(`${fmtKey(state.status.hotkey.hotkey)} now opens Clipit`);
      if (state.view.name === "settings") rerenderSettings();
    }
  } catch {}
}, 5000);
