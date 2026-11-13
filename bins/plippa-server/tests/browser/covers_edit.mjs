// ART_06 browser test (Playwright + Chromium): the "Edit cover art" dialog.
// Not part of `cargo test`; starts its own portal_fixture over a fresh mkfix.py --covers library.
//
//   cargo build -p plippa-server --example portal_fixture
//   PORT=8874 LIB=/tmp/lib_covedit SHOTS=/tmp/shots_covedit node covers_edit.mjs    # PASS/FAIL, ALL OK
//
// Env: BIN (default $CARGO_TARGET_DIR/debug/examples/portal_fixture), PLAYWRIGHT_DIR.
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_covedit_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8874";
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

/** A real PNG of the given shape. */
function png(width, height, [r, g, b]) {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const x of buf) c = crcTable[(c ^ x) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const head = Buffer.alloc(4);
    head.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type), data]);
    const tail = Buffer.alloc(4);
    tail.writeUInt32BE(crc(body));
    return Buffer.concat([head, body, tail]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 2;
  const row = Buffer.concat([Buffer.from([0]), Buffer.from(Array.from({ length: width }, () => [r, g, b]).flat())]);
  const raw = Buffer.concat(Array.from({ length: height }, () => row));
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

spawnSync("python3", [path.join(here, "mkfix.py"), LIB, "--covers"], { stdio: "inherit" });
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
const trip = await idOf("album", "Triptych");
const single = await idOf("album", "Single Cover");
const bare = await idOf("album", "Bare Album");
const theAlbum = await idOf("album", "The Album");
const rainy = await idOf("playlist", "Rainy");
const demo = (await getJson("/api/tracks?view=track")).tracks.find((t) => t.title === "Tagless Demo").id;

// ---- disk helpers ------------------------------------------------------------------------------
const P = {
  trip: "Music/Trip/Triptych",
  single: "Music/Solo/Single",
  rainy: "Playlists",
  demo: "Loose",
};
const abs = (...p) => path.join(LIB, ...p);
const noudata = (rel) => JSON.parse(fs.readFileSync(abs(rel), "utf8"));
const tripNd = () => noudata(`${P.trip}/_Triptych.noudata`);
const singleNd = () => noudata(`${P.single}/_Single.noudata`);
const rainyNd = () => noudata(`${P.rainy}/_Rainy.noudata`);
const demoNd = () => noudata(`${P.demo}/tagless demo.noudata`);
const names = (c) => ((c && c.images) || []).map((i) => i.imageLocalFilename);
const listing = (dir) => {
  const out = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      // thumbnails are generated in the background whenever a cover is shown; .plippa is the index
      if (e.name === "thumbnails" || e.name === ".plippa") continue;
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else out.push(path.relative(LIB, p) + ":" + fs.statSync(p).size + ":" + fs.statSync(p).mtimeMs);
    }
  };
  walk(dir);
  return out.sort().join("\n");
};
const dirFiles = (rel) => (fs.existsSync(abs(rel)) ? fs.readdirSync(abs(rel)).sort() : []);

const browser = await chromium.launch();
const problems = [];
async function newPage(opts = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 }, ...opts });
  const page = await ctx.newPage();
  page.on("console", (m) => m.type() === "error" && !/Failed to load resource/.test(m.text()) && problems.push("console: " + m.text()));
  page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
  return { ctx, page };
}

const tiles = (page) =>
  page.evaluate(() =>
    [...document.querySelectorAll("#covers-grid li.cover-tile")].map((li) => {
      const img = li.querySelector("img");
      const src = img ? img.getAttribute("src") : "";
      return {
        n: (/\/covers\/(\d+)/.exec(src) || [])[1] ?? null,
        staged: /covers\/staged\//.test(src),
        primary: !!li.querySelector(".tile-badge.primary"),
        isNew: !!li.querySelector(".tile-badge.new"),
        failed: li.classList.contains("failed"),
        label: li.getAttribute("aria-label"),
        msg: li.querySelector(".tile-msg").textContent,
      };
    }),
  );
