// ART_07 browser test (Playwright + Chromium): cover-art preview in Add media / Add multiple media.
// Not part of `cargo test`. Starts its own portal_fixture server over a temp library.
//
//   python3 mkaudio.py /tmp/audio && cargo run -p plippa-server --example mkaudio_art -- /tmp/audio
//   cargo build -p plippa-server --example portal_fixture
//   AUDIO=/tmp/audio LIB=/tmp/lib2 SHOTS=/tmp/shots PORT=8872 BIN=<target>/debug/examples/portal_fixture node addmedia_art.mjs
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const AUDIO = process.env.AUDIO;
const LIB = process.env.LIB || "/tmp/plippa_art07_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8872";
const BASE = `http://127.0.0.1:${PORT}`;
const BIN = process.env.BIN || path.join(here, "../../../../target/debug/examples/portal_fixture");
const ART = path.join(AUDIO, "art");
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

const walk = (d, out = [], skipPlippa = true) => {
  for (const e of fs.existsSync(d) ? fs.readdirSync(d, { withFileTypes: true }) : []) {
    const p = path.join(d, e.name);
    if (skipPlippa && e.name === ".plippa") continue;
    e.isDirectory() ? walk(p, out, skipPlippa) : out.push(path.relative(LIB, p));
  }
  return out.sort();
};
const stagingDirs = () => {
  const d = path.join(LIB, ".plippa/tmp");
  return fs.existsSync(d) ? fs.readdirSync(d).filter((n) => n.startsWith("import-")) : [];
};
const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));
const f = (...p) => path.join(ART, ...p);
const dir = (d) => fs.readdirSync(f(d)).sort().map((n) => f(d, n));

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && !/status of (415|404)/.test(m.text()) && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
let alerts = 0;
page.on("dialog", async (d) => {
  if (d.type() === "confirm") return d.accept();
  alerts++;
  await d.dismiss();
});
const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png") });

// ---- helpers -----------------------------------------------------------------------------
const tiles = () => page.locator("#addmulti-art-list li.art-tile");
const settled = () =>
  page.waitForFunction(
    () =>
      !document.querySelector("#addmulti-list li.multi-item.pending, #addmulti-list li.multi-item.uploading") &&
      !document.getElementById("addmulti-drop").classList.contains("disabled"),
    null,
    { timeout: 30000 }
  );
const openMulti = async () => {
  await page.click("#addmulti-open-btn");
  await page.waitForFunction(() => document.getElementById("addmulti-dialog").open);
};
const dropMulti = async (files) => {
  await page.setInputFiles("#addmulti-file", files);
  await settled();
};
const closeMulti = async () => {
  await page.click("#addmulti-cancel");
  await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open);
};
const notes = async () => (await page.locator("#addmulti-art-notes").textContent()) || "";
const imageDocs = (rel) => (readDoc(rel).localPlaybackData?.localCoverImage?.images || []).map((i) => i.imageLocalFilename);
/** Natural width of the first tile's preview image as the browser decoded it. */
const tileWidths = () =>
  page.evaluate(async () => {
    const out = [];
    for (const li of document.querySelectorAll("#addmulti-art-list li.art-tile")) {
      const img = li.querySelector("img");
      const r = await fetch(img.src.split("?")[0]);
      const bmp = await createImageBitmap(await r.blob());
      out.push([bmp.width, bmp.height]);
    }
    return out;
  });

await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");

