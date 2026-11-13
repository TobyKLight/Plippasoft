// NB_16 browser test (Playwright + Chromium): "Edit timestamps" on existing items, stale-revision
// 409 path, hostile text inert, playlist / orphan states, "Create metadata" for an orphan file.
// Not part of `cargo test`. It starts its own portal_fixture server over a temp library.
//
//   cargo build -p plippa-server --example portal_fixture
//   LIB=/tmp/lib16 SHOTS=/tmp/shots16 node edittimes_flow.mjs        # prints PASS/FAIL, ALL OK
//
// (mkfix.py builds the fixture library; a real 5-minute WAV mixtape is added so the item's real
// length is known. The server runs on PORT, default 8816.)
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_nb16_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8816";
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

// ---- fixture library: mkfix + a real 5-minute WAV mixtape ---------------------------------
spawnSync("python3", [path.join(here, "mkfix.py"), LIB], { stdio: "inherit" });
function wav(secs) {
  const rate = 8000;
  const data = Buffer.alloc(rate * secs, 128);
  const h = Buffer.alloc(44);
  h.write("RIFF", 0);
  h.writeUInt32LE(36 + data.length, 4);
  h.write("WAVEfmt ", 8);
  h.writeUInt32LE(16, 16);
  h.writeUInt16LE(1, 20);
  h.writeUInt16LE(1, 22);
  h.writeUInt32LE(rate, 24);
  h.writeUInt32LE(rate, 28);
  h.writeUInt16LE(1, 32);
  h.writeUInt16LE(8, 34);
  h.write("data", 36);
  h.writeUInt32LE(data.length, 40);
  return Buffer.concat([h, data]);
}
fs.mkdirSync(path.join(LIB, "Mixes/Real"), { recursive: true });
fs.writeFileSync(path.join(LIB, "Mixes/Real/real.wav"), wav(300));
fs.writeFileSync(
  path.join(LIB, "Mixes/Real/_Real.noudata"),
  JSON.stringify({
    noudataVersion: "0.4.0",
    playableItemTitle: "Real Length Mix",
    noudataType: "release", playableItemProfile: "mix", releaseData: {},
    mediaList: [{ mediaTitle: "Real Length Mix", localFilePaths: ["real.wav"] }],
  })
);

const server = spawn(BIN, [LIB, PORT], { stdio: ["ignore", "ignore", "inherit"] });
process.on("exit", () => server.kill());
await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100);

const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));
const navCues = (doc, item = 0) => {
  const l = (doc.mediaList[item].cueLists || []).find((x) => x.cueListType === "nav");
  return l ? l.cues.map((c) => c.cueTextSpans.map((s) => s.text).join("")) : null;
};
const J = { "Content-Type": "application/json", "X-Plippa": "1" };
const collections = async (kind) => (await (await fetch(`${BASE}/api/collections?kind=${kind}`)).json()).collections;
const idOf = async (kind, title) => String((await collections(kind)).find((c) => c.title === title).id);

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
let alerts = 0;
page.on("dialog", async (d) => {
  alerts++;
  await d.dismiss();
});
const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png"), fullPage: false });
const dlgOpen = (id) => page.evaluate((i) => document.getElementById(i).open, id);
const toastText = () => page.textContent("#toast");
const previewCount = async () => (await page.locator("#cues-preview .preview-head strong").textContent()) || "";
const open = async (hash) => {
  await page.goto(BASE + "/" + hash);
  await page.waitForSelector("#lib-view .collection");
};
// NB_16: "Edit timestamps" lives in each item row's ⋮ menu — the collection header button is
// gone (a collection has no timestamps of its own). Opens the dialog for item `pos` and waits
// for the pre-fill.
const openCuesFromRow = async (pos = 0) => {
  await page.locator("#lib-view li.item").nth(pos).locator("button.kebab").click();
  await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
  await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
};