const order = async (page) => (await tiles(page)).map((t) => (t.staged ? "S" : t.n)).join(",");
const dlgOpen = (page) => page.locator("#covers-dialog[open]");
async function openFromCard(page, id, tab = "albums") {
  await page.goto(BASE + `/#/${tab}`);
  await page.waitForSelector(`.card[data-id="${id}"]`);
  await page.click(`.card-cell:has(.card[data-id="${id}"]) .card-menu`);
  await page.click('.menu [role="menuitem"]:has-text("Edit cover art")');
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile, #covers-empty:not(.hidden)");
}
async function saveAndWait(page) {
  await page.waitForFunction(() => !document.querySelector("#covers-save").disabled);
  await page.click("#covers-save");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
}

// ---- 1. entry points -----------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, trip);
  const fromCard = await tiles(page);
  check("card ⋮ menu: 'Edit cover art…' opens the dialog with the 3 current images in order", fromCard.length === 3 && (await order(page)) === "0,1,2", fromCard);
  check("the title names the collection", (await page.textContent("#covers-title")) === "Cover art — Triptych");
  check("the first tile is marked Primary, only that one", fromCard.map((t) => t.primary).join() === "true,false,false");
  check("tiles are in a list with positional aria-labels", fromCard[1].label === "Cover art 2 of 3");
  const sq = await page.evaluate(() => [...document.querySelectorAll("#covers-grid .cover-box")].map((b) => b.getBoundingClientRect()).map((r) => [Math.round(r.width), Math.round(r.height)]));
  check("tiles are square thumbnails", sq.length === 3 && sq.every(([w, h]) => Math.abs(w - h) <= 1 && w > 60), sq);
  check("focus is inside the dialog", await page.evaluate(() => document.querySelector("#covers-dialog").contains(document.activeElement)));
  check("Save is disabled while nothing changed", await page.locator("#covers-save").isDisabled());
  await page.screenshot({ path: path.join(SHOTS, "dialog.png") });
  await page.click("#covers-cancel");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
  const back = await page.evaluate(() => document.activeElement && document.activeElement.className);
  check("Cancel with no changes closes without asking; focus returns to the ⋮ opener", /card-menu/.test(back), back);

  // collection header ⋮
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .coll-actions .kebab");
  await page.click("#lib-view .coll-actions .kebab");
  const items = await page.locator('.menu [role="menuitem"]').allTextContents();
  check("collection header ⋮ menu offers Edit cover art…", items.includes("Edit cover art…"), items);
  await page.click('.menu [role="menuitem"]:has-text("Edit cover art")');
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile");
  check("header ⋮ menu: same dialog, same tiles", (await order(page)) === "0,1,2");
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);

  // details panel
  await page.click("#coll-details");
  await page.waitForSelector("#detail-covers-btn:not(.hidden)");
  await page.click("#detail-covers-btn");
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile");
  check("details panel button: same dialog, same tiles", (await order(page)) === "0,1,2" && (await page.textContent("#covers-title")) === "Cover art — Triptych");
  await page.click("#covers-cancel");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
  check("focus returns to the details-panel button", await page.evaluate(() => document.activeElement.id === "detail-covers-btn"));

  // loose track row ⋮ and an album track row (no entry)
  await page.goto(BASE + `/#/tracks`);
  await page.waitForSelector(`tr[data-id="${demo}"]`);
  await page.click(`tr[data-id="${demo}"] .kebab`);
  const rowItems = await page.locator('.menu [role="menuitem"]').allTextContents();
  check("loose track row ⋮ menu offers Edit cover art… (last item)", rowItems[rowItems.length - 1] === "Edit cover art…", rowItems);
  await page.keyboard.press("Escape");
  const multiRow = page.locator(`tr:has-text("One")`).first();
  await multiRow.locator(".kebab").click();
  const multiItems = await page.locator('.menu [role="menuitem"]').allTextContents();
  check("a track inside an album has no Edit cover art… (out of scope)", !multiItems.includes("Edit cover art…"), multiItems);
  await page.keyboard.press("Escape");
  await ctx.close();
}

