// NB_11 browser test (Playwright + Chromium): Add media / Add multiple media end to end.
// Not part of `cargo test`. It starts its own portal_fixture server over a temp library.
//
//   python3 mkfix.py /tmp/lib0 && python3 mkaudio.py /tmp/audio
//   cargo build -p plippa-server --example portal_fixture
//   AUDIO=/tmp/audio LIB=/tmp/lib1 SHOTS=/tmp/shots node addmedia_flow.mjs
//
// (LIB is created from the mkfix fixture; the server runs on PORT, default 8811.)
//
// ART_07 contract: files are uploaded AS THEY ARE DROPPED (import session); OK / Import only
// confirms. Tests therefore wait for uploads to settle before they click OK or reorder.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const AUDIO = process.env.AUDIO;
const LIB = process.env.LIB || "/tmp/plippa_nb11_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8811";
const BASE = `http://127.0.0.1:${PORT}`;
const BIN = process.env.BIN || path.join(here, "../../../../target/debug/examples/portal_fixture");
fs.mkdirSync(SHOTS, { recursive: true });

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

spawnSync("python3", [path.join(here, "mkfix.py"), LIB], { stdio: "inherit" });
const server = spawn(BIN, [LIB, PORT], { stdio: ["ignore", "ignore", "inherit"] });
process.on("exit", () => server.kill());
await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100);