// ===================================================================================
// 1. Edit timestamps on the fixture mixtape (length unknown: empty placeholder file)
// ===================================================================================
const mixId = await idOf("mix", "Rocksteady Set");
await open(`#/mixes/${mixId}`);
check("no header Edit timestamps button (it lives in the row ⋮ menu)", (await page.locator("#coll-cues").count()) === 0 && (await page.locator("#coll-associate").count()) === 1);
await openCuesFromRow();
check("dialog opens modally", await dlgOpen("cues-dialog"));
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
const prefill = await page.inputValue("#cues-text");
check("box is pre-filled from GET /cues", prefill.includes("Warm up") && prefill.includes("Cool down") && prefill.trim().split("\n").length === 3, prefill);
check("hostile cue text is in the box as text", prefill.includes("Peak time <i>x</i>"));
check("subject shows title and unknown length", (await page.textContent("#cues-subject")).includes("Rocksteady Set"), await page.textContent("#cues-subject"));
await until(async () => (await previewCount()).startsWith("3"), 3000);
check("initial preview: 3 cues", (await previewCount()) === "3 cues", await previewCount());
check("preview cell shows the markup literally (no <i> element)", (await page.locator("#cues-preview i").count()) === 0 && (await page.locator("#cues-preview td", { hasText: "Peak time <i>x</i>" }).count()) === 1);
check("Save enabled, no loaded-note (nothing playing)", !(await page.locator("#cues-save").isDisabled()) && (await page.locator("#cues-loaded-note").isHidden()));
await shot("30-edit-dialog");

// edit -> live preview updates
await page.fill("#cues-text", "0:00 Start\nthis line is chatter\n12:30 - Middle part\n1:05:00 End");
await until(async () => (await previewCount()).startsWith("3") && (await page.locator("#cues-preview td", { hasText: "Middle part" }).count()) === 1, 3000);
check("preview follows the edit (3 cues, time formatted)", (await page.locator("#cues-preview td.cue-time").allTextContents()).join() === "00:00:00.000,00:12:30.000,01:05:00.000");
check("ignored line counted and listed", (await page.textContent("#cues-preview .preview-head")).includes("1 ignored line") && (await page.locator("#cues-preview details").count()) === 1);
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
check("toast reports the save", (await toastText()).includes("Saved 3"), await toastText());
check("saved into the .noudata (nav list, title kept)", JSON.stringify(navCues(readDoc("Mixes/Japanese/_Mix.noudata"))) === JSON.stringify(["Start", "Middle part", "End"]) && readDoc("Mixes/Japanese/_Mix.noudata").mediaList[0].cueLists[0].cueListTitle === "Chapters");
check("focus returned to the row ⋮ opener", await page.evaluate(() => document.activeElement.classList.contains("kebab")));
await page.waitForSelector('#lib-view .cues-toggle:has-text("Cues (3)")');
await page.click("#lib-view .cues-toggle");
await page.waitForSelector("#lib-view .cue-text");
check("collection view refreshed: new cues listed", (await page.locator("#lib-view .cue-text").allTextContents()).join() === "Start,Middle part,End", await page.locator("#lib-view .cue-text").allTextContents());

// reopen shows what was saved
await openCuesFromRow();
check("reopen: box shows the saved cues", (await page.inputValue("#cues-text")).includes("12:30") && (await page.inputValue("#cues-text")).includes("Middle part"), await page.inputValue("#cues-text"));

// ===================================================================================
// 2. Stale revision -> 409 message + Reload
// ===================================================================================
const g = await (await fetch(`${BASE}/api/items/${mixId}/cues`)).json();
const put = await fetch(`${BASE}/api/items/${mixId}/cues`, { method: "PUT", headers: J, body: JSON.stringify({ text: "0:00 Changed elsewhere", revision: g.revision }) });
check("external save (other tab/device) succeeded", put.ok);
await page.fill("#cues-text", "0:00 My late edit");
await page.click("#cues-save");
await page.waitForFunction(() => document.getElementById("cues-error").textContent.length > 0);
check("409 shows a clear message", (await page.textContent("#cues-error")).includes("changed since you opened"), await page.textContent("#cues-error"));
check("dialog stays open, user text kept, Reload offered", (await dlgOpen("cues-dialog")) && (await page.inputValue("#cues-text")) === "0:00 My late edit" && (await page.locator("#cues-reload").isVisible()));
check("nothing was overwritten on disk", JSON.stringify(navCues(readDoc("Mixes/Japanese/_Mix.noudata"))) === JSON.stringify(["Changed elsewhere"]));
await shot("31-stale");
await page.click("#cues-reload");
await page.waitForFunction(() => document.getElementById("cues-text").value.includes("Changed elsewhere"));
check("Reload replaces the box with the current version and clears the error", (await page.textContent("#cues-error")) === "" && (await page.locator("#cues-reload").isHidden()));
await page.fill("#cues-text", "0:00 Changed elsewhere\n20:00 Plus mine");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
check("after reload the save goes through", JSON.stringify(navCues(readDoc("Mixes/Japanese/_Mix.noudata"))) === JSON.stringify(["Changed elsewhere", "Plus mine"]));