// ===== 1. 12 MP3s: 10 same front + 2 back -> exactly two tiles before OK ===============
await openMulti();
await dropMulti(dir("set12"));
check("12 rows uploaded", (await page.locator("#addmulti-list li.multi-item.done").count()) === 12);
check("exactly 2 cover tiles before OK", (await tiles().count()) === 2);
check("first tile is labelled Primary", (await tiles().first().locator(".art-pos").textContent()) === "Primary");
check("merge note shown (identical images merged)", /identical images merged/.test(await notes()), await notes());
check("tile images actually load (square thumb box has an <img>)", (await tiles().first().locator("img").count()) === 1 && (await page.waitForFunction(() => document.querySelector("#addmulti-art-list img")?.complete && document.querySelector("#addmulti-art-list img").naturalWidth > 0, null, { timeout: 5000 }).then(() => true).catch(() => false)));
check("name prefilled from the album tag", (await page.inputValue("#addmulti-name")) === "Twelve");
check("OK enabled (audio staged)", !(await page.locator("#addmulti-ok").isDisabled()));
await shot("40-art-two-tiles");
// reorder: second tile up -> the back cover becomes Primary (server order)
const before = await tiles().evaluateAll((l) => l.map((x) => x.dataset.id));
await page.click('#addmulti-art-list li.art-tile:nth-child(2) button[data-action="up"]');
await page.waitForFunction((b) => document.querySelector("#addmulti-art-list li.art-tile").dataset.id === b[1], before);
check("↑ reorders via PUT art/order, focus stays on the moved tile (its other arrow)", await page.waitForFunction((id) => document.activeElement.closest("li.art-tile")?.dataset.id === id, before[1], { timeout: 3000 }).then(() => true).catch(() => false));
// and back again by ↓ on the first
await page.click('#addmulti-art-list li.art-tile:nth-child(1) button[data-action="down"]');
await page.waitForFunction((b) => document.querySelector("#addmulti-art-list li.art-tile").dataset.id === b[0], before);
// drag and drop reorder
await page.dragAndDrop("#addmulti-art-list li.art-tile:nth-child(1)", "#addmulti-art-list li.art-tile:nth-child(2)");
await page.waitForFunction((b) => document.querySelector("#addmulti-art-list li.art-tile").dataset.id === b[1], before);
check("drag and drop reorders the tiles", true);
await page.click('#addmulti-art-list li.art-tile:nth-child(2) button[data-action="up"]');
await page.waitForFunction((b) => document.querySelector("#addmulti-art-list li.art-tile").dataset.id === b[0], before);
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 30000 });
const rel12 = walk(LIB).find((x) => x.endsWith("/Twelve/_Twelve.noudata"));
check("album created", !!rel12, walk(LIB).filter((x) => x.includes("Twelve")));
if (rel12) {
  const names = imageDocs(rel12);
  check("localCoverImage lists 2 images in the chosen order", names.length === 2, names);
  const folder = path.dirname(rel12);
  const files = walk(LIB).filter((x) => x.startsWith(folder + "/"));
  check("folder has cover.* and cover-2.*", files.some((x) => /\/cover\.(jpg|png|webp)$/.test(x)) && files.some((x) => /\/cover-2\.(jpg|png|webp)$/.test(x)), files);
  check("thumbnails/ tiers were generated", files.some((x) => x.includes("/thumbnails/")), files);
  check("localCoverImage order is cover then cover-2", /^cover\./.test(names[0]) && /^cover-2\./.test(names[1]), names);
}
check("no staging folder left after OK", stagingDirs().length === 0, stagingDirs());
// the new album shows its art in the portal (ART_05 component)
await page.waitForFunction(() => /^#\/albums\/\d+$/.test(location.hash));

// ===== 2. same cover at two sizes -> one tile (the larger) + merged note ================
await openMulti();
await dropMulti(dir("sizes"));
check("same cover at two sizes: 1 tile", (await tiles().count()) === 1);
check("a merged note appears", /merged/.test(await notes()), await notes());
const w2 = await tileWidths();
check("the surviving copy is the 1000 px one", w2.length === 1 && w2[0][0] === 1000, w2);
await closeMulti();

// ===== 3. audio + 2 dropped images; images only keeps OK disabled ======================
await openMulti();
await dropMulti([f("plain/p1.mp3"), f("loose/front.jpg"), f("loose/back.png")]);
check("audio + 2 image files: 2 tiles", (await tiles().count()) === 2);
check("dropped images are not listed as track rows", (await page.locator("#addmulti-list li.multi-item").count()) === 1);
await closeMulti();
await openMulti();
await dropMulti([f("loose/front.jpg"), f("loose/wide.png")]);
check("images only: 2 tiles shown", (await tiles().count()) === 2);
check("images only: OK stays disabled", await page.locator("#addmulti-ok").isDisabled());
check("images only: the hint explains why", /Add at least one audio file/.test((await page.textContent("#addmulti-hint")) || "") && (await page.locator("#addmulti-hint").isVisible()));
await page.fill("#addmulti-name", "Imgs");
check("...even with a name", await page.locator("#addmulti-ok").isDisabled());
await dropMulti([f("plain/p1.mp3")]);
check("adding an audio file enables OK", !(await page.locator("#addmulti-ok").isDisabled()));
check("...and hides the hint", await page.locator("#addmulti-hint").isHidden());
// removing the only audio row disables OK again and the art stays
await page.click('#addmulti-list li button[data-action="remove"]');
await page.waitForFunction(() => document.querySelectorAll("#addmulti-list li.multi-item").length === 0);
check("removing the last audio row disables OK again (art kept)", (await page.locator("#addmulti-ok").isDisabled()) && (await tiles().count()) === 2);
await closeMulti();

// ===== 4. ✕ on a tile leaves it out; a later track with the same art does not bring it back
await openMulti();
await dropMulti(dir("set12"));
const ids = await tiles().evaluateAll((l) => l.map((x) => x.dataset.id));
await page.click('#addmulti-art-list li.art-tile:nth-child(2) button[data-action="remove"]');
await page.waitForFunction(() => document.querySelectorAll("#addmulti-art-list li.art-tile").length === 1);
check("✕ removes the second tile", (await tiles().count()) === 1);
await dropMulti([f("plain/p2.mp3")]); // embeds the back cover that was just removed
check("a new track with the same embedded art does not bring it back", (await tiles().count()) === 1);
await page.fill("#addmulti-name", "OneArt");
await page.click("#addmulti-ok");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open, null, { timeout: 30000 });
const rel1 = walk(LIB).find((x) => x.endsWith("/OneArt/_OneArt.noudata"));
check("the album has exactly the one remaining image", !!rel1 && imageDocs(rel1).length === 1, rel1 && imageDocs(rel1));
check("the removed picture is not in the folder", !!rel1 && !walk(LIB).some((x) => x.startsWith(path.dirname(rel1) + "/") && /cover-2\./.test(x)));

