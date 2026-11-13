// NB_VERIFY G3: the whole "import and manage mixes / playlists" journey in a real browser
// against the REAL plippa-server binary and the REAL plippa-engine binary (mock audio sink), with
// hostile data in every text field. Not part of `cargo test`.
//
//   cargo build -p plippa-server -p plippa-engine
//   cd bins/plippa-server/tests/browser && LIB=/tmp/lib_journey SHOTS=/tmp/shots_j node journey_real.mjs
//
// Needs Node + global playwright (PLAYWRIGHT_DIR) and Chromium; never `playwright install` in the cloud.
// The library starts empty; audio files are generated here (silent 8 kHz WAV, RIFF INFO tags).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(here, "../../../..");
const LIB = process.env.LIB || fs.mkdtempSync(path.join(os.tmpdir(), "plippa-journey-"));
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8830";
const BASE = `http://127.0.0.1:${PORT}`;
const SERVER_BIN = process.env.SERVER_BIN || path.join(ROOT, "target/debug/plippa-server");
const ENGINE_BIN = process.env.ENGINE_BIN || path.join(ROOT, "target/debug/plippa-engine");
fs.mkdirSync(SHOTS, { recursive: true });
fs.rmSync(LIB, { recursive: true, force: true });
fs.mkdirSync(LIB, { recursive: true });
fs.writeFileSync(path.join(LIB, ".gitkeep"), "");
const IPC = fs.mkdtempSync(path.join(os.tmpdir(), "plippa-journey-ipc-"));
const AUDIO = fs.mkdtempSync(path.join(os.tmpdir(), "plippa-journey-audio-"));

