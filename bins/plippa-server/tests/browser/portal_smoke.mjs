// Browser smoke test (Playwright + Chromium). Not part of `cargo test`.
//
//   python3 mkfix.py <dir>            # fixture library or your own
//   cargo run -p plippa-server --example portal_fixture -- <dir> 8799 &
//   NODE_PATH=/opt/node22/lib/node_modules LIB=<dir> BASE=http://127.0.0.1:8799 \
//     SHOTS=<screenshot-dir> node portal_smoke.mjs
//
// The example server has a fake engine, so Play / Next / cue jumps are answered like the real one.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import os from "node:os";

const require = createRequire(import.meta.url);
// ESM ignores NODE_PATH, so resolve the globally installed playwright explicitly.
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");

const BASE = process.env.BASE || "http://127.0.0.1:8799";
const LIB = process.env.LIB;
const SHOTS = process.env.SHOTS || ".";
fs.mkdirSync(SHOTS, { recursive: true });

let failures = 0;
const check = (name, ok, extra) => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${ok || extra === undefined ? "" : "  -> " + extra}`);
  if (!ok) failures++;
};

const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));
const names = (doc) => doc.mediaList.map((m) => path.basename(m.localFilePaths[0]));

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
let alerts = 0;
page.on("dialog", async (d) => {
  if (d.type() === "confirm") return d.accept();
  alerts++;
  await d.dismiss();
});

const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png"), fullPage: true });
const view = page.locator("#lib-view");

await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");

// --- navigation ---------------------------------------------------------------
check("one tab row with the six views, in order", (await page.locator(".tab-btn").allTextContents()).map((t) => t.replace(/ \(\d+\)/, "")).join() === "Tracks,Albums,Playlists,Podcasts,Audiobooks,Mixes");
await page.click('[data-tab="mixes"]');
await page.waitForSelector('[data-tab="mixes"].on');
check("hash follows", page.url().endsWith("#/mixes"));
await shot("01-mixes");
await page.click('[data-tab="tracks"]');
await page.waitForSelector('[data-tab="tracks"].on');
await view.locator("table.tracks", { hasText: "orphan song" }).waitFor();
check("tracks lists the loose file", (await view.textContent()).includes("orphan song"));
await page.fill("#lib-filter", "zzz");
check("filter empties", (await view.textContent()).includes("Nothing matches"));
await page.fill("#lib-filter", "");
await shot("02-tracks");

// --- albums, hostile titles ------------------------------------------------------
await page.click('[data-tab="albums"]');
await view.locator(".card").first().waitFor();
check("album cards", (await view.locator(".card").count()) === 2);
check("hostile title is text, not markup", (await view.locator("img[src='x']").count()) === 0 && (await view.textContent()).includes("<img src=x onerror=alert(1)>"));
check("album cover img present", (await view.locator(".card img").count()) === 1);
await shot("03-albums");
await view.locator(".card", { hasText: "The Album" }).click();
await view.locator("ol.items li").first().waitFor();
check("album items", (await view.locator("ol.items li").count()) === 3);
// FIX_01D: albums are reorderable (but not "editable": no ✕ / rename / delete).
check("album has an Edit button", (await page.locator("#coll-edit").count()) === 1);
await page.click("#coll-edit");
check(
  "album edit mode: arrows only (no remove, rename or delete)",
  (await page.locator('button[title="Move up"]').count()) === 3 &&
    (await page.locator('button[title="Remove from playlist"]').count()) === 0 &&
    (await page.locator("#rename-input").count()) === 0 &&
    (await page.locator("#coll-delete").count()) === 0
);
check(
  "album edit mode explains that filenames are not changed",
  (await page.textContent("#coll-reorder-note")).includes("filenames are not changed")
);
await page.locator("ol.items li").nth(1).locator('button[title="Move up"]').click();
await page.waitForFunction(() => document.querySelector("ol.items li .item-title strong").textContent === "Two");
const albumRel = "Music/Band/Album/_Album.noudata";
check(
  "↑ writes the album mediaList order",
  names(readDoc(albumRel)).join() === "02 - Two.mp3,01 - One.mp3,03 - Three.mp3",
  names(readDoc(albumRel)).join()
);
check(
  "...and no media file is renamed",
  ["01 - One.mp3", "02 - Two.mp3", "03 - Three.mp3"].every((f) => fs.existsSync(path.join(LIB, "Music/Band/Album", f)))
);
await page.reload();
await view.locator("ol.items li").first().waitFor();
check(
  "album order persists across a reload",
  (await page.locator("ol.items li .item-title strong").allTextContents()).join() === "Two,One,Three"
);
await page.click("#coll-edit");
await page.locator("ol.items li").nth(0).locator('button[title="Move down"]').click();
await page.waitForFunction(() => document.querySelector("ol.items li .item-title strong").textContent === "One");
check(
  "↓ restores the album order (back to the fixture order)",
  names(readDoc(albumRel)).join() === "01 - One.mp3,02 - Two.mp3,03 - Three.mp3",
  names(readDoc(albumRel)).join()
);
await page.click("#coll-edit"); // Done
await page.click("#coll-play");
await page.waitForFunction(() => document.getElementById("np-collection").textContent === "The Album");
check("now-playing shows collection + track", (await page.textContent("#np-entry")).includes("Track 1/3"));
await page.locator("ol.items li").nth(1).locator('button[title="Play from here"]').click();
await page.waitForFunction(() => document.getElementById("np-entry").textContent.includes("Track 2/3"));
check("play from item 2", true);
check("row highlighted as playing", await page.locator("li.item.playing").count() === 1);
await page.click("#next-btn");
await page.waitForFunction(() => document.getElementById("np-entry").textContent.includes("Track 3/3"));
check("Next advances", true);
await shot("04-album");

// --- three-dot menu ---------------------------------------------------------------
const kebab = page.locator("ol.items li").nth(1).locator("button.kebab");
await kebab.focus();
await page.keyboard.press("Enter");
await page.waitForSelector(".menu [role=menuitem]");
check("menu opens with keyboard, focus inside", await page.evaluate(() => document.activeElement.getAttribute("role") === "menuitem"));
await page.keyboard.press("Escape");
check("Esc closes and returns focus", (await page.locator(".menu").count()) === 0 && (await page.evaluate(() => document.activeElement.classList.contains("kebab"))));
await kebab.click();
await page.click(".menu >> text=Add to playlist…");
await page.waitForSelector(".menu >> text=Rainy");
check("picker lists playlists + New playlist", (await page.locator(".menu .menuitem").allTextContents()).join("|").includes("New playlist…"));
// viewport-only: a full-page capture resizes the window, which closes an open menu
await page.screenshot({ path: path.join(SHOTS, "05-menu.png") });
await page.locator(".menu button", { hasText: "Rainy" }).click();
await page.waitForFunction(() => document.getElementById("toast").textContent.includes("Added “Two” to “Rainy”"));
check("toast on add", true);
check("Rainy on disk has 4 entries (dup allowed)", names(readDoc("Playlists/_Rainy.noudata")).join() === "01 - One.mp3,02 - Two.mp3,03 - Three.mp3,02 - Two.mp3");
await page.locator("ol.items li").nth(2).locator("button.kebab").click();
await page.click(".menu >> text=Add to playlist…");
await page.click(".menu >> text=New playlist…");
await page.fill('.menu input', "Road trip");
await page.keyboard.press("Enter");
await page.waitForFunction(() => document.getElementById("toast").textContent.includes("new playlist"));
check("New playlist created with the track", fs.existsSync(path.join(LIB, "Playlists/_Road trip.noudata")) && names(readDoc("Playlists/_Road trip.noudata")).join() === "03 - Three.mp3");
await page.mouse.click(5, 5);
check("outside click closes menu", (await page.locator(".menu").count()) === 0);
// menu on a tracks row + on a hostile album item
await page.goto(BASE + "/#/tracks");
await view.locator("table.tracks").waitFor();
await view.locator("tr", { hasText: "orphan song" }).locator("button.kebab").click();
await page.click(".menu >> text=Add to playlist…");
await page.locator(".menu button", { hasText: "Sunny" }).click();
await page.waitForFunction(() => document.getElementById("toast").textContent.includes("to “Sunny”"));
check("track row menu adds orphan to Sunny", names(readDoc("Playlists/_Sunny.noudata")).includes("orphan song.mp3"));

// --- playlists ---------------------------------------------------------------------
await page.click('[data-tab="playlists"]');
await view.locator(".card").first().waitFor();
await page.waitForFunction(() => document.querySelectorAll("#lib-view .card").length === 3);
check("playlists cards incl. new one", true);
await page.click("#create-playlist-btn");
await page.fill("#new-playlist-title", "Focus");
await page.click(".inline-form button.primary");
await view.locator("#rename-input").waitFor();
check("create playlist opens it in edit mode", (await page.inputValue("#rename-input")) === "Focus");
check("Focus on disk", fs.existsSync(path.join(LIB, "Playlists/_Focus.noudata")));
await page.goto(BASE + "/#/playlists");
await view.locator(".card", { hasText: "Rainy" }).click();
await view.locator("ol.items li").first().waitFor();
await page.click("#coll-edit");
check("edit mode shows ✕ and arrows", (await page.locator('button[title="Remove from playlist"]').count()) === 4 && (await page.locator('button[title="Move up"]').count()) === 4);
await shot("06-playlist-edit");
await page.locator("ol.items li").nth(1).locator('button[title="Move up"]').click();
await page.waitForFunction(() => document.querySelector("ol.items li .item-title strong").textContent === "Two");
check("↑ button reorders on disk", names(readDoc("Playlists/_Rainy.noudata")).join() === "02 - Two.mp3,01 - One.mp3,03 - Three.mp3,02 - Two.mp3");
await page.locator("ol.items li").nth(3).dragTo(page.locator("ol.items li").nth(0));
await page.waitForFunction(() => document.querySelectorAll("ol.items li .item-title strong")[0].textContent === "Two" && document.querySelectorAll("ol.items li .item-title strong")[1].textContent === "Two");
check("drag reorder persisted", names(readDoc("Playlists/_Rainy.noudata")).join() === "02 - Two.mp3,02 - Two.mp3,01 - One.mp3,03 - Three.mp3");
await page.locator("ol.items li").nth(0).locator('button[title="Remove from playlist"]').click();
await page.waitForFunction(() => document.querySelectorAll("ol.items li").length === 3);
check("✕ removes on disk", names(readDoc("Playlists/_Rainy.noudata")).join() === "02 - Two.mp3,01 - One.mp3,03 - Three.mp3");
await page.fill("#rename-input", "Rainy Days <b>");
await page.click("#rename-btn");
await page.waitForFunction(() => /#\/playlists\/\d+$/.test(location.hash) && document.querySelector(".collection")?.dataset.id === location.hash.split("/").pop() && document.getElementById("rename-input")?.value === "Rainy Days <b>");
check("rename moved the file", fs.readdirSync(path.join(LIB, "Playlists")).some((f) => f.startsWith("_Rainy Days")), fs.readdirSync(path.join(LIB, "Playlists")).join());
check("URL follows the new id", /#\/playlists\/\d+$/.test(page.url()));
await page.click("#coll-delete");
await page.waitForFunction(() => location.hash === "#/playlists" && document.querySelector("#lib-view .cards"));
check("delete removes file (confirm accepted)", !fs.readdirSync(path.join(LIB, "Playlists")).some((f) => f.startsWith("_Rainy")));
await shot("07-after-delete");

// --- long form ----------------------------------------------------------------------
await page.goto(BASE + "/#/mixes");
await view.locator("table.tracks .title-cell a", { hasText: "Rocksteady" }).click();
await view.locator(".cues-toggle").click();
await view.locator(".cue-btn").first().waitFor();
check("cue list shows 3 navigable cues (credits list hidden)", (await view.locator(".cue-btn").count()) === 3);
// FIX_01E: timestamps only in the ⋮ menu — the collection header has no timestamps button.
check("no header Edit timestamps button", (await page.locator("#coll-cues").count()) === 0);
check("no per-row Timestamps button", (await page.locator("#lib-view .cue-edit-btn").count()) === 0);
await page.locator("#lib-view li.item").first().locator("button.kebab").click();
const mixTs = page.locator('.menu [role="menuitem"]:has-text("Edit timestamps")');
check(
  "⋮ menu offers Edit timestamps for the mixtape item",
  (await mixTs.count()) === 1 && !(await mixTs.getAttribute("aria-disabled"))
);
await page.keyboard.press("Escape");
check("cue text is text", (await view.textContent()).includes("Peak time <i>x</i>") && (await view.locator("i").count()) === 0);
await shot("08-mixtape-cues");
await view.locator(".cue-btn", { hasText: "Peak time" }).click();
await page.waitForFunction(() => document.getElementById("np-cue").textContent.includes("Peak time"), null, { timeout: 15000 });
check("cue click seeks, bar shows the cue", (await page.textContent("#np-detail")).includes("10:00"));
await page.click("#next-btn");
await page.waitForFunction(() => document.getElementById("np-cue").textContent.includes("Cool down"));
check("Next goes to the next cue", true);
await page.goto(BASE + "/#/audiobooks");
await view.locator(".card", { hasText: "Hobbit" }).click();
await view.locator("ol.items li").first().waitFor();
check("audiobook items", (await view.locator("ol.items li").count()) === 2);
await page.goto(BASE + "/#/podcasts");
await view.locator(".card").first().waitFor();
check("podcast card", (await view.locator(".card", { hasText: "The Show" }).count()) === 1);

// --- persistence + deep link ---------------------------------------------------------
await page.goto(BASE + "/#/audiobooks");
await page.goto(BASE + "/");
await view.locator(".card").first().waitFor();
check("last tab restored from localStorage", page.url().endsWith("/") && (await page.locator(".tab-btn.on").textContent()).startsWith("Audiobooks"));
await page.goto(BASE + "/#/nonsense");
await page.waitForSelector(".tab-btn.on");
check("bad hash falls back", (await page.locator(".tab-btn.on").count()) === 1);

// --- narrow layout ---------------------------------------------------------------------
await page.setViewportSize({ width: 390, height: 800 });
await page.goto(BASE + "/#/albums");
await view.locator(".card").first().waitFor();
check("no horizontal scroll at phone width", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), await page.evaluate(() => document.documentElement.scrollWidth));
await shot("09-phone");

// --- restart persistence + empty library -------------------------------------------------
// A second server process over the same folder: what the first one wrote must be what it serves.
const BIN = process.env.BIN || "../../../../target/debug/examples/portal_fixture";
const startServer = async (lib, port) => {
  const child = spawn(BIN, [lib, String(port)], { stdio: "ignore" });
  for (let i = 0; i < 100; i++) {
    try { if ((await fetch(`http://127.0.0.1:${port}/api/kinds`)).ok) return child; } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill();
  throw new Error("second server did not start");
};
const second = await startServer(LIB, 8797);
const pl = await (await fetch("http://127.0.0.1:8797/api/collections?kind=playlist")).json();
check("after a server restart the edited playlists are still there", ["Focus", "Road trip", "Sunny"].every((t) => pl.collections.some((c) => c.title === t)) && !pl.collections.some((c) => c.title.startsWith("Rainy")), JSON.stringify(pl.collections.map((c) => c.title)));
second.kill();