// ---- 2. reorder with the buttons; save; persisted ---------------------------------------------------
{
  const { ctx, page } = await newPage();
  const before = tripNd();
  const filesBefore = dirFiles(P.trip);
  await openFromCard(page, trip);
  // [wide, front, big] -> move the 3rd to the front: big, wide, front
  await page.locator("#covers-grid li.cover-tile").nth(2).locator("button", { hasText: "←" }).click();
  await page.locator("#covers-grid li.cover-tile").nth(1).locator("button", { hasText: "←" }).click();
  check("buttons reorder: [big, wide, front] and Primary follows", (await order(page)) === "2,0,1" && (await tiles(page))[0].primary);
  check("focus stays on the moved tile's button", await page.evaluate(() => document.activeElement.closest("li.cover-tile") === document.querySelector("#covers-grid li.cover-tile")));
  const edge = await page.evaluate(() => {
    const lis = [...document.querySelectorAll("#covers-grid li.cover-tile")];
    const b = (li, t) => [...li.querySelectorAll(".tile-actions button")].find((x) => x.textContent === t);
    return [b(lis[0], "←").disabled, b(lis[2], "→").disabled, b(lis[1], "←").disabled];
  });
  check("first tile cannot move earlier, last cannot move later", edge[0] && edge[1] && !edge[2], edge);
  check("Save is now enabled", await page.locator("#covers-save").isEnabled());
  const lab = (await tiles(page))[1].label;
  check("buttons carry position in their aria-label", await page.locator(`#covers-grid li >> nth=1 >> button[aria-label^="${lab}, move"]`).count() === 2, lab);
  await saveAndWait(page);
  await page.waitForSelector("#toast:not(.hidden)");
  check("toast says Cover art updated", (await page.textContent("#toast")) === "Cover art updated");
  const after = tripNd();
  const loc = names(after.localPlaybackData && after.localPlaybackData.localCoverImage);
  check("localCoverImage lists the new order, the same files", loc.join() === "big.png,wide.png,front.png", loc);
  check("artist coverImage untouched", JSON.stringify(after.releaseData.coverImage) === JSON.stringify(before.releaseData.coverImage));
  check("no file renamed or created", dirFiles(P.trip).join() === filesBefore.filter((f) => f !== "").join() || dirFiles(P.trip).filter((f) => !f.startsWith("thumbnails")).join() === filesBefore.filter((f) => !f.startsWith("thumbnails")).join(), { now: dirFiles(P.trip), before: filesBefore });
  // the card thumbnail is the new first image; reload and verify
  await page.reload();
  const covers = await getJson(`/api/collections/${trip}/covers`);
  check("API: order persisted, first image is big.png (2000x1000)", covers.covers[0].w === 2000 && covers.covers[0].h === 1000, covers.covers[0]);
  await page.goto(BASE + "/#/albums");
  await page.waitForSelector(`.card[data-id="${trip}"] .card-cover img`);
  const stale = await page.evaluate(async (id) => {
    // what the page would get from its HTTP cache vs. what the server serves right now
    const src = document.querySelector(`.card[data-id="${id}"] .card-cover img`).getAttribute("src");
    const cached = await (await fetch(src)).arrayBuffer();
    const fresh = await (await fetch(src, { cache: "no-store" })).arrayBuffer();
    return { src, cached: cached.byteLength, fresh: fresh.byteLength };
  }, trip);
  check("the browser's cached card thumbnail is the new first image, not the stale one", stale.cached === stale.fresh && stale.fresh > 0, stale);
  await ctx.close();
}

// ---- 3. reorder with drag ------------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, trip);
  const start = await order(page); // 0,1,2 of [big, wide, front]
  await page.locator("#covers-grid li.cover-tile").nth(0).dragTo(page.locator("#covers-grid li.cover-tile").nth(2));
  const now = await order(page);
  check("dragging the first tile onto the last moves it there", start === "0,1,2" && now === "1,2,0", { start, now });
  check("drag leaves no stray drag classes", (await page.locator("#covers-grid .dragging, #covers-grid .drop-target").count()) === 0);
  await saveAndWait(page);
  const loc = names(tripNd().localPlaybackData.localCoverImage);
  check("drag order persisted: [wide, front, big]", loc.join() === "wide.png,front.png,big.png", loc);
  await ctx.close();
}