let failures = 0;
const check = (name, ok, extra) => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${ok || extra === undefined ? "" : "  -> " + JSON.stringify(extra)}`);
  if (!ok) failures++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, ms = 10000, step = 50) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    const v = await fn();
    if (v) return v;
    await sleep(step);
  }
  return false;
}

// ---- audio fixtures ------------------------------------------------------------------------
const chunk = (id, data) => {
  const pad = data.length % 2 ? Buffer.from([0]) : Buffer.alloc(0);
  const h = Buffer.alloc(8);
  h.write(id, 0, "latin1");
  h.writeUInt32LE(data.length, 4);
  return Buffer.concat([h, data, pad]);
};
function wav(secs, info) {
  const rate = 8000;
  const fmt = Buffer.alloc(16);
  fmt.writeUInt16LE(1, 0); fmt.writeUInt16LE(1, 2); fmt.writeUInt32LE(rate, 4); fmt.writeUInt32LE(rate, 8); fmt.writeUInt16LE(1, 12); fmt.writeUInt16LE(8, 14);
  const parts = [Buffer.from("WAVE"), chunk("fmt ", fmt)];
  if (info) {
    parts.push(chunk("LIST", Buffer.concat([Buffer.from("INFO"), ...Object.entries(info).map(([k, v]) => chunk(k, Buffer.from(v + "\0", "utf8")))])));
  }
  parts.push(chunk("data", Buffer.alloc(rate * secs, 128)));
  const body = Buffer.concat(parts);
  const h = Buffer.alloc(8);
  h.write("RIFF", 0, "latin1");
  h.writeUInt32LE(body.length, 4);
  return Buffer.concat([h, body]);
}
const HOSTILE_TITLE = `<img src=x onerror=alert(1)>‮ evil​<script>alert(2)</script>`;
const writeAudio = (name, secs, info) => {
  const p = path.join(AUDIO, name);
  fs.writeFileSync(p, wav(secs, info));
  return p;
};
const mixFile = writeAudio("Friday Night Mix.wav", 600, { IART: "DJ Foo" }); // 10 minutes; an artist tag makes the import a release (untagged = looseTrack)
const trackNames = ["Alpha", "Bravo", "Charlie", "Delta", "Echo"];
const trackFiles = trackNames.map((n, i) => writeAudio(`0${i + 1} ${n}.wav`, 600, { IART: "The Band", IPRD: "Debut Album", INAM: n }));
const hostileTrack = writeAudio("hostile.wav", 600, { IART: "<b>Art</b>‮", IPRD: "<u>Al</u>", INAM: HOSTILE_TITLE });
const longTrack = writeAudio("long.wav", 600, { IART: "L".repeat(400), IPRD: "Longs", INAM: "T".repeat(3000) });

// ---- real stack ---------------------------------------------------------------------------
const env = { ...process.env, PLIPPA_IPC_DIR: IPC };
const engine = spawn(ENGINE_BIN, [`--library-root=${LIB}`, "--sink=mock", "--board=plippa-D-poc2"], { env, stdio: ["ignore", "ignore", "pipe"] });
let engineLog = "";
engine.stderr.on("data", (d) => (engineLog += d));
await until(() => fs.existsSync(path.join(IPC, "engine.sock")), 15000, 100);
const server = spawn(SERVER_BIN, ["--library-root", LIB, "--bind", `127.0.0.1:${PORT}`], { env, stdio: ["ignore", "ignore", "pipe"] });
let serverLog = "";
server.stderr.on("data", (d) => (serverLog += d));
const stop = () => { server.kill(); engine.kill(); };
process.on("exit", stop);
await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100);
const st0 = await (await fetch(BASE + "/api/status")).json();
check("real server is connected to the real engine (ipc_live)", st0.ipc_live === true, st0);

const api = async (p, opt = {}) => {
  const r = await fetch(BASE + p, { ...opt, headers: { "X-Plippa": "1", ...(opt.headers || {}) } });
  const t = await r.text();
  try { return JSON.parse(t); } catch { return t; }
};
const np = () => api("/api/now-playing");
const walk = (d, out = []) => {
  for (const e of fs.existsSync(d) ? fs.readdirSync(d, { withFileTypes: true }) : []) {
    const p = path.join(d, e.name);
    if (e.name === ".plippa") continue;
    e.isDirectory() ? walk(p, out) : out.push(path.relative(LIB, p));
  }
  return out.sort();
};
const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));
const mediaNames = (doc) => doc.mediaList.map((m) => path.basename(m.localFilePaths[0]));

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
const badRequests = [];
const notFound = []; // the hostile release lives in a folder whose name the path rules refuse, so its cover URL answers 404 (by design)
page.on("request", (r) => { if (/\/x(\?|$)/.test(new URL(r.url()).pathname + new URL(r.url()).search)) badRequests.push(r.url()); });
page.on("response", (r) => { if (r.url().startsWith(BASE) && r.status() >= 500) badRequests.push(`${r.status()} ${r.url()}`); if (r.status() === 404) notFound.push(r.url()); });
let alerts = 0;
page.on("dialog", async (d) => {
  if (d.type() === "confirm") return d.accept();
  alerts++;
  await d.dismiss();
});
const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png"), fullPage: false });
const toast = () => page.textContent("#toast");
const view = page.locator("#lib-view");

// Security headers on the real server (CSP has no unsafe-*), checked on page and a script and JSON.
for (const p of ["/", "/app.js", "/library.js", "/api/status"]) {
  const r = await fetch(BASE + p);
  const csp = r.headers.get("content-security-policy") || "";
  check(`CSP on ${p} has no unsafe-inline/unsafe-eval and a script-src fallback to 'self'`, csp.includes("default-src 'self'") && !csp.includes("unsafe") && !/script-src[^;]*\*/.test(csp), csp);
  check(`nosniff + no-referrer on ${p}`, r.headers.get("x-content-type-options") === "nosniff" && r.headers.get("referrer-policy") === "no-referrer");
}

await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");
check("empty library: Long-form tabs and Add buttons are there", (await page.locator("#add-open-btn, #addmulti-open-btn").count()) === 2);

// ===================================================================================
// 1. Import a mixtape with pasted timestamps (hostile chapter text included)
// ===================================================================================
const tracklist = `Friday Night Mix - full tracklist
00:00 Warm up - DJ Foo
02:30 - Second one
1. 05:12 <img src=x onerror=alert(1)> third
[07:40] Fourth (dub) ‮evil
09:05 ${"very long chapter title ".repeat(60)}

