// ART_05 browser test (Playwright + Chromium): cover thumbnails, slideshow and lightbox.
// Not part of `cargo test`; starts its own portal_fixture over a fresh mkfix.py library.
//
//   cargo build -p plippa-server --example portal_fixture
//   PORT=8872 LIB=/tmp/lib_covers SHOTS=/tmp/shots_covers node covers_view.mjs     # PASS/FAIL, ALL OK
//
// Env: BIN (default $CARGO_TARGET_DIR/debug/examples/portal_fixture), PLAYWRIGHT_DIR.
// The auto-advance timer is driven with Playwright's clock (page.clock.runFor), not real waits.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_covers_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8872";
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
const rainy = await idOf("playlist", "Rainy");
const demo = (await getJson("/api/tracks?view=track")).tracks.find((t) => t.title === "Tagless Demo").id;

const browser = await chromium.launch();
const problems = [];
async function newPage(opts = {}) {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 }, ...opts });
  const page = await ctx.newPage();
  page.on("console", (m) => m.type() === "error" && !/Failed to load resource/.test(m.text()) && problems.push("console: " + m.text()));
  page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
  return { ctx, page };
}

const sliderState = (page) =>
  page.evaluate(() => {
    const s = document.querySelector("#lib-view .cover-slider");
    if (!s) return null;
    const layers = [...s.querySelectorAll(".cover-layer")];
    const act = s.querySelector(".cover-layer.is-active");
    const dots = [...s.querySelectorAll(".cover-dot")];
    return {
      layers: layers.length,
      active: layers.filter((l) => l.classList.contains("is-active")).length,
      src: act && act.getAttribute("src"),
      dot: dots.findIndex((d) => d.hasAttribute("aria-current")),
      dots: dots.length,
      nav: s.querySelectorAll(".cover-nav").length,
    };
  });
const coverN = (src) => Number((/\/covers\/(\d+)/.exec(src || "") || [])[1]);

// ---- 1. thumbnails ---------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  const reqs = [];
  page.on("request", (r) => reqs.push(r.url()));
  await page.goto(BASE + "/#/albums");
  const card = page.locator(`.card[data-id="${trip}"] .card-cover`);
  await card.locator("img").waitFor();
  await page.waitForFunction((id) => {
    const i = document.querySelector(`.card[data-id="${id}"] .card-cover img`);
    return i && i.complete && i.naturalWidth > 0;
  }, trip);
  const box = await card.boundingBox();
  const ibox = await card.locator("img").boundingBox();
  const nat = await card.locator("img").evaluate((i) => [i.naturalWidth, i.naturalHeight, getComputedStyle(i).objectFit]);
  check("card thumbnail box is square (non-square 400x200 source)", Math.abs(box.width - box.height) < 1.5 && box.width > 40, box);
  check("the image fills the square box and is cropped, not stretched", Math.abs(ibox.width - ibox.height) < 1.5 && nat[2] === "cover" && nat[0] / nat[1] > 1.5, { ibox, nat });
  check("cards request the 128 tier", (await card.locator("img").getAttribute("src")).includes("size=128") && reqs.some((u) => u.includes(`/api/collections/${trip}/covers/0?size=128`)));
  check("a placeholder shows for an album without art", (await page.locator(`.card[data-id="${bare}"] .card-cover`).getAttribute("class")).includes("nocover"));
  check("an album with art drops the placeholder", !(await card.getAttribute("class")).includes("nocover"));
  await page.screenshot({ path: path.join(SHOTS, "cards.png"), fullPage: true });
  await page.click('[data-tab="playlists"]');
  await page.waitForSelector(`.card[data-id="${rainy}"]`);
  check("playlist with art shows its thumbnail", (await page.locator(`.card[data-id="${rainy}"] .card-cover img`).count()) === 1);
  await ctx.close();
}