// ---- 4. delete: an artist image is hidden, never deleted ------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, theAlbum);
  const before = noudata("Music/Band/Album/_Album.noudata");
  check("no hide-note yet", (await page.locator("#covers-note.hidden").count()) === 1 && (await tiles(page)).length === 1);
  await page.locator('#covers-grid button[aria-label="Remove cover art 1"]').click();
  const note = await page.textContent("#covers-note");
  check("deleting an artist image shows the 'hides, file stays' note", /hides the artist's image; the file stays on the stick/.test(note) && (await page.locator("#covers-note:not(.hidden)").count()) === 1, note);
  check("the tile disappears immediately", (await tiles(page)).length === 0 && (await page.locator("#covers-empty:not(.hidden)").count()) === 1);
  await saveAndWait(page);
  const nd = noudata("Music/Band/Album/_Album.noudata");
  check("localCoverImage is now empty (hiding, not deleting)", names(nd.localPlaybackData && nd.localPlaybackData.localCoverImage).length === 0);
  check("the artist's file cover.png is still on disk", fs.existsSync(abs("Music/Band/Album", "cover.png")));
  check("artist coverImage untouched", JSON.stringify(nd.releaseData.coverImage) === JSON.stringify(before.releaseData.coverImage));
  const api = await getJson(`/api/collections/${theAlbum}/covers`);
  check("the portal no longer shows it", api.covers.length === 0, api.covers.length);
  const ph = await until(async () => (await page.locator(`.card[data-id="${theAlbum}"] .card-cover.nocover`).count()) === 1 && (await page.locator(`.card[data-id="${theAlbum}"] .card-cover img`).count()) === 0);
  check("and the card (refreshed by the save) falls back to the placeholder", !!ph);
  await ctx.close();
}

// ---- 5. add images: picker, "New" tiles, one non-square -------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, single);
  check("starts with 1 image", (await tiles(page)).length === 1);
  await page.setInputFiles("#covers-file", [
    { name: "square.png", mimeType: "image/png", buffer: png(200, 200, [10, 200, 10]) },
    { name: "tall.png", mimeType: "image/png", buffer: png(120, 300, [200, 10, 10]) },
  ]);
  await page.waitForFunction(() => document.querySelectorAll("#covers-grid .tile-badge.new").length === 2 && !document.querySelector("#covers-grid li.busy"));
  const t = await tiles(page);
  check("two tiles appear marked New, appended after the existing one", t.length === 3 && t[1].isNew && t[2].isNew && !t[0].isNew && t[1].staged && t[2].staged, t);
  check("Save enabled once uploads are done", await page.locator("#covers-save").isEnabled());
  const sq = await page.evaluate(() => [...document.querySelectorAll("#covers-grid .cover-box")].map((b) => b.getBoundingClientRect()).map((r) => Math.abs(r.width - r.height) <= 1));
  check("the non-square upload is shown in a square tile", sq.every(Boolean), sq);
  await page.screenshot({ path: path.join(SHOTS, "added.png") });
  await saveAndWait(page);
  const nd = singleNd();
  const loc = names(nd.localPlaybackData && nd.localPlaybackData.localCoverImage);
  check("localCoverImage has the original then both new files", loc.length === 3 && loc[0] === "cover.png" && loc.every((n) => fs.existsSync(abs(P.single, n))), loc);
  const dims = (await getJson(`/api/collections/${single}/covers`)).covers.map((c) => `${c.w}x${c.h}`);
  check("both appear in the portal with their real size", dims.join() === "300x300,200x200,120x300", dims);
  const thumbs = dirFiles(`${P.single}/thumbnails`);
  check("thumbnails exist for the new images", loc.slice(1).every((n) => thumbs.some((f) => f.startsWith(path.parse(n).name + "-"))), thumbs);
  const originals = fs.readFileSync(abs(P.single, loc[2]));
  check("the non-square original is not modified (still 120x300)", originals.readUInt32BE(16) === 120 && originals.readUInt32BE(20) === 300);
  await ctx.close();
}