const walk = (d, out = []) => {
  for (const e of fs.existsSync(d) ? fs.readdirSync(d, { withFileTypes: true }) : []) {
    const p = path.join(d, e.name);
    if (e.name === ".plippa") continue;
    e.isDirectory() ? walk(p, out) : out.push(path.relative(LIB, p));
  }
  return out.sort();
};
const tmpLeft = () => {
  const out = [];
  const w = (d) => {
    if (!fs.existsSync(d)) return;
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      e.isDirectory() ? w(p) : out.push(p);
    }
  };
  w(path.join(LIB, ".plippa/tmp"));
  return out;
};
const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));
const mediaNames = (doc) => doc.mediaList.map((m) => path.basename(m.localFilePaths[0]));

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
// (the 415s are the deliberate refusals of fake.mp3 below)
page.on("console", (m) => m.type() === "error" && !m.text().includes("status of 415") && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
let alerts = 0;
let confirms = 0;
page.on("dialog", async (d) => {
  if (d.type() === "confirm") {
    confirms++;
    return d.accept();
  }
  alerts++;
  await d.dismiss();
});
const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png"), fullPage: false });

// uploads happen on drop: wait until no row is pending/uploading
const settled = () => page.waitForFunction(() => !document.querySelector("#addmulti-list li.multi-item.pending, #addmulti-list li.multi-item.uploading") && !document.getElementById("addmulti-drop").classList.contains("disabled"), null, { timeout: 30000 });
const oneReady = () => page.waitForFunction(() => !document.getElementById("add-ok").disabled, null, { timeout: 30000 });
await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");
check("old inline Add control is gone", (await page.locator("#file-input, #import-kind").count()) === 0);
check("both buttons exist", (await page.locator("#add-open-btn").count()) === 1 && (await page.locator("#addmulti-open-btn").count()) === 1);

// ===================================================================================
// Modal 1: Add media
// ===================================================================================
await page.click("#add-open-btn");
check("dialog opens modally", await page.evaluate(() => document.getElementById("add-dialog").open));
check("OK disabled until a file is chosen", await page.locator("#add-ok").isDisabled());
check("focus moved into the dialog", await page.evaluate(() => document.getElementById("add-dialog").contains(document.activeElement)));
check("kinds: five options, Music default", (await page.locator('input[name="add-kind"]').count()) === 5 && (await page.locator('input[name="add-kind"][value="music"]').isChecked()));
check("timestamps hidden for Music", await page.locator("#add-cues").isHidden());
await shot("10-add-empty");
await page.keyboard.press("Escape");
check("Esc closes the dialog", !(await page.evaluate(() => document.getElementById("add-dialog").open)));
check("focus restored to the Add media button", await page.evaluate(() => document.activeElement.id === "add-open-btn"));

// several files dropped on the single-file zone -> told to use Add multiple media
await page.click("#add-open-btn");
await page.evaluate(() => {
  const dt = new DataTransfer();
  dt.items.add(new File([new Uint8Array(10)], "a.wav", { type: "audio/wav" }));
  dt.items.add(new File([new Uint8Array(10)], "b.wav", { type: "audio/wav" }));
  const z = document.getElementById("add-drop");
  z.dispatchEvent(new DragEvent("dragenter", { dataTransfer: dt, bubbles: true, cancelable: true }));
  z.dispatchEvent(new DragEvent("dragover", { dataTransfer: dt, bubbles: true, cancelable: true }));
});
check("drop zone shows the hover state", await page.locator("#add-drop.hover").count() === 1);
await page.evaluate(() => {
  const dt = new DataTransfer();
  dt.items.add(new File([new Uint8Array(10)], "a.wav", { type: "audio/wav" }));
  dt.items.add(new File([new Uint8Array(10)], "b.wav", { type: "audio/wav" }));
  document.getElementById("add-drop").dispatchEvent(new DragEvent("drop", { dataTransfer: dt, bubbles: true, cancelable: true }));
});
check("two dropped files: message points to Add multiple media", (await page.textContent("#add-error")).includes("Add multiple media"));
check("...and OK stays disabled", await page.locator("#add-ok").isDisabled());
// a non-audio file is refused client-side
await page.setInputFiles("#add-file", path.join(AUDIO, "notes.txt"));
check("non-audio file refused inline", (await page.textContent("#add-error")).includes("does not look like an audio file"));

// a real single file dropped via DataTransfer (mix1h.wav, 3.6 MB) -> Mixtape + tracklist
await page.setInputFiles("#add-file", path.join(AUDIO, "mix1h.wav"));
check("file chosen: OK enabled", await until(async () => !(await page.locator("#add-ok").isDisabled()), 2000));
await page.check('input[name="add-kind"][value="mix"]');
check("timestamps box appears for Mixtape", await page.locator("#add-cues").isVisible());
const tracklist = `Rocksteady Night Vol. 3 (full set)
00:00 Intro - DJ Foo
02:31 - Track Two
1. 05:12 Third one
[08:40] Fourth (dub)
11:03 – Fifth – special
14:20 Sixth
18:55 - Seventh
23:10 Eighth
27:45 Ninth
33:00 Tenth
41:30 Eleventh
55:00 Twelfth

thanks for watching, like and subscribe!
`;
await page.fill("#add-cue-text", tracklist);
await page.waitForSelector("#add-preview table tbody tr");
check("preview shows 12 cues (debounced)", (await page.locator("#add-preview table tbody tr").count()) === 12);
check("preview times are HH:MM:SS.mmm", (await page.locator("#add-preview td.cue-time").nth(2).textContent()) === "00:05:12.000");
check("preview counts ignored lines", (await page.textContent("#add-preview .preview-head")).includes("2 ignored lines"));
check("file length read client-side", (await page.textContent("#add-file-line")).includes("1:00:00"));
await shot("11-add-mixtape-preview");
// a cue beyond the length is flagged (duration known)
await page.fill("#add-cue-text", tracklist + "1:30:00 way too late\n");
await page.waitForSelector("#add-preview ul.preview-warn li");
check("cue past the end is dropped from the preview (12 left)", (await page.locator("#add-preview table tbody tr").count()) === 12);
check("cue beyond the file length gets a warning", (await page.locator("#add-preview ul.preview-warn li").count()) >= 1, await page.textContent("#add-preview"));
await page.fill("#add-cue-text", tracklist);
await page.waitForFunction(() => document.querySelectorAll("#add-preview table tbody tr").length === 12);
// hostile pasted text stays text
await page.fill("#add-cue-text", tracklist + "58:00 <img src=x onerror=alert(1)>\n");
await page.waitForFunction(() => document.querySelectorAll("#add-preview table tbody tr").length === 13);
check("hostile cue text rendered as text", (await page.locator("#add-preview img").count()) === 0 && (await page.locator("#add-preview td", { hasText: "<img src=x" }).count()) === 1);
await page.fill("#add-cue-text", tracklist);
await page.waitForFunction(() => document.querySelectorAll("#add-preview table tbody tr").length === 12);

await oneReady();
check("the file is staged on the device before OK is pressed", tmpLeft().length === 1, tmpLeft());
await page.click("#add-ok");
await page.waitForFunction(() => !document.getElementById("add-dialog").open, null, { timeout: 20000 });
check("upload done: dialog closed, toast shown", (await page.textContent("#toast")).includes("with 12 cues"), await page.textContent("#toast"));
await page.waitForFunction(() => location.hash === "#/mixes");
const mixRel = walk(LIB).find((f) => f.startsWith("Music/Mixes/") && f.endsWith(".noudata"));
check("mixtape .noudata written under Music/Mixes", !!mixRel, walk(LIB));
const mixDoc = mixRel ? readDoc(mixRel) : null;
const navList = mixDoc && mixDoc.mediaList[0].cueLists.find((l) => l.cueListType === "nav");
check("12 nav cues stored", navList && navList.cues.length === 12, navList && navList.cues.length);
check("v0.4.0: release + root playableItemProfile mix", mixDoc && mixDoc.noudataType === "release" && mixDoc.playableItemProfile === "mix" && !(mixDoc.releaseData || {}).releaseProfile, mixDoc);
check("no temp files left", tmpLeft().length === 0, tmpLeft());
// visible + navigable in the NB_10 view
await page.waitForSelector("#lib-view table.tracks");
check("new mix appears as a row in the Mixes tab", (await page.locator("#lib-view table.tracks tr", { hasText: "mix1h" }).count()) === 1);
await page.click('#lib-view table.tracks .title-cell a:has-text("mix1h")');
await page.waitForSelector("#lib-view ol.items");
await page.click('#lib-view button:has-text("Cues")');
await page.waitForSelector("ul.cue-list .cue-btn");
check("cues navigable in the collection view (12)", (await page.locator("ul.cue-list .cue-btn").count()) === 12);
await shot("12-mixtape-cues");
await page.click('ul.cue-list .cue-btn >> nth=3');
check("clicking a cue seeks (Cue shown in the now-playing bar)", !!(await until(async () => (await page.textContent("#np-cue")).includes("Fourth"), 5000, 100)), await page.textContent("#np-cue"));

// music via Add media: tags decide the folder, no timestamps box
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", path.join(AUDIO, "tracks/01 Alpha.wav"));
await oneReady();
await page.click("#add-ok");
await page.waitForFunction(() => !document.getElementById("add-dialog").open);
check("music track placed by its tags", walk(LIB).includes("Music/The Band/Debut/01 Alpha.wav"), walk(LIB).filter((f) => f.includes("Band")));

// Refused by the server (html with an .mp3 name): message shown inline, dialog stays open
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", path.join(AUDIO, "fake.mp3"));
await page.waitForFunction(() => document.getElementById("add-error").textContent.length > 0);
check("server refusal shown inline (at drop time)", (await page.textContent("#add-error")).includes("audio"), await page.textContent("#add-error"));
check("dialog still open, OK stays disabled (nothing staged), picker usable again", (await page.evaluate(() => document.getElementById("add-dialog").open)) && (await page.locator("#add-ok").isDisabled()) && !(await page.locator("#add-pick-btn").isDisabled()));
await page.click("#add-cancel");
check("cancel leaves no staging behind", await until(() => tmpLeft().length === 0, 3000), tmpLeft());

// ===================================================================================
// Modal 2: Add multiple media
// ===================================================================================
const tracks = fs.readdirSync(path.join(AUDIO, "tracks")).sort().map((f) => path.join(AUDIO, "tracks", f));
const items = () => page.locator("#addmulti-list li.multi-item");
const order = async () => (await page.locator("#addmulti-list .multi-fname").allTextContents()).map((s) => s.replace(/\.wav$/, ""));

await page.click("#addmulti-open-btn");
check("multi dialog open, OK disabled", (await page.evaluate(() => document.getElementById("addmulti-dialog").open)) && (await page.locator("#addmulti-ok").isDisabled()));
await page.setInputFiles("#addmulti-file", tracks);
await settled();
check("8 files listed", (await items().count()) === 8);
check("all 8 are already uploaded (upload on drop)", (await page.locator("#addmulti-list li.multi-item.done").count()) === 8 && tmpLeft().length === 8, tmpLeft().length);
await page.setInputFiles("#addmulti-file", [path.join(AUDIO, "notes.txt")]);
check("non-audio skipped with a note", (await page.textContent("#addmulti-skipped")).includes("notes.txt") && (await items().count()) === 8);
check("the name was prefilled from the album tag of the first upload", (await page.inputValue("#addmulti-name")) === "Debut", await page.inputValue("#addmulti-name"));
await page.fill("#addmulti-name", "");
check("OK disabled without a name", await page.locator("#addmulti-ok").isDisabled());
await page.fill("#addmulti-name", "X");
check("OK enabled with files + name", !(await page.locator("#addmulti-ok").isDisabled()));
// reorder: Bravo (#2) down twice via buttons, then Echo up once, remove Golf
await page.click('#addmulti-list li:nth-child(2) button[data-action="down"]');
check("focus stays on the moved row's button", await page.evaluate(() => document.activeElement.dataset.action === "down"));
await page.click('#addmulti-list li:nth-child(3) button[data-action="down"]');
await page.click('#addmulti-list li:nth-child(5) button[data-action="up"]');
let o = await order();
check("order after three button moves", o.join() === "01 Alpha,03 Charlie,04 Delta,05 Echo,02 Bravo,06 Foxtrot,07 Golf,08 Hotel", o);
// mouse drag & drop reorder: drag row 3 onto row 1 (both visible without scrolling the list)
await page.dragAndDrop("#addmulti-list li:nth-child(3)", "#addmulti-list li:nth-child(1)");
o = await order();
check("drag and drop moves the row", o.join() === "04 Delta,01 Alpha,03 Charlie,05 Echo,02 Bravo,06 Foxtrot,07 Golf,08 Hotel", o);
await shot("20-multi-list");
// remove Golf
await page.click('#addmulti-list li:has-text("Golf") button[data-action="remove"]');
await page.waitForFunction(() => document.querySelectorAll("#addmulti-list li.multi-item").length === 7);
check("removing an uploaded row deletes its staged file on the device", await until(() => tmpLeft().length === 7, 3000), tmpLeft().length);
o = await order();
check("remove works (7 left, no Golf)", o.length === 7 && !o.some((n) => n.includes("Golf")), o);
const wantOrder = o.slice();
check("chosen order", wantOrder.join() === "04 Delta,01 Alpha,03 Charlie,05 Echo,02 Bravo,06 Foxtrot,08 Hotel", wantOrder);
await page.check('input[name="addmulti-mode"][value="album"]');
check("album hint text", (await page.textContent("#addmulti-mode-hint")).includes("_<name>.noudata"));
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 20000 });
check("toast says album created", (await page.textContent("#toast")).includes("Created album"), await page.textContent("#toast"));
await page.waitForFunction(() => /^#\/albums\/\d+$/.test(location.hash));
const albumRel = "Music/The Band/X/_X.noudata";
check("album written at Music/<artist>/X/_X.noudata (artist from tags)", walk(LIB).includes(albumRel), walk(LIB).filter((f) => f.includes("X")));
const albumDoc = readDoc(albumRel);
check("playableItemTitle = X", albumDoc.playableItemTitle === "X");
const onDisk = mediaNames(albumDoc).map((n) => n.replace(/\.wav$/, ""));
check("album mediaList matches the chosen order (7 tracks)", onDisk.length === 7 && onDisk.join() === wantOrder.join(), { onDisk, wantOrder });
check("album files exist in the folder", mediaNames(albumDoc).every((n) => fs.existsSync(path.join(LIB, "Music/The Band/X", n))));
check("no Golf on disk in X", !walk(LIB).some((f) => f.startsWith("Music/The Band/X/") && f.includes("Golf")));
check("no staging left", tmpLeft().length === 0, tmpLeft());
await page.waitForSelector("#lib-view ol.items li");
check("collection view lists 7 items in that order", (await page.locator("#lib-view ol.items li.item").count()) === 7);
await shot("21-album-created");

// Playlist from the same files
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", tracks.slice(0, 5));
await settled();
await page.check('input[name="addmulti-mode"][value="playlist"]');
check("playlist hint text", (await page.textContent("#addmulti-mode-hint")).includes("Playlists/_<name>.noudata"));
await page.fill("#addmulti-name", "Evening");
await page.click('#addmulti-list li:nth-child(5) button[data-action="up"]');
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 20000 });
await page.waitForFunction(() => /^#\/playlists\/\d+$/.test(location.hash));
const plRel = "Playlists/_Evening.noudata";
check("playlist file written", walk(LIB).includes(plRel), walk(LIB).filter((f) => f.startsWith("Playlists")));
const plNames = mediaNames(readDoc(plRel)).map((n) => n.replace(/\.wav$/, ""));
// ("01 Alpha" was imported earlier through Add media, so this copy is uniquified, never overwritten)
check("playlist order 01,02,03,05,04", plNames.join() === "01 Alpha-1,02 Bravo,03 Charlie,05 Echo,04 Delta", plNames);
check("playlist tracks live in their own §4.4 folders", readDoc(plRel).mediaList.every((m) => m.localFilePaths[0].startsWith("../Music/")), readDoc(plRel).mediaList.map((m) => m.localFilePaths[0]));
await page.waitForSelector("#lib-view ol.items li.item");
check("playlist view shows 5 tracks", (await page.locator("#lib-view ol.items li.item").count()) === 5);
await shot("22-playlist-created");

// Partial failure: a fake .mp3 among good files
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", [tracks[5], path.join(AUDIO, "fake.mp3"), tracks[7]]);
await page.waitForSelector("#addmulti-skipfailed:not(.hidden)");
await page.fill("#addmulti-name", "Partial");
check("failed file reported, others continue", (await page.locator("li.multi-item.failed").count()) === 1 && (await page.locator("li.multi-item.done").count()) === 2);
check("failure message shows the server text", (await page.locator("li.multi-item.failed .multi-status").textContent()).includes("Failed:"));
check("OK offers a retry, dialog still open", (await page.textContent("#addmulti-ok")) === "Retry failed files" && (await page.evaluate(() => document.getElementById("addmulti-dialog").open)));
check("nothing created yet (collection only at finish)", !walk(LIB).some((f) => f.includes("Partial")), walk(LIB).filter((f) => f.includes("Partial")));
await shot("23-multi-partial-failure");
await page.click("#addmulti-ok"); // retry: fails again
await page.waitForSelector("#addmulti-skipfailed:not(.hidden)");
check("retry of the failed file fails again, still not created", (await page.locator("li.multi-item.failed").count()) === 1);
await page.click("#addmulti-skipfailed");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 20000 });
const partRel = "Music/The Band/Partial/_Partial.noudata";
check("continue without failed: album has the 2 good files", walk(LIB).includes(partRel) && mediaNames(readDoc(partRel)).length === 2, walk(LIB).filter((f) => f.includes("Partial")));