// 2b. The file changed under the open dialog but NOT the cue list (this is what the engine's
// resume write-back does on pause/stop): Save must go through with the user's text, no 409 dialog.
await openCuesFromRow();
{
  const rel = path.join(LIB, "Mixes/Japanese/_Mix.noudata");
  const d = readDoc("Mixes/Japanese/_Mix.noudata");
  d.localPlaybackData = { ...(d.localPlaybackData || {}), userNotes: { createdByUser: "Resume writer" } };
  fs.writeFileSync(rel, JSON.stringify(d, null, 1));
}
await page.fill("#cues-text", "0:00 Changed elsewhere\n20:00 Plus mine\n30:00 Typed while the player wrote");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open, null, { timeout: 8000 }).catch(() => {});
check("file touched (not the cues) while the dialog was open: Save still goes through", !(await dlgOpen("cues-dialog")) && JSON.stringify(navCues(readDoc("Mixes/Japanese/_Mix.noudata"))) === JSON.stringify(["Changed elsewhere", "Plus mine", "Typed while the player wrote"]), await page.textContent("#cues-error"));
check("...and what the other writer put in the file is kept", readDoc("Mixes/Japanese/_Mix.noudata").localPlaybackData.userNotes.createdByUser === "Resume writer");

// ===================================================================================
// 3. Hostile text stays inert (box, preview, saved cue list)
// ===================================================================================
await openCuesFromRow();
await page.fill("#cues-text", "0:00 <img src=x onerror=alert(1)>\n1:00 <b onmouseover=alert(2)>bold</b>\n2:00 ‮evil​");
await until(async () => (await page.locator("#cues-preview td", { hasText: "<img" }).count()) === 1, 3000);
check("preview shows markup as text, no elements", (await page.locator("#cues-preview img, #cues-preview b").count()) === 0);
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
await page.click('#lib-view .cues-toggle');
await page.waitForSelector("#lib-view .cue-text");
check("saved cue list rendered as text, no elements", (await page.locator("#lib-view img, #lib-view .cue-text b").count()) === 0 && (await page.locator("#lib-view .cue-text", { hasText: "<img src=x onerror=alert(1)>" }).count()) === 1);
check("control/bidi/zero-width characters stripped on save", JSON.stringify(navCues(readDoc("Mixes/Japanese/_Mix.noudata"))) === JSON.stringify(["<img src=x onerror=alert(1)>", "<b onmouseover=alert(2)>bold</b>", "evil"]), navCues(readDoc("Mixes/Japanese/_Mix.noudata")));
check("no alert() ever ran", alerts === 0);
await sleep(300);
check("no broken-image request (no <img> in the page)", (await page.locator("img[src='x']").count()) === 0);

// ===================================================================================
// 4. Clearing removes the owned list
// ===================================================================================
await openCuesFromRow();
check("button label is Save while there is text", (await page.textContent("#cues-save")) === "Save");
await page.fill("#cues-text", "");
await until(async () => (await page.textContent("#cues-preview")).includes("remove the timestamps"), 3000);
check("empty box: preview says saving removes the timestamps; button says Remove", (await page.textContent("#cues-preview")).includes("remove the timestamps") && (await page.textContent("#cues-save")) === "Remove timestamps");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
check("toast: removed; no nav list left on disk", (await toastText()).includes("removed") && navCues(readDoc("Mixes/Japanese/_Mix.noudata")) === null, await toastText());
await page.waitForFunction(() => !document.querySelector("#lib-view .cues-toggle"));
check("collection view has no Cues toggle any more", true);