// ---- 6. delete a local image: file and thumbnails go -----------------------------------------------------------
{
  const { ctx, page } = await newPage();
  const loc0 = names(singleNd().localPlaybackData.localCoverImage);
  const victim = loc0[1];
  const stem = path.parse(victim).name;
  const hadThumbs = dirFiles(`${P.single}/thumbnails`).some((f) => f.startsWith(stem + "-"));
  await openFromCard(page, single);
  await page.locator('#covers-grid button[aria-label="Remove cover art 2"]').click();
  check("no 'hides the artist's image' note for a user image", (await page.locator("#covers-note.hidden").count()) === 1);
  await saveAndWait(page);
  const loc = names(singleNd().localPlaybackData.localCoverImage);
  check("the list lost it", loc.length === 2 && !loc.includes(victim), loc);
  check("its file is gone from disk", hadThumbs && !fs.existsSync(abs(P.single, victim)));
  check("its thumbnails are gone", !dirFiles(`${P.single}/thumbnails`).some((f) => f.startsWith(stem + "-")), dirFiles(`${P.single}/thumbnails`));
  await ctx.close();
}

// ---- 6b. an entry the portal cannot show is kept and announced ------------------------------------------------
{
  const f = `${P.single}/_Single.noudata`;
  const nd = singleNd();
  const loc = names(nd.localPlaybackData.localCoverImage);
  fs.writeFileSync(abs(P.single, "broken.png"), "this is not a png at all");
  nd.localPlaybackData.localCoverImage.images.push({ imageLocalFilename: "broken.png" });
  fs.writeFileSync(abs(f), JSON.stringify(nd));
  await fetch(BASE + "/api/rescan", { method: "POST", headers: { "X-Plippa": "1" } });
  const { ctx, page } = await newPage();
  await openFromCard(page, single);
  const note = (await page.textContent("#covers-unshown")).trim();
  check("the dialog says 1 image can't be shown and is kept", (await page.locator("#covers-unshown:not(.hidden)").count()) === 1 && /^1 image on the stick can't be shown here and is kept/.test(note), note);
  check("only the servable tiles are listed", (await tiles(page)).length === loc.length, loc);
  await page.locator('#covers-grid button[aria-label="Remove cover art 1"]').click();
  await saveAndWait(page);
  const after = names(singleNd().localPlaybackData.localCoverImage);
  check("saving keeps the hidden entry and its file", after.includes("broken.png") && fs.existsSync(abs(P.single, "broken.png")), after);
  await ctx.close();
}

// ---- 7. refused files: .txt and 20 MB ---------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  const reqs = [];
  page.on("request", (r) => r.method() === "POST" && reqs.push(r.url()));
  await openFromCard(page, bare);
  check("an album without art shows the empty hint", (await page.locator("#covers-empty:not(.hidden)").count()) === 1 && (await tiles(page)).length === 0);
  await page.setInputFiles("#covers-file", [
    { name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("hello") },
    { name: "huge.png", mimeType: "image/png", buffer: Buffer.alloc(20 * 1024 * 1024) },
  ]);
  const err = await page.textContent("#covers-error");
  check("a .txt is refused with a clear message", /notes\.txt.*not a JPEG, PNG or WebP/.test(err), err);
  check("a 20 MB file is refused with a clear message", /huge\.png.*larger than 16 MB/.test(err), err);
  check("no tile and no upload for either", (await tiles(page)).length === 0 && reqs.length === 0, reqs);
  // the server stays the authority: a file that claims to be a PNG but is not
  await page.setInputFiles("#covers-file", [{ name: "fake.png", mimeType: "image/png", buffer: Buffer.from("this is not a png at all") }]);
  await page.waitForFunction(() => document.querySelector("#covers-grid li.failed"));
  const f = (await tiles(page))[0];
  check("a fake image shows the server's refusal on a failed tile", f.failed && /JPEG, PNG or WebP|image/i.test(f.msg), f);
  check("the failed tile offers Retry and Remove", (await page.locator("#covers-grid li.failed .tile-retry:not(.hidden)").count()) === 1 && (await page.locator('#covers-grid li.failed button[aria-label^="Remove failed upload"]').count()) === 1);
  check("a failed tile does not count as a change (Save stays off)", await page.locator("#covers-save").isDisabled());
  await page.locator('#covers-grid li.failed button[aria-label^="Remove failed upload"]').click();
  check("Remove clears the failed tile", (await tiles(page)).length === 0);
  await page.click("#covers-cancel");
  await ctx.close();
}