// ---- 1b. a stale link falls back to the placeholder ------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.route(`**/api/collections/${single}/covers/0?size=128`, (r) => r.abort());
  await page.goto(BASE + "/#/albums");
  await page.waitForSelector(`.card[data-id="${single}"]`);
  const ok = await until(() =>
    page.evaluate((id) => {
      const b = document.querySelector(`.card[data-id="${id}"] .card-cover`);
      return b.classList.contains("nocover") && !b.querySelector("img");
    }, single),
  );
  check("a failed thumbnail request leaves the placeholder, not a broken image", !!ok);
  await ctx.close();
}

// ---- 2. slideshow ----------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.clock.install();
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-dot");
  let s = await sliderState(page);
  check("3 images: two stacked layers, one active, three dots, prev/next", s.layers === 2 && s.active === 1 && s.dots === 3 && s.nav === 2 && s.dot === 0 && coverN(s.src) === 0, s);
  const stage = await page.locator("#lib-view .cover-stage").boundingBox();
  check("slider stage is square", Math.abs(stage.width - stage.height) < 1.5 && stage.width > 60, stage);
  await page.screenshot({ path: path.join(SHOTS, "collection.png"), fullPage: true });

  await page.clock.runFor(5100);
  s = await sliderState(page);
  check("auto-advances after 5 s; both layers stay in the DOM with one active", s.layers === 2 && s.active === 1 && coverN(s.src) === 1 && s.dot === 1, s);
  const cross = await page.evaluate(() => {
    const l = [...document.querySelectorAll("#lib-view .cover-layer")];
    return { fade: getComputedStyle(l[0]).transitionDuration, props: getComputedStyle(l[0]).transitionProperty };
  });
  check("the swap is a 300 ms opacity crossfade", cross.fade === "0.3s" && cross.props === "opacity", cross);

  await page.hover("#lib-view .cover-stage");
  await page.clock.runFor(12000);
  s = await sliderState(page);
  check("hover pauses the auto-advance", s.dot === 1, s);
  await page.mouse.move(2, 2);
  await page.clock.runFor(5100);
  s = await sliderState(page);
  check("moving away resumes it", s.dot === 2, s);

  await page.focus("#lib-view .cover-open");
  await page.keyboard.press("ArrowRight");
  s = await sliderState(page);
  check("ArrowRight wraps to the first image; dots follow", s.dot === 0 && coverN(s.src) === 0, s);
  await page.keyboard.press("ArrowLeft");
  s = await sliderState(page);
  check("ArrowLeft goes back (wrapping)", s.dot === 2, s);
  await page.clock.runFor(12000);
  check("keyboard focus inside the slider pauses it", (await sliderState(page)).dot === 2);
  await page.click("#lib-view .cover-prev");
  check("prev button", (await sliderState(page)).dot === 1);
  await page.click("#lib-view .cover-dot >> nth=2");
  check("a dot jumps to its image", (await sliderState(page)).dot === 2);
  const small = await page.evaluate(() => [...document.querySelectorAll("#lib-view .cover-nav")].map((b) => b.getBoundingClientRect().height).filter((h) => h < 44));
  check("prev/next targets are at least 44 px", small.length === 0, small);

  // leaving the page of the view stops the timer: the slider of a re-rendered view is gone
  await page.goto(BASE + `/#/albums`);
  await page.waitForSelector(".cards");
  check("leaving the collection removes the slider", (await page.locator(".cover-slider").count()) === 0);
  await ctx.close();
}

// ---- 3. one image / no image / loose track ---------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/albums/${single}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-layer");
  const s = await sliderState(page);
  check("single image: no controls and no timer", s.nav === 0 && s.dots === 0 && s.dot === -1 && s.layers === 2, s);
  await page.goto(BASE + `/#/albums/${bare}`);
  await page.waitForSelector("#lib-view .coll-head");
  await sleep(300);
  check("no image: placeholder, no slider", (await page.locator(".cover-slider").count()) === 0 && (await page.locator("#lib-view .coll-cover .cover-box.nocover").count()) === 1);
  await page.goto(BASE + `/#/tracks/${demo}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-layer");
  check("loose track with itemCoverImage shows its art", coverN(await page.locator("#lib-view .cover-layer.is-active").getAttribute("src")) === 0);
  await page.goto(BASE + `/#/playlists/${rainy}`);
  await page.waitForSelector("#lib-view li.item");
  await page.waitForSelector("#lib-view li.item .row-cover");
  const rows = await page.locator("#lib-view li.item .row-cover").evaluateAll((bs) => bs.map((b) => b.getBoundingClientRect()).map((r) => [r.width, r.height]));
  check("playlist items whose art differs show a square row thumbnail (<= 2.75 rem)", rows.length === 3 && rows.every(([w, h]) => Math.abs(w - h) < 1.5 && h <= 44.5), rows);
  await ctx.close();
}

