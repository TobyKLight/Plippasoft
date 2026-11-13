// BCS_10 browser test (Playwright + Chromium): the "Delete" dialog (preview then confirm).
// Not part of `cargo test`; starts its own portal_fixture over a fresh mkfix.py library.
//
//   cargo build -p plippa-server --example portal_fixture
//   PORT=8876 LIB=/tmp/lib_del SHOTS=/tmp/shots_del node delete_flow.mjs    # PASS/FAIL, ALL OK
//
// Env: BIN (default $CARGO_TARGET_DIR/debug/examples/portal_fixture), PLAYWRIGHT_DIR.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_del_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8876";
const BASE = `http://127.0.0.1:${PORT}`;
const BIN =
  process.env.BIN ||
  path.join(process.env.CARGO_TARGET_DIR || path.join(here, "../../../../target"), "debug/examples/portal_fixture");
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
if (!(await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100))) {
  console.log("FAIL  server did not start");
  process.exit(1);
}
const getJson = (p) => fetch(BASE + p).then((r) => r.json());
const idOf = async (kind, title) => {
  const l = await getJson(`/api/collections?kind=${kind}`);
  return l.collections.find((c) => c.title === title).id;
};
const album = await idOf("album", "The Album");
const hostile = await idOf("album", "<img src=x onerror=alert(1)>");
const hobbit = await idOf("audiobook", "The Hobbit");
const rainy = await idOf("playlist", "Rainy");

// ---- disk helpers ------------------------------------------------------------------------------
/** Every file outside .plippa/ + thumbnails (generated when art is shown): path:size. */
function listing(dir) {
  const out = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      if (e.name === "thumbnails" || e.name === ".plippa") continue;
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else out.push(path.relative(LIB, p) + ":" + fs.statSync(p).size);
    }
  };
  walk(dir);
  return out.sort().join("\n");
}
const abs = (...p) => path.join(LIB, ...p);
const P = { album: "Music/Band/Album", hobbit: "Audiobooks/Tolkien/Hobbit", hostile: "Music/Evil/Hostile" };

const browser = await chromium.launch();
const problems = [];
async function newPage(opts = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 }, ...opts });
  const page = await ctx.newPage();
  page.on("console", (m) => m.type() === "error" && !/Failed to load resource/.test(m.text()) && problems.push("console: " + m.text()));
  page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
  return { ctx, page };
}

const dlgOpen = (page) => page.locator("#delete-dialog[open]");
const loaded = (page) =>
  page.waitForFunction(() => {
    const loading = document.querySelector("#delete-loading");
    return loading.classList.contains("hidden") || document.querySelector("#delete-error").textContent.length > 0;
  });
const preview = (page) =>
  page.evaluate(() => ({
    title: document.querySelector("#delete-title").textContent,
    subject: document.querySelector("#delete-subject").textContent,
    summary: document.querySelector("#delete-summary").textContent,
    files: [...document.querySelectorAll("#delete-files li")].map((li) => li.textContent),
    lists: [...document.querySelectorAll("#delete-lists > *")].map((n) => n.textContent),
    blockers: [...document.querySelectorAll("#delete-blockers li")].map((li) => li.textContent),
    warnings: [...document.querySelectorAll("#delete-warnings li")].map((li) => li.textContent),
    okDisabled: document.querySelector("#delete-ok").disabled,
    focus: document.activeElement && document.activeElement.id,
  }));
async function cardMenu(page, id, tab = "albums") {
  await page.goto(BASE + `/#/${tab}`);
  await page.waitForSelector(`.card[data-id="${id}"]`);
  await page.click(`.card-cell:has(.card[data-id="${id}"]) .card-menu`);
  return page.locator('.menu [role="menuitem"]').allTextContents();
}

