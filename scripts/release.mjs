// Builds a signed Clipit release and publishes it on GitHub, where installed
// copies of Clipit find it through the updater.
//
//   npm run release            -> builds, then publishes vX.Y.Z (version from tauri.conf.json)
//   npm run release -- --dry   -> builds and writes latest.json, publishes nothing
//
// Needs: the updater signing key (TAURI_SIGNING_PRIVATE_KEY, or the file
// %USERPROFILE%\.clipit\updater.key), GitHub CLI signed in, and release notes for
// the version in CHANGELOG.md ("## X.Y.Z" heading).

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = "MoathByte/clipit";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dry = process.argv.includes("--dry");

const conf = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8"));
const version = conf.version;
const tag = `v${version}`;

// Release notes: the CHANGELOG section for this version.
const changelog = readFileSync(join(root, "CHANGELOG.md"), "utf8");
const section = changelog.split(/^## /m).find((s) => s.startsWith(version));
if (!section) throw new Error(`CHANGELOG.md has no "## ${version}" section`);
const notes = section.split("\n").slice(1).join("\n").trim();

// Signing key (never committed).
const env = { ...process.env };
if (!env.TAURI_SIGNING_PRIVATE_KEY) {
  const keyFile = join(homedir(), ".clipit", "updater.key");
  if (!existsSync(keyFile)) throw new Error(`No signing key: set TAURI_SIGNING_PRIVATE_KEY or create ${keyFile}`);
  env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(keyFile, "utf8").trim();
}
env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ??= "";

// npm is a .cmd script on Windows and needs a shell; everything else runs directly,
// so paths with spaces are passed safely.
const run = (cmd, args, shell = false) => execFileSync(cmd, args, { cwd: root, env, stdio: "inherit", shell });

console.log(`Building Clipit ${version}…`);
run("npm run build", [], true);

const target = process.env.CARGO_TARGET_DIR || join(root, "src-tauri", "target");
const nsis = join(target, "release", "bundle", "nsis");
const installer = `Clipit_${version}_x64-setup.exe`;
const exe = join(nsis, installer);
const sig = readFileSync(`${exe}.sig`, "utf8").trim();

const latest = {
  version,
  notes,
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": { signature: sig, url: `https://github.com/${REPO}/releases/download/${tag}/${installer}` },
  },
};
const latestPath = join(root, "latest.json");
writeFileSync(latestPath, JSON.stringify(latest, null, 2));
const notesPath = join(root, "release-notes.tmp.md");
writeFileSync(notesPath, notes);
console.log(`Wrote latest.json for ${tag}`);

if (dry) {
  console.log("Dry run: nothing published.");
} else {
  const gh = process.env.GH_PATH || "gh";
  run(gh, ["release", "create", tag, exe, latestPath, "--repo", REPO, "--title", `Clipit ${version}`, "--notes-file", notesPath]);
  console.log(`Published https://github.com/${REPO}/releases/tag/${tag}`);
}