// ---- 4. reduced motion -----------------------------------------------------------------------
{
  const { ctx, page } = await newPage({ reducedMotion: "reduce" });
  await page.clock.install();
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-dot");
  await page.clock.runFor(30000);
  const s = await sliderState(page);
  check("prefers-reduced-motion: never auto-advances", s.dot === 0, s);
  await page.click("#lib-view .cover-next");
  const tr = await page.evaluate(() => getComputedStyle(document.querySelector("#lib-view .cover-layer")).transitionDuration);
  check("prefers-reduced-motion: manual switching is instant (no fade) and works", (await sliderState(page)).dot === 1 && tr === "0s", tr);
  await ctx.close();
}

// ---- 5. lightbox -----------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.clock.install();
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-dot");
  await page.click("#lib-view .cover-next"); // now on image 2
  await page.click("#lib-view .cover-open");
  const dlg = page.locator("dialog.cover-lightbox[open]");
  await dlg.waitFor();
  check("lightbox opens on the slider's index (aria-label)", (await dlg.getAttribute("aria-label")) === "Cover art 2 of 3");
  const lbSrc = await dlg.locator("img").getAttribute("src");
  check("lightbox asks for the 1024 tier", lbSrc.includes("size=1024") && coverN(lbSrc) === 1, lbSrc);
  const lb = await dlg.locator(".lb-box").boundingBox();
  check("lightbox image box is square and within the viewport", Math.abs(lb.width - lb.height) < 1.5 && lb.width <= 1200 * 0.92 + 1 && lb.height <= 900 * 0.92 + 1, lb);
  await page.screenshot({ path: path.join(SHOTS, "lightbox.png") });
  await page.clock.runFor(12000);
  check("no auto-advance while the lightbox is open", (await dlg.getAttribute("aria-label")) === "Cover art 2 of 3" && (await sliderState(page)).dot === 1);
  await page.keyboard.press("ArrowRight");
  check("ArrowRight navigates the lightbox", (await dlg.getAttribute("aria-label")) === "Cover art 3 of 3");
  await page.keyboard.press("ArrowRight");
  check("the lightbox does not wrap", (await dlg.getAttribute("aria-label")) === "Cover art 3 of 3");
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.press("ArrowRight");
  const focusIn = await page.evaluate(() => document.querySelector("dialog.cover-lightbox").contains(document.activeElement));
  for (let i = 0; i < 6; i++) await page.keyboard.press("Tab");
  const focusStill = await page.evaluate(() => document.querySelector("dialog.cover-lightbox").contains(document.activeElement));
  check("focus is inside the dialog and stays there when tabbing", focusIn && focusStill);
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.querySelector("dialog.cover-lightbox"));
  const back = await page.evaluate(() => document.activeElement && document.activeElement.className);
  check("Esc closes the lightbox and focus returns to the opener", back === "cover-open", back);
  let s = await sliderState(page);
  check("the slider resumes on the lightbox's last image", s.dot === 2 && coverN(s.src) === 2, s);
  await page.clock.runFor(5100);
  check("focus on the image keeps it paused", (await sliderState(page)).dot === 2);
  await page.evaluate(() => document.activeElement.blur());
  await page.mouse.move(2, 2); // the earlier clicks left the pointer over the slider (hover pauses)
  await page.clock.runFor(5100);
  s = await sliderState(page);
  check("and auto-advance continues from there once focus leaves", s.dot === 0, s);
  // backdrop click
  await page.click("#lib-view .cover-open");
  await page.locator("dialog.cover-lightbox[open]").waitFor();
  await page.mouse.click(3, 3);
  await page.waitForFunction(() => !document.querySelector("dialog.cover-lightbox"));
  check("a click on the backdrop closes it", true);
  // Enter on the image opens it too
  await page.focus("#lib-view .cover-open");
  await page.keyboard.press("Enter");
  check("Enter on the image opens the lightbox", (await page.locator("dialog.cover-lightbox[open]").count()) === 1);
  await page.keyboard.press("Escape");
  await ctx.close();
}