// ---- 1. the preview comes first; Cancel writes nothing ------------------------------------------
{
  const { ctx, page } = await newPage();
  const snap = listing(LIB);
  const items = await cardMenu(page, album);
  check("album card ⋮ menu offers Delete…", items.includes("Delete…"), items);
  await page.click('.menu [role="menuitem"]:has-text("Delete")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  const p = await preview(page);
  check("the dialog names the item", p.title === "Delete this item?" && p.subject === "The Album", p);
  check("the summary counts the files and the size", /^\d+ files · /.test(p.summary), p.summary);
  check(
    "every media file and the cover are listed",
    p.files.length >= 5 &&
      ["01 - One.mp3", "02 - Two.mp3", "03 - Three.mp3", "cover.png"].every((f) => p.files.some((x) => x.includes(f))),
    p.files
  );
  check("the .noudata and the folder are named", p.lists.some((l) => /_Album\.noudata/.test(l)) && p.lists.some((l) => /Music\/Band\/Album.*becomes empty/.test(l)), p.lists);
  check("the playlist that loses tracks is named", p.lists.some((l) => /playlist will lose the deleted tracks: Rainy/.test(l)), p.lists);
  check("the Noubin key that is released is named", p.lists.some((l) => /album\.noubin\.aaa111/.test(l)), p.lists);
  check("no blockers: Delete is enabled", p.blockers.length === 0 && !p.okDisabled, p);
  check("Cancel has the focus, not Delete", p.focus === "delete-cancel", p.focus);
  check("nothing was on disk changed by the preview", listing(LIB) === snap);
  await page.screenshot({ path: path.join(SHOTS, "preview.png") });
  await page.click("#delete-cancel");
  await page.waitForFunction(() => !document.querySelector("#delete-dialog").open);
  check("Cancel after a preview deletes nothing", listing(LIB) === snap);
  const back = await page.evaluate(() => document.activeElement && document.activeElement.className);
  check("focus returns to the ⋮ opener", /card-menu/.test(back), back);

  // Esc behaves the same way
  await page.click(`.card-cell:has(.card[data-id="${album}"]) .card-menu`);
  await page.click('.menu [role="menuitem"]:has-text("Delete")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector("#delete-dialog").open);
  check("Esc cancels without deleting", listing(LIB) === snap);
  await ctx.close();
}

// ---- 2. the details panel opens the same dialog --------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/albums/${album}`);
  await page.waitForSelector("#coll-details");
  await page.click("#coll-details");
  await page.waitForSelector("#detail-delete-btn");
  await page.click("#detail-delete-btn");
  await dlgOpen(page).waitFor();
  await loaded(page);
  const p = await preview(page);
  check("details panel Delete… shows the same preview", p.subject === "The Album" && p.files.length >= 5, p);
  await page.click("#delete-cancel");
  await page.waitForFunction(() => !document.querySelector("#delete-dialog").open);
  check("focus returns to the details-panel button", await page.evaluate(() => document.activeElement.id === "detail-delete-btn"));
  await ctx.close();
}

// ---- 3. confirm: folder, index rows, playlist and key all go ---------------------------------------
{
  const { ctx, page } = await newPage();
  let asked = 0;
  page.on("dialog", (d) => {
    asked++;
    d.accept();
  });
  await cardMenu(page, album);
  await page.click('.menu [role="menuitem"]:has-text("Delete")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  await page.click("#delete-ok");
  await page.waitForFunction(() => !document.querySelector("#delete-dialog").open);
  await page.waitForSelector("#toast:not(.hidden)");
  const toast = await page.textContent("#toast");
  check("the toast reports how much went", /^Deleted \d+ files, /.test(toast), toast);
  check("no extra window.confirm for a destructive action that has a dialog", asked === 0, asked);
  check("the release folder is gone", !fs.existsSync(abs(P.album)));
  check("the card is gone from the list", (await until(async () => (await page.locator(`.card[data-id="${album}"]`).count()) === 0)));
  const gone = await getJson(`/api/playables/${album}`).then((r) => r.error || null).catch(() => "error");
  check("the API no longer knows the id", gone !== null, gone);
  const pl = await getJson(`/api/collections/${rainy}`);
  check("the playlist survives, valid, with the deleted entries removed", pl.title === "Rainy" && pl.item_count === 0, pl.item_count);
  const key = await getJson(`/api/playables`).then((r) => JSON.stringify(r));
  check("no row keeps the released Noubin key", !key.includes("album.noubin.aaa111"));
  await ctx.close();
}

// ---- 4. one media file of a release ---------------------------------------------------------------
{
  // A release with a single media file offers no single-file delete: it is the last one.
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/albums/${hostile}`);
  await page.waitForSelector("#lib-view li.item");
  const items = await page.locator("#lib-view li.item").first().locator(".kebab").click().then(() => page.locator('.menu [role="menuitem"]').allTextContents());
  check("a single-file release offers no 'Delete this file…' (it is the last one)", !items.includes("Delete this file…"), items);
  await page.keyboard.press("Escape");
  await ctx.close();
}
{
  // The Hobbit has 2 parts: delete the first one only (The Album is deleted further down).
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/audiobooks/${hobbit}`);
  await page.waitForSelector("#lib-view li.item");
  const items = await page.locator("#lib-view li.item").first().locator(".kebab").click().then(() => page.locator('.menu [role="menuitem"]').allTextContents());
  check("an item ⋮ menu offers Delete this file…", items.includes("Delete this file…"), items);
  await page.click('.menu [role="menuitem"]:has-text("Delete this file")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  const p = await preview(page);
  check("the preview is about that one file", p.title === "Delete this file?" && p.files.length === 1 && /hobbit-1\.m4b/.test(p.files[0]), p);
  check("the playlist note is absent for a single file", p.lists.every((l) => !/playlist will lose/.test(l)), p.lists);
  await page.click("#delete-ok");
  await page.waitForFunction(() => !document.querySelector("#delete-dialog").open);
  check("one file is gone, the other stays", !fs.existsSync(abs(P.hobbit, "hobbit-1.m4b")) && fs.existsSync(abs(P.hobbit, "hobbit-2.m4b")));
  const nd = JSON.parse(fs.readFileSync(abs(P.hobbit, "_Hobbit.noudata"), "utf8"));
  check("the .noudata lost exactly that entry", nd.mediaList.length === 1 && nd.mediaList[0].mediaTitle === "Part 2", nd.mediaList.map((m) => m.mediaTitle));
  const back = await getJson(`/api/collections/${hobbit}`);
  check("the release stays in the list with 1 item", back.item_count === 1 && back.items[0].available, back.item_count);
  await ctx.close();
}

// ---- 5. a playlist is never deleted from here ------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/playlists/${rainy}`);
  await page.waitForSelector("#lib-view .coll-actions .kebab");
  await page.click("#lib-view .coll-actions .kebab");
  const items = await page.locator('.menu [role="menuitem"]').allTextContents();
  check("a playlist's ⋮ menu has no Delete… (it has Delete playlist in edit mode)", !items.includes("Delete…"), items);
  await page.keyboard.press("Escape");
  await ctx.close();
}

// ---- 6. hostile metadata: the server's text stays text ----------------------------------------------
{
  const { ctx, page } = await newPage();
  await cardMenu(page, hostile);
  await page.click('.menu [role="menuitem"]:has-text("Delete")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  const p = await preview(page);
  check("the hostile title is shown as text", p.subject === "<img src=x onerror=alert(1)>", p.subject);
  check("nothing in the dialog parsed it as markup", (await page.locator("#delete-dialog img, #delete-dialog b, #delete-dialog script").count()) === 0);
  const files = p.files.join(" ");
  check("the file paths are shown in code elements", (await page.locator("#delete-files li code").count()) === p.files.length, files);
  await page.click("#delete-cancel");
  await ctx.close();
}

// ---- 7. phone width --------------------------------------------------------------------------------
{
  const { ctx, page } = await newPage({ viewport: { width: 390, height: 800 }, hasTouch: true });
  await cardMenu(page, album);
  await page.click('.menu [role="menuitem"]:has-text("Delete")');
  await dlgOpen(page).waitFor();
  await loaded(page);
  await sleep(250);
  const m = await page.evaluate(() => {
    const d = document.getElementById("delete-dialog").getBoundingClientRect();
    const small = [...document.querySelectorAll("#delete-dialog button")].filter((b) => b.offsetParent && b.getBoundingClientRect().height < 43.5).map((b) => b.textContent + ":" + b.getBoundingClientRect().height);
    return { sw: document.documentElement.scrollWidth, iw: innerWidth, left: d.left, right: d.right, small, dsw: document.getElementById("delete-dialog").scrollWidth, dcw: document.getElementById("delete-dialog").clientWidth };
  });
  check("390 px: the dialog fits, no horizontal scroll", m.sw <= m.iw && m.left >= 0 && m.right <= m.iw && m.dsw <= m.dcw, m);
  check("390 px: every dialog button is at least 44 px high", m.small.length === 0, m.small);
  await page.screenshot({ path: path.join(SHOTS, "phone.png") });
  await page.click("#delete-cancel");
  await ctx.close();
}

check("no console errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures === 0 ? "ALL OK" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