// ---- 8. 12 images max ------------------------------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, bare);
  const files = Array.from({ length: 13 }, (_, i) => ({ name: `i${i}.png`, mimeType: "image/png", buffer: png(16 + i, 16, [i * 10, 50, 50]) }));
  await page.setInputFiles("#covers-file", files);
  await page.waitForFunction(() => document.querySelectorAll("#covers-grid .tile-badge.new").length === 12 && !document.querySelector("#covers-grid li.busy"), null, { timeout: 30000 });
  check("at most 12 tiles; the 13th is refused with a message", (await tiles(page)).length === 12 && /At most 12/.test(await page.textContent("#covers-error")));
  check("Add images… is disabled at the limit", await page.locator("#covers-pick-btn").isDisabled());
  page.once("dialog", (d) => d.accept());
  await page.click("#covers-cancel");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
  await ctx.close();
}

// ---- 9. Cancel / Esc write nothing, ask when there are changes ------------------------------------------------------
{
  const { ctx, page } = await newPage();
  const snap = listing(LIB);
  await openFromCard(page, rainy, "playlists");
  await page.setInputFiles("#covers-file", [{ name: "cancelled.png", mimeType: "image/png", buffer: png(64, 64, [1, 2, 3]) }]);
  await page.waitForFunction(() => document.querySelectorAll("#covers-grid .tile-badge.new").length === 1 && !document.querySelector("#covers-grid li.busy"));
  const stagedSrc = await page.locator("#covers-grid li:has(.tile-badge.new) img").getAttribute("src");
  const stagedPath = stagedSrc.replace(/\?.*$/, "");
  check("the staged upload is previewable while the dialog is open", (await fetch(BASE + stagedPath)).status === 200);
  await page.locator('#covers-grid button[aria-label="Remove cover art 1"]').click();
  let asked = null;
  page.once("dialog", (d) => {
    asked = d.message();
    d.dismiss();
  });
  await page.click("#covers-cancel");
  await sleep(200);
  check("Cancel with changes asks first; declining keeps the dialog and the edits", /Discard/.test(asked || "") && (await dlgOpen(page).count()) === 1 && (await tiles(page)).length === 1);
  page.once("dialog", (d) => d.accept());
  await page.click("#covers-cancel");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
  check("accepting discards: nothing on disk changed", listing(LIB) === snap);
  check("Cancel told the server to drop the staged upload (slot freed)", !!(await until(async () => (await fetch(BASE + stagedPath)).status === 404)));
  // Esc
  await page.click(`.card-cell:has(.card[data-id="${rainy}"]) .card-menu`);
  await page.click('.menu [role="menuitem"]:has-text("Edit cover art")');
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile");
  await page.locator('#covers-grid button[aria-label="Remove cover art 1"]').click();
  page.once("dialog", (d) => d.accept());
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector("#covers-dialog").open);
  check("Esc (after a confirm) also writes nothing", listing(LIB) === snap);
  // reopen: state is fresh
  await page.click(`.card-cell:has(.card[data-id="${rainy}"]) .card-menu`);
  await page.click('.menu [role="menuitem"]:has-text("Edit cover art")');
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile");
  check("reopening shows the stored list, not the discarded edits", (await tiles(page)).length === 1 && !(await tiles(page))[0].isNew);
  await page.keyboard.press("Escape");
  await ctx.close();
}