// Cancel with uploaded files: session discarded, nothing created
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", [tracks[0], path.join(AUDIO, "fake.mp3")]);
await page.fill("#addmulti-name", "Abandoned");
await page.waitForSelector("#addmulti-skipfailed:not(.hidden)");
check("a good file is staged on the device", tmpLeft().length === 1, tmpLeft());
await page.click("#addmulti-cancel");
check("cancel discards staged files", await until(() => tmpLeft().length === 0, 3000), tmpLeft());
check("cancel created nothing", !walk(LIB).some((f) => f.includes("Abandoned")));

// ===================================================================================
// FIX_01A: every intake batch is sorted naturally; "Sort by name" re-sorts everything
// ===================================================================================
const dropMulti = (names) =>
  page.evaluate((list) => {
    const dt = new DataTransfer();
    for (const n of list) dt.items.add(new File([new Uint8Array(10)], n, { type: "audio/wav" }));
    const z = document.getElementById("addmulti-drop");
    for (const type of ["dragenter", "dragover", "drop"]) {
      z.dispatchEvent(new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true }));
    }
  }, names);
await page.click("#addmulti-open-btn");
check("Sort by name is disabled with no rows", await page.locator("#addmulti-sort-btn").isDisabled());
await dropMulti(["10 Ten.wav", "9 Nine.wav", "11 Eleven.wav", "2 Two.wav"]);
await settled(); // (the 10-byte fakes are refused by the server: the rows stay, marked failed)
check("a reverse drop is listed naturally (2, 9, 10, 11)", (await order()).join() === "2 Two,9 Nine,10 Ten,11 Eleven", await order());
check("Sort by name is enabled with 2+ rows", !(await page.locator("#addmulti-sort-btn").isDisabled()));
await dropMulti(["3 Three.wav", "1 One.wav"]);
await settled();
check("a second drop is appended below the first batch, sorted among itself", (await order()).join() === "2 Two,9 Nine,10 Ten,11 Eleven,1 One,3 Three", await order());
await page.click("#addmulti-sort-btn");
check("Sort by name re-sorts the merged list", (await order()).join() === "1 One,2 Two,3 Three,9 Nine,10 Ten,11 Eleven", await order());
check("focus is back on the Sort button", await page.evaluate(() => document.activeElement.id === "addmulti-sort-btn"));
for (const name of ["Two", "One", "Three", "Eleven"]) {
  await page.click(`#addmulti-list li:has-text("${name}") button[data-action="remove"]`);
}
// six rows minus four leaves TWO (Nine, Ten): Sort is still enabled; one more removal disables it.
check("two rows left (Nine, Ten) after removing four of six", (await order()).join() === "9 Nine,10 Ten", await order());
check("Sort by name is still enabled with exactly two rows", !(await page.locator("#addmulti-sort-btn").isDisabled()));
await page.click('#addmulti-list li:has-text("Ten") button[data-action="remove"]');
check("Sort by name is disabled with fewer than two rows", await page.locator("#addmulti-sort-btn").isDisabled());
await page.click("#addmulti-cancel");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 5000 }).catch(() => {});
check("the sort-check dialog closed without creating anything", !(await page.evaluate(() => document.getElementById("addmulti-dialog").open)));