// ===================================================================================
// 5. Real length: warnings against the item's duration; loaded-on-player note
// ===================================================================================
const realId = await idOf("mix", "Real Length Mix");
await open(`#/mixes/${realId}`);
await page.click("#coll-play");
await page.waitForFunction((id) => document.querySelector("#lib-view li.item.playing") || document.querySelector(".now-playing") && /Real Length/.test(document.querySelector(".now-playing").textContent), realId, { timeout: 10000 }).catch(() => {});
await sleep(1500); // let a now-playing poll arrive
await openCuesFromRow();
check("subject shows the real length (5:00)", (await page.textContent("#cues-subject")).includes("length 5:00"), await page.textContent("#cues-subject"));
check("item is loaded on the player: note shown in the dialog", await page.locator("#cues-loaded-note").isVisible(), await page.textContent("#cues-loaded-note"));
check("...and it says cues apply on next load", (await page.textContent("#cues-loaded-note")).includes("next time it is loaded"));
await page.fill("#cues-text", "0:00 Intro\n04:00 Late one\n9:00 Past the end");
await until(async () => (await page.locator("#cues-preview .preview-warn li").count()) > 0, 3000);
check("past-the-end warning uses the real length; only 2 cues kept", (await previewCount()) === "2 cues" && (await page.textContent("#cues-preview .preview-warn")).length > 0, await page.textContent("#cues-preview"));
await shot("32-real-length");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
check("saved 2 cues (third dropped by the server)", JSON.stringify(navCues(readDoc("Mixes/Real/_Real.noudata"))) === JSON.stringify(["Intro", "Late one"]));
check("toast mentions a note", (await toastText()).includes("Saved 2") && (await toastText()).includes("note"), await toastText());
await page.waitForSelector("#coll-cue-note");
check("collection view shows the 'applies on next load' note", (await page.textContent("#coll-cue-note")).includes("next time it is loaded"));

// ===================================================================================
// 6. Several items in one release (audiobook with 2 parts); menu entry
// ===================================================================================
const hobbit = await idOf("audiobook", "The Hobbit");
await open(`#/audiobooks/${hobbit}`);
check(
  "multi-item release: no header button and no per-row button (FIX_01E)",
  (await page.locator("#coll-cues").count()) === 0 && (await page.locator("#lib-view .cue-edit-btn").count()) === 0
);
await page.locator("#lib-view li.item").nth(1).locator("button.kebab").click();
await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
check("subject names the item (2 of 2)", (await page.textContent("#cues-subject")).includes("item 2 of 2"), await page.textContent("#cues-subject"));
await page.fill("#cues-text", "0:00 Chapter A\n3:10 Chapter B");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
const hd = readDoc("Audiobooks/Tolkien/Hobbit/_Hobbit.noudata");
check("cues written to item 2 only", JSON.stringify(navCues(hd, 1)) === JSON.stringify(["Chapter A", "Chapter B"]) && navCues(hd, 0) === null);
// ⋮ menu path
await page.locator("#lib-view li.item").nth(0).locator("button.kebab").click();
check("⋮ menu has Edit timestamps enabled", (await page.locator('.menu [role="menuitem"]:has-text("Edit timestamps")').count()) === 1 && !(await page.locator('.menu [role="menuitem"]:has-text("Edit timestamps")').getAttribute("aria-disabled")));
await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
check("menu opens the dialog for item 1", (await page.textContent("#cues-subject")).includes("item 1 of 2"));
await page.keyboard.press("Escape");
check("Esc closes the dialog without saving", !(await dlgOpen("cues-dialog")) && navCues(readDoc("Audiobooks/Tolkien/Hobbit/_Hobbit.noudata"), 0) === null);

// ===================================================================================
// 7. Playlists: disabled with explanation
// ===================================================================================
const rainy = await idOf("playlist", "Rainy");
await open(`#/playlists/${rainy}`);
check("playlist: no header timestamps control (nothing of its own to timestamp)", (await page.locator("#coll-cues").count()) === 0);
await page.locator("#lib-view li.item").nth(0).locator("button.kebab").click();
const mi = page.locator('.menu [role="menuitem"]:has-text("Edit timestamps")');
check("playlist row ⋮: entry disabled with the explanation", (await mi.getAttribute("aria-disabled")) === "true" && (await mi.getAttribute("title")).includes("Playlists have no timestamps"));
await mi.click({ force: true });
check("...and does not open the dialog", !(await dlgOpen("cues-dialog")));
await page.keyboard.press("Escape");