// ---- 6. details panel ------------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#coll-details");
  await page.click("#coll-details");
  await page.waitForSelector("#detail-cover .cover-slider .cover-dot");
  const b = await page.locator("#detail-cover .cover-stage").boundingBox();
  const panel = await page.locator("#detail-panel").boundingBox();
  check("details panel has a square cover slot, <= 14 rem, above the title", Math.abs(b.width - b.height) < 1.5 && b.width <= 224.5 && b.width > 60 && b.y < (await page.locator("#detail-title").boundingBox()).y && b.x >= panel.x - 1, { b, panel });
  await page.screenshot({ path: path.join(SHOTS, "detail.png"), fullPage: true });
  await page.goto(BASE + `/#/albums/${bare}`);
  await page.waitForSelector("#coll-details");
  await page.click("#coll-details");
  await sleep(400);
  check("details panel shows a placeholder when there is no art", (await page.locator("#detail-cover .cover-slider").count()) === 0 && (await page.locator("#detail-cover .cover-box.nocover").count()) === 1);
  await ctx.close();
}

// ---- 7. hostile alt text ---------------------------------------------------------------------
{
  const { ctx, page } = await newPage();
  let dialogs = 0;
  page.on("dialog", (d) => {
    dialogs++;
    d.dismiss();
  });
  const bad = [];
  page.on("request", (r) => {
    if (new URL(r.url()).pathname === "/x") bad.push(r.url());
  });
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-slider .cover-dot");
  await page.click("#lib-view .cover-dot >> nth=1");
  await page.click("#lib-view .cover-open");
  await page.locator("dialog.cover-lightbox[open]").waitFor();
  await page.click("#coll-details", { force: true }).catch(() => {});
  await page.keyboard.press("Escape");
  await page.click("#coll-details");
  await page.waitForSelector("#detail-cover .cover-dot");
  await sleep(500);
  const alt = await page.locator("#lib-view .cover-layer.is-active").getAttribute("alt");
  const injected = await page.evaluate(() => document.querySelectorAll('img[src="x"], #lib-view .cover-slider img:not(.cover-layer)').length);
  check("hostile altText stays text: no dialog, no /x request, no injected element", dialogs === 0 && bad.length === 0 && injected === 0 && alt === "<img src=x onerror=alert(1)>", { dialogs, bad, injected, alt });
  await ctx.close();
}

// ---- 8. phone width --------------------------------------------------------------------------
{
  const { ctx, page } = await newPage({ viewport: { width: 390, height: 800 }, hasTouch: true });
  for (const [name, hash] of [["cards", "#/albums"], ["collection with slider", `#/albums/${trip}`], ["playlist rows", `#/playlists/${rainy}`]]) {
    await page.goto(BASE + "/" + hash);
    await page.waitForSelector("#lib-view .card, #lib-view .coll-head");
    await sleep(600);
    const o = await page.evaluate(() => ({ sw: document.documentElement.scrollWidth, iw: innerWidth }));
    check(`390 px: no horizontal scroll (${name})`, o.sw <= o.iw, o);
  }
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-dot");
  await page.click("#coll-details");
  await page.waitForSelector("#detail-cover .cover-dot");
  await sleep(300);
  const o = await page.evaluate(() => ({ sw: document.documentElement.scrollWidth, iw: innerWidth }));
  check("390 px: no horizontal scroll with slider and details panel", o.sw <= o.iw, o);
  await page.goto(BASE + `/#/albums/${trip}`);
  await page.waitForSelector("#lib-view .cover-dot");
  await page.screenshot({ path: path.join(SHOTS, "phone.png"), fullPage: true });
  await ctx.close();
}

check("no console errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures === 0 ? "ALL OK" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