// ===== 5. Cancel / Esc leaves nothing; the session is gone ===============================
const libBefore = walk(LIB);
await openMulti();
await dropMulti(dir("set12"));
const artUrl = await page.getAttribute("#addmulti-art-list img", "src");
check("staging exists while the dialog is open", stagingDirs().length === 1, stagingDirs());
await page.keyboard.press("Escape");
await page.waitForFunction(() => !document.getElementById("addmulti-dialog").open);
check("Esc: staging folder gone", await until(() => stagingDirs().length === 0, 5000), stagingDirs());
check("Esc: no new files in the library", JSON.stringify(walk(LIB)) === JSON.stringify(libBefore));
const gone = await fetch(BASE + artUrl.split("?")[0]);
check("the discarded session answers 404", gone.status === 404, gone.status);
await openMulti();
await dropMulti(dir("set12"));
await closeMulti();
check("Cancel button: staging gone, library unchanged", (await until(() => stagingDirs().length === 0, 5000)) && JSON.stringify(walk(LIB)) === JSON.stringify(libBefore));
// the single dialog too
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", f("solo/solo.mp3"));
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 1);
await page.keyboard.press("Escape");
check("single dialog Esc: staging gone, library unchanged", (await until(() => stagingDirs().length === 0, 5000)) && JSON.stringify(walk(LIB)) === JSON.stringify(libBefore));

// ===== 6. Single dialog ==================================================================
await page.click("#add-open-btn");
check("single: OK disabled until a file is staged", await page.locator("#add-ok").isDisabled());
await page.setInputFiles("#add-file", f("solo/solo.mp3"));
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 1);
check("single: one MP3 with art previews the art", true);
await page.waitForFunction(() => !document.getElementById("add-ok").disabled);
// an image dropped next to it joins the set; a second audio file replaces the first one
await page.setInputFiles("#add-file", f("loose/front.jpg"));
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 2);
check("single: a dropped image joins the art", true);
await page.click('#add-art-list li.art-tile:nth-child(2) button[data-action="remove"]');
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 1);
await page.click("#add-ok");
await page.waitForFunction(() => !document.getElementById("add-dialog").open, null, { timeout: 30000 });
const solo = walk(LIB).find((x) => x.endsWith(".noudata") && readDoc(x).playableItemTitle === "Solo Song");
check("single: a loose track was created", !!solo && readDoc(solo).noudataType === "looseTrack", solo);
check("single: the track has itemCoverImage", !!solo && (readDoc(solo).mediaList[0].itemCoverImage?.images || []).length === 1, solo && readDoc(solo).mediaList);
check("single: no POST /api/media was needed (no staging left)", stagingDirs().length === 0);
// cues still preview and apply
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", f("plain/p1.mp3"));
await page.waitForFunction(() => !document.getElementById("add-ok").disabled);
await page.check('input[name="add-kind"][value="mix"]');
await page.fill("#add-cue-text", "00:00 Intro\n00:00 Again\n");
await page.waitForSelector("#add-preview table tbody tr");
check("single: cue text previews", (await page.locator("#add-preview table tbody tr").count()) >= 1);
await page.fill("#add-cue-text", "00:00 Intro\n");
await page.waitForFunction(() => document.querySelectorAll("#add-preview table tbody tr").length === 1);
await page.click("#add-ok");
await page.waitForFunction(() => !document.getElementById("add-dialog").open, null, { timeout: 30000 });
check("single: cues applied on OK", /with 1 cue/.test(await page.textContent("#toast")), await page.textContent("#toast"));
// replacing the audio file keeps the dropped images
await page.click("#add-open-btn");
await page.setInputFiles("#add-file", [f("loose/front.jpg")]);
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 1);
check("single: image only: OK disabled + hint", (await page.locator("#add-ok").isDisabled()) && (await page.locator("#add-hint").isVisible()));
await page.setInputFiles("#add-file", f("plain/p1.mp3"));
await page.waitForFunction(() => !document.getElementById("add-ok").disabled);
await page.setInputFiles("#add-file", f("plain/p2.mp3"));
await page.waitForFunction(() => document.querySelectorAll("#add-art-list li.art-tile").length === 2);
check("single: replacing the audio file works and keeps the dropped image", (await page.textContent("#add-file-line")).includes("p2.mp3"));
await page.click("#add-cancel");