thanks for watching!
`;
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", mixFile);
await page.check('input[name="add-kind"][value="mix"]');
await page.fill("#add-cue-text", tracklist);
await page.waitForFunction(() => document.querySelectorAll("#add-preview table tbody tr").length === 5);
check("preview: 5 cues, hostile text inert", (await page.locator("#add-preview img").count()) === 0 && (await page.locator("#add-preview td", { hasText: "<img src=x" }).count()) === 1);
check("preview dialog does not overflow the viewport horizontally", await page.evaluate(() => { const d = document.getElementById("add-dialog"); return d.scrollWidth <= d.clientWidth + 1; }));
await shot("j01-add-mixtape");
await page.click("#add-ok");
await page.waitForFunction(() => !document.getElementById("add-dialog").open, null, { timeout: 20000 });
await page.waitForFunction(() => location.hash === "#/mixes");
check("toast says 5 cues", (await toast()).includes("5 cues"), await toast());
const mixRel = walk(LIB).find((f) => f.startsWith("Music/Mixes/") && f.endsWith(".noudata"));
check("mixtape .noudata on disk with 5 nav cues", !!mixRel && readDoc(mixRel).mediaList[0].cueLists[0].cues.length === 5, walk(LIB));
await page.waitForSelector("#lib-view table.tracks");
await page.click('#lib-view table.tracks .title-cell a:has-text("Friday Night Mix")');
await page.waitForSelector("#lib-view ol.items li");
await page.click('#lib-view button:has-text("Cues")');
await page.waitForSelector("ul.cue-list .cue-btn");
check("5 cues listed, hostile cue text is text", (await page.locator("ul.cue-list .cue-btn").count()) === 5 && (await page.locator("#lib-view img").count()) === 0);
check("layout: long cue text does not cause horizontal page scroll", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), await page.evaluate(() => document.documentElement.scrollWidth));
await shot("j02-mixtape-cues");

// ===================================================================================
// 2. Play it on the real engine; jump to a cue
// ===================================================================================
await page.click("#coll-play");
const playing = await until(async () => (await np()).transport === "Playing", 10000, 100);
check("real engine: Play on the mixtape -> Playing", !!playing, await np());
const n1 = await np();
check("now-playing names the collection and is a 1-item list", n1.collection_title === "Friday Night Mix" && n1.track_count === 1, n1);
await page.click("ul.cue-list .cue-btn >> nth=2"); // 05:12
const jumped = await until(async () => { const n = await np(); return n.cue_title && n.cue_title.includes("third") && n.position_ms >= 311000 && n.position_ms < 330000; }, 10000, 100);
check("real engine: clicking a cue seeks there (cue 3 at ~5:12, same track)", !!jumped, await np());
check("now-playing bar shows the cue", await until(async () => (await page.textContent("#np-cue")).includes("third"), 5000, 100));
await page.click("#next-btn");
check("Next goes to the next cue", !!(await until(async () => (await np()).cue_title?.includes("Fourth"), 5000, 100)), await np());
await page.click("#play-pause-btn");
check("Pause works", !!(await until(async () => (await np()).transport === "Paused", 5000, 100)), await np());
await shot("j03-playing");

// ===================================================================================
// 3. Edit the timestamps afterwards
// ===================================================================================
// Pausing made the engine ask the server to write the resume point into this same .noudata (it
// lands about a second later, while the dialog is open): Save must still work, because the cue
// list itself did not change (editcues.js retries with the fresh revision).
await page.locator("#lib-view li.item").first().locator("button.kebab").click();
await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
check("dialog pre-filled with the 5 saved cues", (await page.inputValue("#cues-text")).split("\n").filter(Boolean).length === 5);
check("dialog warns that the loaded item keeps the old cues until reloaded", await page.locator("#cues-loaded-note").isVisible());
await page.fill("#cues-text", "00:00 Warm up\n03:00 Fixed second\n06:30 New third\n08:00 Closer");
await page.waitForFunction(() => document.querySelectorAll("#cues-preview table tbody tr").length === 4);
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open, null, { timeout: 8000 }).catch(async (e) => { console.log("DEBUG save", await page.evaluate(() => JSON.stringify({ err: document.getElementById("cues-error").textContent, toast: document.getElementById("toast").textContent, saveTxt: document.getElementById("cues-save").textContent, dis: document.getElementById("cues-save").disabled }))); throw e; });
const navTexts = readDoc(mixRel).mediaList[0].cueLists.find((l) => l.cueListType === "nav").cues.map((c) => c.cueTextSpans.map((s) => s.text).join(""));
check("edited cues persisted (4 cues)", navTexts.join("|") === "Warm up|Fixed second|New third|Closer", navTexts);
// stop + play again: the engine picks up the new cues
await page.click("#unload-btn");
await until(async () => (await np()).transport === "Stopped", 5000, 100);
await page.click("#coll-play");
await until(async () => (await np()).transport === "Playing", 10000, 100);
await page.waitForSelector('#lib-view .cues-toggle:has-text("Cues (4)")');
const l1 = await api("/api/now-playing/list");
check("after reload the engine publishes the 4 edited cues", l1.entries.filter((e) => e.kind === "cue").map((e) => e.title).join("|") === "Warm up|Fixed second|New third|Closer", l1.entries.map((e) => e.title));
await page.click("#unload-btn");

// ===================================================================================
// 4. Library tracks -> album via Add multiple media (hostile + very long tags included)
// ===================================================================================
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", [...trackFiles, hostileTrack, longTrack]);
await page.fill("#addmulti-name", "Debut Album");
await page.check('input[name="addmulti-mode"][value="album"]');
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 30000 });
await page.waitForFunction(() => /^#\/albums\/\d+$/.test(location.hash));
await page.waitForSelector("#lib-view ol.items li.item");
check("album has 7 tracks", (await page.locator("#lib-view ol.items li.item").count()) === 7);
check("hostile track title is text (no <img>, no <script>, no request to /x, no alert)", (await view.locator("img[src='x'], script").count()) === 0 && badRequests.length === 0 && alerts === 0, badRequests);
check("hostile title appears without the bidi/zero-width characters", !(await view.textContent()).match(/[‮​]/));
check("layout: 3000-char title does not cause horizontal scroll", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), await page.evaluate(() => document.documentElement.scrollWidth));
await shot("j04-album");
const albumRel = walk(LIB).find((f) => f.includes("Debut Album") && f.endsWith(".noudata"));
check("album on disk in the chosen order", !!albumRel && mediaNames(readDoc(albumRel)).slice(0, 5).join() === "01 Alpha.wav,02 Bravo.wav,03 Charlie.wav,04 Delta.wav,05 Echo.wav", albumRel && mediaNames(readDoc(albumRel)));

// play the album on the real engine, Next / Previous
await page.click("#coll-play");
await until(async () => (await np()).transport === "Playing" && (await np()).track_count === 7, 10000, 100);
check("real engine loaded the album (7 entries)", (await np()).track_count === 7, await np());
await page.click("#next-btn");
check("Next -> track 2", !!(await until(async () => (await np()).track_index === 1, 5000, 100)), await np());
await page.click('#lib-view li.item:nth-child(4) button[title="Play from here"]');
check("play from item 4 jumps there", !!(await until(async () => (await np()).track_index === 3, 10000, 100)), await np());
await page.click("#unload-btn");

// ===================================================================================
// 5. Playlist from library tracks via the ⋮ menu, reorder, rename, delete an item, play
// ===================================================================================
await page.click('#lib-view li.item:nth-child(1) button[aria-haspopup="menu"]');
await page.click('.menu button:has-text("Add to playlist")');
await page.waitForSelector('.menu button:has-text("New playlist")');
await page.click('.menu button:has-text("New playlist")');
await page.fill('.menu input[type="text"]', `Road <b>trip</b>‮ ${"x".repeat(50)}`);
await page.keyboard.press("Enter");
await page.waitForFunction(() => document.getElementById("toast").textContent.includes("new playlist"));
for (const pos of [3, 5]) {
  await page.click(`#lib-view li.item:nth-child(${pos}) button[aria-haspopup="menu"]`);
  await page.click('.menu button:has-text("Add to playlist")');
  await page.waitForSelector(".menu button.menuitem:not(.disabled-look)");
  await page.click('.menu button:has-text("Road")');
  await page.waitForFunction((t) => document.getElementById("toast").textContent.includes("Added"), null);
  await sleep(150);
}
await page.goto(BASE + "/#/playlists");
await page.waitForSelector("#lib-view .card");
check("playlists tab: one playlist, title literal", (await view.locator(".card").count()) === 1 && (await view.textContent()).includes("Road <b>trip</b>") && (await view.locator("b").count()) === 0);
await view.locator(".card").first().click();
await page.waitForSelector("#lib-view ol.items li.item");
check("playlist has 3 tracks (Alpha, Charlie, Echo)", (await page.locator("#lib-view ol.items li.item .item-title strong").allTextContents()).join() === "Alpha,Charlie,Echo", await page.locator("#lib-view ol.items li.item .item-title strong").allTextContents());
await page.click("#coll-edit");
await page.waitForSelector("#lib-view ol.items.editing");
await page.click('#lib-view li.item:nth-child(3) button[title="Move up"]');
await page.waitForFunction(() => document.querySelector("ol.items li:nth-child(2) strong")?.textContent === "Echo");
check("reorder by button persisted (Alpha, Echo, Charlie)", (() => { const f = walk(LIB).find((x) => x.startsWith("Playlists/")); return mediaNames(readDoc(f)).join() === "01 Alpha.wav,05 Echo.wav,03 Charlie.wav"; })(), walk(LIB).filter((f) => f.startsWith("Playlists")));
await page.dragAndDrop("#lib-view li.item:nth-child(3)", "#lib-view li.item:nth-child(1)");
await page.waitForFunction(() => document.querySelector("ol.items li:nth-child(1) strong")?.textContent === "Charlie");
check("drag reorder persisted (Charlie first)", (() => { const f = walk(LIB).find((x) => x.startsWith("Playlists/")); return mediaNames(readDoc(f))[0] === "03 Charlie.wav"; })());
const oldHash = await page.evaluate(() => location.hash);
await page.fill("#rename-input", "My Road Trip");
await page.click("#rename-btn");
// the file is renamed (the id may or may not change): wait for the toast and the re-rendered view
await page.waitForFunction(() => document.getElementById("toast").textContent.includes("Renamed to") && document.getElementById("rename-input")?.value === "My Road Trip" && document.querySelectorAll("ol.items li.item").length === 3);
await sleep(300); // let the re-render triggered by the rename settle before the next click
check("rename moved the file", walk(LIB).some((f) => f === "Playlists/_My Road Trip.noudata"), walk(LIB).filter((f) => f.startsWith("Playlists")));
await page.click('#lib-view li.item:nth-child(2) button[title="Remove from playlist"]');
await page.waitForFunction(() => document.querySelectorAll("ol.items li.item").length === 2);
check("remove item persisted (2 left)", mediaNames(readDoc("Playlists/_My Road Trip.noudata")).length === 2);
await shot("j05-playlist-edit");
await page.click("#coll-edit"); // leave edit mode
await page.click("#coll-play");
await until(async () => (await np()).transport === "Playing" && (await np()).track_count === 2, 10000, 100);
const plList = await api("/api/now-playing/list");
check("real engine plays the playlist in the edited order", (await np()).collection_title === "My Road Trip" && plList.entries.filter((e) => e.kind === "track").map((e) => e.title).join() === "Charlie,Echo", plList.entries.map((e) => e.title));
await page.click("#unload-btn");