// ---- 10. file drop on the dialog; a playlist; the loose track ---------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, rainy, "playlists");
  await page.evaluate(() => {
    const dt = new DataTransfer();
    const bytes = Uint8Array.from(atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="), (c) => c.charCodeAt(0));
    dt.items.add(new File([bytes], "dropped.png", { type: "image/png" }));
    const d = document.getElementById("covers-dialog");
    d.dispatchEvent(new DragEvent("dragover", { dataTransfer: dt, bubbles: true, cancelable: true }));
    d.dispatchEvent(new DragEvent("drop", { dataTransfer: dt, bubbles: true, cancelable: true }));
  });
  await page.waitForFunction(() => document.querySelectorAll("#covers-grid .tile-badge.new").length === 1 && !document.querySelector("#covers-grid li.busy"));
  check("dropping a file on the dialog adds a New tile", (await tiles(page)).length === 2);
  await saveAndWait(page);
  const loc = names(rainyNd().localPlaybackData.localCoverImage);
  check("playlist: the dropped image is saved next to the existing one", loc.length === 2 && loc[0] === "rainy-art.png" && fs.existsSync(abs(P.rainy, loc[1])), loc);
  await ctx.close();
}
{
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/tracks`);
  await page.waitForSelector(`tr[data-id="${demo}"]`);
  await page.click(`tr[data-id="${demo}"] .kebab`);
  await page.click('.menu [role="menuitem"]:has-text("Edit cover art")');
  await dlgOpen(page).waitFor();
  await page.waitForSelector("#covers-grid li.cover-tile");
  check("loose track: the dialog lists its itemCoverImage", (await tiles(page)).length === 1);
  await page.setInputFiles("#covers-file", [{ name: "second.png", mimeType: "image/png", buffer: png(90, 90, [90, 10, 90]) }]);
  await page.waitForFunction(() => document.querySelectorAll("#covers-grid .tile-badge.new").length === 1 && !document.querySelector("#covers-grid li.busy"));
  await saveAndWait(page);
  const nd = demoNd();
  const item = names(nd.mediaList[0].itemCoverImage);
  check("loose track: itemCoverImage on the media item has both images", item.length === 2 && item[0] === "demo-art.png" && fs.existsSync(abs(P.demo, item[1])), item);
  check("loose track: no localCoverImage invented", !(nd.localPlaybackData && nd.localPlaybackData.localCoverImage));
  await ctx.close();
}

// ---- 11. a server error keeps the dialog open --------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await openFromCard(page, trip);
  const countBefore = (await tiles(page)).length;
  await page.locator('#covers-grid button[aria-label="Remove cover art 1"]').click();
  await page.route(`**/api/collections/${trip}/covers`, (r) => (r.request().method() === "PUT" ? r.fulfill({ status: 400, contentType: "application/json", body: JSON.stringify({ error: "nope, <b>bad</b>" }) }) : r.continue()));
  await page.click("#covers-save");
  await page.waitForFunction(() => document.querySelector("#covers-error").textContent.length > 0);
  check("a failed Save shows the server's message as text and keeps the dialog and edits", (await page.textContent("#covers-error")) === "nope, <b>bad</b>" && (await dlgOpen(page).count()) === 1 && (await tiles(page)).length === countBefore - 1 && (await page.locator("#covers-error b").count()) === 0);
  check("Save is usable again after the error", await page.locator("#covers-save").isEnabled());
  page.once("dialog", (d) => d.accept());
  await page.click("#covers-cancel");
  await ctx.close();
}

// ---- 12. phone width -----------------------------------------------------------------------------------------------------
{
  const { ctx, page } = await newPage({ viewport: { width: 390, height: 800 }, hasTouch: true });
  await openFromCard(page, trip);
  await page.waitForSelector("#covers-grid li.cover-tile");
  await sleep(300);
  const m = await page.evaluate(() => {
    const d = document.getElementById("covers-dialog").getBoundingClientRect();
    const small = [...document.querySelectorAll("#covers-dialog button")].filter((b) => b.offsetParent && b.getBoundingClientRect().height < 43.5).map((b) => b.textContent + ":" + b.getBoundingClientRect().height);
    const narrow = [...document.querySelectorAll("#covers-grid .tile-actions button")].filter((b) => b.getBoundingClientRect().width < 43.5).map((b) => b.getAttribute("aria-label"));
    return { sw: document.documentElement.scrollWidth, iw: innerWidth, left: d.left, right: d.right, small, narrow, dsw: document.getElementById("covers-dialog").scrollWidth, dcw: document.getElementById("covers-dialog").clientWidth };
  });
  check("390 px: dialog fits the viewport, no horizontal scroll", m.sw <= m.iw && m.left >= 0 && m.right <= m.iw && m.dsw <= m.dcw, m);
  check("390 px: every dialog button is at least 44 px high and wide", m.small.length === 0 && m.narrow.length === 0, m);
  await page.screenshot({ path: path.join(SHOTS, "phone.png") });
  await page.click("#covers-cancel");
  await ctx.close();
}

check("no console errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures === 0 ? "ALL OK" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