// ===== 7. Removing an uploaded audio row removes its unique art =========================
await openMulti();
await dropMulti([f("plain/p1.mp3"), f("plain/p2.mp3")]);
check("p2's back cover is previewed", (await tiles().count()) === 1);
await page.click('#addmulti-list li:has-text("p2.mp3") button[data-action="remove"]');
await page.waitForFunction(() => document.querySelectorAll("#addmulti-list li.multi-item").length === 1);
check("removing the row removed the art only it carried", await until(async () => (await tiles().count()) === 0, 5000));
check("...and the art section is hidden", await page.locator("#addmulti-art").isHidden());
await closeMulti();

// ===== 8. Other formats; unsupported / hostile ==========================================
await openMulti();
await dropMulti([f("formats/pic.flac"), f("formats/covr.m4a")]);
check("FLAC PICTURE and M4A covr (same picture) merge into 1 tile", (await tiles().count()) === 1, await tiles().count());
await closeMulti();
await openMulti();
await dropMulti([f("hostile/hostile.mp3")]);
check("embedded GIF is ignored with a note", /1 unsupported image ignored/.test(await notes()) && (await tiles().count()) === 0, await notes());
check("hostile tag text is inert (name field holds it as text, nothing injected)", (await page.inputValue("#addmulti-name")) === "<script>alert(2)</script>" && (await page.locator("#addmulti-dialog img[onerror], #addmulti-dialog script").count()) === 0);
await dropMulti([f("loose/notimage.jpg"), f("loose/gif.gif")]);
const nt = await notes();
check("a text file named .jpg and a GIF file are reported, not added", /notimage\.jpg/.test(nt) && /gif\.gif/.test(nt) && (await tiles().count()) === 0, nt);
check("an unusable image does not disable the dialog", !(await page.locator("#addmulti-pick-btn").isDisabled()));
await closeMulti();

// ===== 9. Phone width ====================================================================
await page.setViewportSize({ width: 390, height: 800 });
await openMulti();
await dropMulti(dir("set12").slice(0, 2).concat([f("loose/front.jpg"), f("loose/back.png"), f("loose/wide.png")]));
check("phone: 4+ tiles", (await tiles().count()) >= 4, await tiles().count());
await shot("41-art-phone");
check("phone: no horizontal scroll in the dialog", await page.evaluate(() => { const d = document.getElementById("addmulti-dialog"); return d.scrollWidth <= d.clientWidth + 1; }));
check("phone: no horizontal page scroll", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
const tops = await tiles().evaluateAll((l) => l.map((x) => Math.round(x.getBoundingClientRect().top)));
check("phone: the tile grid wraps", new Set(tops).size >= 2 && Math.min(...tops) < Math.max(...tops), tops);
check("phone: tile buttons are touch sized", await page.evaluate(() => [...document.querySelectorAll("#addmulti-art-list button")].every((b) => b.getBoundingClientRect().height >= 40)));
await closeMulti();

check("no alert() was triggered by hostile text", alerts === 0, alerts);
check("no console/page errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures ? `\n${failures} FAILED` : "\nALL OK");
process.exit(failures ? 1 : 0);