// The same order must reach the created `.noudata` (real uploads, reverse picker order).
const naturalDir = path.join(AUDIO, "natural");
fs.mkdirSync(naturalDir, { recursive: true });
fs.copyFileSync(tracks[0], path.join(naturalDir, "9 Nine.wav"));
fs.copyFileSync(tracks[1], path.join(naturalDir, "10 Ten.wav"));
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", [path.join(naturalDir, "10 Ten.wav"), path.join(naturalDir, "9 Nine.wav")]);
check("picker batch in reverse is listed 9 before 10", (await order()).join() === "9 Nine,10 Ten", await order());
await settled();
await page.fill("#addmulti-name", "Natural");
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 20000 });
const naturalRel = walk(LIB).find((f) => f.endsWith("/Natural/_Natural.noudata"));
check("the natural-order album was created", !!naturalRel, walk(LIB).filter((f) => f.includes("Natural")));
if (naturalRel) {
  const got = mediaNames(readDoc(naturalRel)).map((n) => n.replace(/\.wav$/, ""));
  check("the created .noudata lists 9 before 10", got.join() === "9 Nine,10 Ten", got);
}

// ===================================================================================
// Killing the browser mid-upload
// ===================================================================================
if (fs.existsSync(path.join(AUDIO, "big.wav"))) {
  const before = walk(LIB);
  const victim = await ctx.newPage();
  await victim.goto(BASE + "/");
  await victim.waitForSelector("#add-open-btn");
  await victim.click("#add-open-btn");
  await victim.setInputFiles("#add-file", path.join(AUDIO, "big.wav")); // uploads on drop
  // wait until the server has a partial file in its staging folder, then kill the page
  const partial = await until(() => tmpLeft().find((p) => p.endsWith(".part")), 15000, 5);
  check("upload in flight: a partial temp file exists", !!partial);
  await victim.close({ runBeforeUnload: false });
  check("after the page is killed the partial file is gone", await until(() => tmpLeft().length === 0, 20000, 100), tmpLeft());
  check("...and no media or .noudata appeared", JSON.stringify(walk(LIB)) === JSON.stringify(before), walk(LIB).filter((f) => !before.includes(f)));
  // the slot is free again
  const r = await fetch(BASE + "/api/media?kind=music&filename=after.wav", { method: "POST", headers: { "X-Plippa": "1" }, body: fs.readFileSync(path.join(AUDIO, "tracks/02 Bravo.wav")) });
  check("server still accepts uploads afterwards", r.status === 200, r.status);
}