// ===================================================================================
// 6. Delete the playlist; restart the server over the same folder
// ===================================================================================
await page.click("#coll-edit");
await page.click("#coll-delete");
await page.waitForFunction(() => location.hash === "#/playlists");
check("playlist deleted from disk", !walk(LIB).some((f) => f.startsWith("Playlists/")), walk(LIB).filter((f) => f.startsWith("Playlists")));
check("empty Playlists tab shows an empty state", (await view.locator(".empty").count()) >= 1);

// ===================================================================================
// 7. A stick somebody else prepared: hostile text in EVERY field of the .noudata
// ===================================================================================
const EVIL = [
  `<img src=x onerror=alert(1)>`, `<script>alert(2)</script>`, `"><svg onload=alert(3)>`, `javascript:alert(4)`,
  `\u202Eevil\u202C rtl \u200B zero\u2066width\u2069 \u0007bell \u001b[2J esc \uFEFF bom`, `${"W".repeat(4000)}`, `' OR 1=1; --`, "`$(touch /tmp/pwned)`", `{{constructor.constructor('alert(5)')()}}`,
];
const ev = (i) => EVIL[i % EVIL.length];
const hdir = path.join(LIB, "Music", `<b>Evil</b> ${"d".repeat(60)}`, "Rel <i>x</i>");
fs.mkdirSync(hdir, { recursive: true });
const hw = wav(30);
const hfiles = [`<img src=x onerror=alert(1)>.wav`, `a"b'c.wav`, `javascript:alert(1).wav`].map((n) => { fs.writeFileSync(path.join(hdir, n), hw); return n; });
fs.writeFileSync(path.join(hdir, "cover.png"), Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==", "base64"));
fs.writeFileSync(path.join(hdir, "_Rel.noudata"), JSON.stringify({
  noudataVersion: "0.4.0", noubinKey: "x.noubin.ABC", playableItemTitle: ev(0) + ev(4),
  noudataType: "release", playableItemProfile: "mix", releaseData: { credits: { primaryArtists: [{ primaryArtistName: ev(1) + ev(4) }] }, coverImage: { images: [{ imageLocalFilename: "cover.png" }, { imageLocalFilename: "../../../etc/passwd" }] }, tags: [ev(2), ev(3)], description: ev(5) },
  localPlaybackData: { userNotes: { createdByUser: ev(2), note: ev(0) } },
  mediaList: hfiles.map((f, i) => ({ mediaTitle: ev(i * 2) + ev(4), localFilePaths: [f], cueLists: [{ cueListType: "nav", cueListTitle: ev(3), cues: [{ startTime: "0", cueTextSpans: [{ text: ev(i) + ev(4) }] }, { startTime: "0:10", cueTextSpans: [{ text: ev(5) }] }] }] })),
}));
await api("/api/rescan", { method: "POST" });
await page.goto(BASE + "/#/mixes");
await page.waitForSelector("#lib-view .card");
const domAudit = () => page.evaluate(() => {
  const bad = [];
  for (const el of document.querySelectorAll("*")) {
    for (const a of el.attributes) if (/^on/i.test(a.name)) bad.push(`${el.tagName}[${a.name}]`);
    if (el.tagName === "SCRIPT" && !/^\/[a-z]+\.js$/.test(el.getAttribute("src") || "")) bad.push("inline/foreign script");
    if (["IFRAME", "OBJECT", "EMBED", "SVG", "FORM", "LINK", "BASE", "META"].includes(el.tagName.toUpperCase()) && !(el.tagName === "LINK" && el.getAttribute("href") === "/styles.css") && el.tagName !== "META") bad.push(el.tagName);
    if (el.tagName === "IMG" && !(el.getAttribute("src") || "").startsWith("/api/collections/")) bad.push("img src=" + el.getAttribute("src"));
    if (el.tagName === "A" && /^\s*javascript:/i.test(el.getAttribute("href") || "")) bad.push("a javascript:");
    if (el.getAttribute("style") && !el.classList.contains("menu")) bad.push(el.tagName + "[style] " + el.outerHTML.slice(0, 120));
  }
  return bad;
});
const noBad = async (label) => {
  const bad = await domAudit();
  const wide = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  check(`hostile: ${label}: DOM has no handlers/foreign elements/style attributes`, bad.length === 0, bad);
  check(`hostile: ${label}: no horizontal page scroll (4000-char strings)`, wide <= 1, wide);
  const badTxt = (await page.textContent("body")).match(/.{0,30}[\u202A-\u202E\u2066-\u2069\u200B-\u200F\uFEFF\u0000-\u0008\u001b].{0,10}/);
  check(`hostile: ${label}: no control/bidi characters reach the page text`, !badTxt, badTxt && JSON.stringify(badTxt[0]));
};
await noBad("mixtape cards");
await page.click('#lib-view .card:has-text("<img src=x")');
await page.waitForSelector("#lib-view ol.items li.item");
await page.click('#lib-view .cues-toggle >> nth=0');
await page.waitForSelector("ul.cue-list .cue-btn");
await noBad("collection view + cues");
await page.click('#lib-view li.item:nth-child(1) button[aria-haspopup="menu"]');
await noBad("⋮ menu open");
await page.keyboard.press("Escape");
await page.click("#coll-details");
await page.waitForSelector("#detail-panel:not(.hidden)");
await noBad("detail panel");
// FIX_01E/FIX_01 review: timestamps live only in the ⋮ menu now. This block is the hostile-
// text test for the editor, so it must fail loudly if the dialog cannot be opened — no catches.
await page.click('#lib-view li.item:nth-child(1) button[aria-haspopup="menu"]');
await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
await page.waitForFunction(() => document.getElementById("cues-dialog")?.open === true, null, { timeout: 5000 });
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
check("hostile: ⋮ → Edit timestamps opens the dialog with the hostile cue text as text",
  (await page.inputValue("#cues-text")).includes("<img src=x onerror=alert(1)>"));
await noBad("edit timestamps dialog (hostile text in box, preview, subject)");
await page.click("#cues-cancel");
await page.click('#lib-view li.item:nth-child(1) button[title="Play from here"]');
await until(async () => (await np()).transport === "Playing", 10000, 100);
await sleep(1200); // let the 1 s poll render the now-playing bar
await noBad("now-playing bar with a hostile title");
const npText = await page.textContent("#now-playing");
check("now-playing bar shows the title as text", npText.includes("<img src=x") , npText.slice(0, 200));
await page.click("#unload-btn");
await page.goto(BASE + "/#/tracks");
await page.waitForSelector("#lib-view table.tracks, #lib-view .empty");
await noBad("tracks tab");
// hostile playlist name via the API is sanitised; the page stays intact
const made = await api("/api/playlists", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ title: `<img src=x onerror=alert(1)>\u202E ${"P".repeat(150)}`, tracks: [] }) });
await page.goto(BASE + "/#/playlists");
await page.waitForSelector("#lib-view .card");
await noBad("playlists tab with a hostile playlist name");
check("hostile playlist title stored without bidi/control characters", !JSON.stringify(made).match(/[\u202E]/), made.title);
check("hostile: no alert, no request to /x", alerts === 0 && badRequests.length === 0, badRequests);
await shot("j07-hostile");

// ===================================================================================
// 7. Static checks that the UI stayed clean
// ===================================================================================
check("no alert() fired", alerts === 0);
const unexpected404 = notFound.filter((u) => !u.endsWith("/cover"));
check("no console/page errors (other than refused cover URLs), no 5xx, no other 404", problems.filter((p) => !p.includes("status of 404") && !p.includes("status of 409")).length === 0 && badRequests.length === 0 && unexpected404.length === 0, [...problems, ...badRequests, ...unexpected404]);
check("server log has no panic", !/panicked/.test(serverLog) && !/panicked/.test(engineLog), (serverLog + engineLog).split("\n").filter((l) => /panick/.test(l)));
await page.setViewportSize({ width: 390, height: 800 });
await page.goto(BASE + "/#/albums");
await view.locator(".card").first().waitFor();
check("phone width: no horizontal scroll on the album list", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
await browser.close();
stop();
console.log(failures ? `${failures} FAILED` : "ALL OK");
process.exit(failures ? 1 : 0);