const emptyLib = fs.mkdtempSync(path.join(os.tmpdir(), "plippa-empty-"));
fs.writeFileSync(path.join(emptyLib, "song.mp3"), "");
const third = await startServer(emptyLib, 8796);
const errs = [];
const p2 = await ctx.newPage();
p2.on("console", (m) => m.type() === "error" && errs.push(m.text()));
p2.on("pageerror", (e) => errs.push(e.message));
await p2.goto("http://127.0.0.1:8796/#/mixes");
await p2.waitForSelector('[data-empty="mixes"]');
for (const [tab, key] of [["audiobooks", "audiobooks"], ["podcasts", "podcasts"], ["mixes", "mixes"]]) {
  await p2.goto(`http://127.0.0.1:8796/#/${tab}`);
  await p2.waitForSelector(`[data-empty="${key}"]`);
}
check("empty Podcasts/Audiobooks/Mixes tabs show an empty state with an Add media hint", (await p2.textContent("#lib-view")).includes("Add media"));
check("all six tabs are visible with nothing in them", (await p2.locator(".tab-btn").count()) === 6);
await p2.screenshot({ path: path.join(SHOTS, "10-empty-podcasts.png"), fullPage: true });
await p2.goto("http://127.0.0.1:8796/#/albums");
await p2.waitForSelector('[data-empty="albums"]');
await p2.goto("http://127.0.0.1:8796/#/playlists");
await p2.waitForSelector('[data-empty="playlists"]');
check("no console errors on the empty library", errs.length === 0, errs.join(" | "));
third.kill();

check("no alert() ever fired", alerts === 0);
check("no console/page errors", problems.length === 0, problems.join(" | "));
await browser.close();
console.log(failures ? `${failures} FAILED` : "ALL OK");
process.exit(failures ? 1 : 0);