// ===================================================================================
// NB_09 unite dialog, Case C (files + their own _<release>.noudata; offline): still works
// next to the new dialogs and files an album the NB_11 dialogs can see
// ===================================================================================
{
  const relDir = path.join(AUDIO, "caseC");
  fs.mkdirSync(relDir, { recursive: true });
  fs.copyFileSync(tracks[0], path.join(relDir, "01 Alpha.wav"));
  fs.copyFileSync(tracks[1], path.join(relDir, "02 Bravo.wav"));
  fs.writeFileSync(
    path.join(relDir, "_Case C Release.noudata"),
    JSON.stringify({
      noudataVersion: "0.4.0",
      playableItemTitle: "Case C Release <b>x</b>",
      noudataType: "release", playableItemProfile: "music", releaseData: { credits: { primaryArtists: [{ primaryArtistName: "Offline Band" }] } },
      mediaList: [
        { mediaTitle: "One", localFilePaths: ["01 Alpha.wav"] },
        { mediaTitle: "Two", localFilePaths: ["02 Bravo.wav"] },
      ],
    })
  );
  await page.setViewportSize({ width: 1200, height: 900 });
  await page.click("#unite-open-btn");
  await page.click('#unite-actions button:has-text("I already have the release folder")');
  await page.waitForSelector('#unite-body input[type="file"]');
  await page.setInputFiles('#unite-body input[type="file"]', [
    path.join(relDir, "_Case C Release.noudata"),
    path.join(relDir, "01 Alpha.wav"),
    path.join(relDir, "02 Bravo.wav"),
  ]);
  await page.click('#unite-actions button:has-text("Upload chosen files")');
  await until(async () => (await page.locator("#unite-body li").allTextContents()).filter((t) => t.includes("→")).length === 3, 10000, 100);
  check("Case C: three files staged via the unite dialog", (await page.locator("#unite-body li").allTextContents()).filter((t) => t.includes("→")).length === 3, await page.locator("#unite-body li").allTextContents());
  await page.click('#unite-actions button:has-text("Next: match files")');
  await page.waitForFunction(() => /Add to my library|Add to library|Apply|Add this/.test(document.getElementById("unite-actions").textContent), null, { timeout: 10000 }).catch(() => {});
  await shot("40-unite-case-c");
  const labels = await page.locator("#unite-actions button").allTextContents();
  const applyLabel = labels.find((l) => /add|apply|confirm/i.test(l) && !/close/i.test(l));
  check("Case C: proposal step reached (an apply button is offered)", !!applyLabel, labels);
  if (applyLabel) {
    await page.click(`#unite-actions button:has-text("${applyLabel}")`);
    await until(async () => (await page.textContent("#unite-title")).includes("Added"), 10000, 100);
    check("Case C: applied, offline, no network needed", (await page.textContent("#unite-title")).includes("Added to your library"), await page.textContent("#unite-title"));
    const caseC = walk(LIB).find((f) => f.endsWith(".noudata") && f.includes("Offline Band"));
    check("Case C: release .noudata filed under the artist", !!caseC, walk(LIB).filter((f) => f.includes("Offline")));
    await page.click('#unite-actions button:has-text("Close")');
    await page.goto(BASE + "/#/albums");
    await page.waitForSelector("#lib-view .card");
    check("Case C: the album is listed in the Albums tab; hostile title shown as text", (await page.locator("#lib-view .card", { hasText: "Case C Release <b>x</b>" }).count()) === 1 && (await page.locator("#lib-view .card b").count()) === 0);
  }
}

// ===================================================================================
// Phone width + a11y sanity
// ===================================================================================
await page.setViewportSize({ width: 390, height: 800 });
await page.click("#addmulti-open-btn");
await page.setInputFiles("#addmulti-file", tracks.slice(0, 3));
await settled();
await shot("30-multi-phone");
check("no horizontal scroll inside the multi dialog at 390 px", await page.evaluate(() => { const d = document.getElementById("addmulti-dialog"); return d.scrollWidth <= d.clientWidth + 1; }));
check("no horizontal page scroll at 390 px", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
check("dialogs are labelled", await page.evaluate(() => ["add-dialog", "addmulti-dialog"].every((id) => document.getElementById(document.getElementById(id).getAttribute("aria-labelledby")))));
check("progress bars have labels", await page.evaluate(() => ["add-progress", "addmulti-progress"].every((id) => document.getElementById(id).getAttribute("aria-label"))));
await page.click("#addmulti-cancel");

check("no alert() was triggered by hostile text", alerts === 0, alerts);
check("no console/page errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures ? `\n${failures} FAILED` : "\nALL OK");
process.exit(failures ? 1 : 0);