// ===================================================================================
// 8. Orphan file: message, Create metadata, then timestamps
// ===================================================================================
const orphan = String((await collections("track")).find((c) => c.is_orphan).id);
await open(`#/tracks/${orphan}`);
check("orphan: Create metadata button and explanation, no Edit timestamps", (await page.locator("#coll-create-meta").count()) === 1 && (await page.locator("#coll-cues").count()) === 0 && (await page.textContent("#coll-orphan-note")).includes("no metadata"));
await shot("33-orphan");
// the ⋮ menu in the Tracks list
await page.goto(BASE + "/#/tracks");
await page.waitForSelector("#lib-view table.tracks tbody tr");
await page.locator(`#lib-view tr[data-id="${orphan}"] button.kebab`).click();
const om = page.locator('.menu [role="menuitem"]:has-text("Edit timestamps")');
check("orphan ⋮: Edit timestamps disabled, Create metadata offered", (await om.getAttribute("aria-disabled")) === "true" && (await om.getAttribute("title")).includes("no metadata") && (await page.locator('.menu [role="menuitem"]:has-text("Create metadata")').count()) === 1);
await page.keyboard.press("Escape");
await open(`#/tracks/${orphan}`);
await page.click("#coll-create-meta");
check("Create metadata dialog opens", await dlgOpen("meta-dialog"));
check("five kinds, Music default", (await page.locator('input[name="meta-kind"]').count()) === 5 && (await page.locator('input[name="meta-kind"][value="music"]').isChecked()));
await page.check('input[name="meta-kind"][value="mix"]');
await page.fill("#meta-title-input", "Loose Mix <b>x</b>");
await page.click("#meta-ok");
await page.waitForFunction(() => !document.getElementById("meta-dialog").open);
check("toast + no .noudata replaced: new file beside the audio", fs.existsSync(path.join(LIB, "Loose/orphan song.noudata")) && (await toastText()).includes("Created metadata"), await toastText());
const od = readDoc("Loose/orphan song.noudata");
check("file is a looseTrack (profile mix) with the chosen title and the audio as its single item", od.noudataType === "looseTrack" && od.playableItemProfile === "mix" && !od.releaseData && od.playableItemTitle === "Loose Mix <b>x</b>" && od.mediaList.length === 1 && od.mediaList[0].localFilePaths[0] === "orphan song.mp3", od);
await page.waitForFunction(() => /^#\/mixes\/\d+$/.test(location.hash));
check("portal moved to the new loose track, grouped under Mixes (kind track, group mix)", true);
await page.waitForFunction(() => document.getElementById("cues-dialog").open);
await page.waitForFunction(() => !document.getElementById("cues-text").disabled);
check("Edit timestamps opened right away for a long-form kind", true);
check("title rendered as text in the page", (await page.locator("#lib-view h2").textContent()) === "Loose Mix <b>x</b>" && (await page.locator("#lib-view h2 b").count()) === 0);
await page.fill("#cues-text", "0:00 Hello\n2:00 World");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
check("timestamps saved on the formerly orphan file", JSON.stringify(navCues(readDoc("Loose/orphan song.noudata"))) === JSON.stringify(["Hello", "World"]));
check("the orphan entry is gone from Tracks (no duplicate)", !(await collections("track")).some((c) => c.is_orphan));

// ===================================================================================
// 9. Phone width: nothing overflows with the dialog open
// ===================================================================================
await page.setViewportSize({ width: 390, height: 800 });
await open(`#/mixes/${mixId}`);
await openCuesFromRow();
await page.fill("#cues-text", "0:00 A very long title that keeps going and going and going and going to test wrapping\n1:00 B");
await sleep(600);
check("390 px: no horizontal page scroll", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
check("390 px: Save button at least 44 px high", (await page.locator("#cues-save").boundingBox()).height >= 44);
await shot("34-phone");
await page.keyboard.press("Escape");

check("no console errors / page errors", problems.filter((p) => !p.includes("status of 409") && !p.includes("status of 400")).length === 0, problems);
await browser.close();
console.log(failures ? `\n${failures} FAILED` : "\nALL OK");
process.exit(failures ? 1 : 0);
