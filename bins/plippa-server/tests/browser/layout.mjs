// POLISH_03 browser test (Playwright + Chromium): the library and detail sections lay out
// correctly at every viewport width. Not part of `cargo test`; starts its own portal_fixture
// server over a fixture library (mkfix.py plus an album with long and unbroken titles).
//
//   cargo build -p plippa-server --example portal_fixture
//   LIB=/tmp/lib_layout SHOTS=/tmp/shots_layout node layout.mjs      # prints PASS/FAIL, ALL OK (port 8860)
//
// Asserted at each width, on the Tracks tab and on an open Albums collection, with the detail
// panel filled: section.library has no horizontal overflow and nothing paints outside it,
// library and detail boxes are disjoint, side by side (detail to the right) from 1000 px and
// stacked (detail below, full width) under that, and the page has no horizontal scroll.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_layout_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8860";
const BASE = `http://127.0.0.1:${PORT}`;
const BIN =
  process.env.BIN ||
  path.join(process.env.CARGO_TARGET_DIR || path.join(here, "../../../../target"), "debug/examples/portal_fixture");
const WIDTHS = [2560, 1600, 1280, 1100, 1000, 999, 960, 900, 861, 860, 700, 600, 390, 320];
const BREAKPOINT = 1000;
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

// --- fixture: mkfix.py library plus a long-title multi-track album ---------------------------
spawnSync("python3", [path.join(here, "mkfix.py"), LIB], { stdio: "inherit" });
const UNBROKEN = "Unbroken_" + "x".repeat(31); // 40 characters, no break opportunity
const LONG = "A very long track title that keeps going and going so that a table cell has to wrap or scroll somewhere";
const dir = path.join(LIB, "Music", "Long", "Wide");
fs.mkdirSync(dir, { recursive: true });
const items = [LONG, UNBROKEN, "Short"].map((t, i) => {
  fs.writeFileSync(path.join(dir, `0${i + 1}.mp3`), "");
  return { mediaTitle: t, localFilePaths: [`0${i + 1}.mp3`] };
});
fs.writeFileSync(
  path.join(dir, "_Wide.noudata"),
  JSON.stringify({
    noudataVersion: "0.4.0",
    noudataType: "release",
    playableItemTitle: UNBROKEN + " album with a long name too",
    playableItemProfile: "music",
    releaseData: { credits: { primaryArtists: [{ primaryArtistName: "Artist_" + "y".repeat(33) }] } },
    mediaList: items,
  }),
);

const server = spawn(BIN, [LIB, PORT], { stdio: ["ignore", "ignore", "inherit"] });
process.on("exit", () => server.kill());
if (!(await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100))) {
  console.log("FAIL  server did not start");
  process.exit(1);
}

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
page.on("dialog", (d) => d.dismiss());

// Everything is measured inside the page: boxes, overflow, and any element painted outside the
// library box that no scrolling ancestor clips.
function measure() {
  const lib = document.querySelector("section.library");
  const det = document.querySelector("section.detail");
  const r = (e) => {
    const b = e.getBoundingClientRect();
    return { l: b.left, t: b.top, r: b.right, b: b.bottom, w: b.width, h: b.height };
  };
  const L = r(lib);
  const D = r(det);
  const escaped = [];
  for (const el of lib.querySelectorAll("*")) {
    const b = el.getBoundingClientRect();
    if (b.width === 0 && b.height === 0) continue;
    let clipped = false;
    for (let p = el.parentElement; p && p !== lib; p = p.parentElement) {
      const o = getComputedStyle(p).overflowX;
      if (o !== "visible") {
        clipped = true;
        break;
      }
    }
    if (!clipped && (b.right > L.r + 0.5 || b.left < L.l - 0.5)) escaped.push(el.tagName.toLowerCase() + "." + el.className);
  }
  const disjoint = L.r <= D.l + 0.5 || D.r <= L.l + 0.5 || L.b <= D.t + 0.5 || D.b <= L.t + 0.5;
  const root = document.documentElement;
  return {
    lib: L,
    det: D,
    libScroll: lib.scrollWidth,
    libClient: lib.clientWidth,
    escaped: escaped.slice(0, 5),
    disjoint,
    pageScroll: root.scrollWidth,
    inner: innerWidth,
    layoutLeft: document.querySelector(".layout").getBoundingClientRect().left,
    layoutRight: document.querySelector(".layout").getBoundingClientRect().right,
  };
}

async function show(view) {
  if (view === "tracks") {
    await page.evaluate(() => (location.hash = "#/tracks"));
    await page.locator("table.tracks").waitFor();
  } else {
    await page.evaluate(() => (location.hash = "#/albums"));
    await page.locator(".card").first().waitFor();
    await page.locator(".card", { hasText: UNBROKEN }).first().click();
    await page.locator("ol.items").waitFor();
  }
  await sleep(100);
}

await page.setViewportSize({ width: 1280, height: 900 });
await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");
// Fill the detail panel (Details… of the long album), then walk the views.
await show("albums");
await page.click("#coll-details");
await page.locator("#detail-panel:not(.hidden)").waitFor();

for (const w of WIDTHS) {
  await page.setViewportSize({ width: w, height: 800 });
  for (const view of ["tracks", "albums"]) {
    await show(view);
    const m = await page.evaluate(measure);
    const tag = `${w}px ${view}`;
    check(`${tag}: library has no horizontal overflow`, m.libScroll <= m.libClient, [m.libScroll, m.libClient]);
    check(`${tag}: nothing paints outside the library box`, m.escaped.length === 0, m.escaped);
    check(`${tag}: library and detail do not overlap`, m.disjoint, { lib: m.lib, det: m.det });
    if (w >= BREAKPOINT) {
      check(`${tag}: detail is to the right of the library`, m.det.l >= m.lib.r - 0.5 && Math.abs(m.det.t - m.lib.t) < 40, { lib: m.lib, det: m.det });
      check(`${tag}: detail ends at the right edge of the layout`, m.layoutRight - m.det.r <= 40, [m.layoutRight, m.det.r]);
      check(`${tag}: layout is centred`, Math.abs(m.layoutLeft - (m.inner - m.layoutRight)) <= 2, [m.layoutLeft, m.inner - m.layoutRight]);
    } else {
      check(`${tag}: detail stacked below the library, full width`, m.det.t >= m.lib.b - 0.5 && Math.abs(m.det.w - m.lib.w) <= 1, { lib: m.lib, det: m.det });
    }
    check(`${tag}: no horizontal page scroll`, m.pageScroll <= m.inner, [m.pageScroll, m.inner]);
    if (w <= 560 && view === "tracks") {
      const small = await page.evaluate(() =>
        [...document.querySelectorAll("section.library button, section.detail button")]
          .filter((b) => b.offsetParent !== null && !b.classList.contains("link") && b.getBoundingClientRect().height < 43.5)
          .map((b) => (b.id || b.textContent || b.className).trim().slice(0, 24)),
      );
      check(`${tag}: touch buttons are at least 44 px high`, small.length === 0, small);
    }
    if ([1600, 960, 390, 320].includes(w)) await page.screenshot({ path: path.join(SHOTS, `${w}-${view}.png`), fullPage: true });
  }
}

// Sticky detail on desktop must not clip a tall panel on a short viewport.
await page.setViewportSize({ width: 1280, height: 400 });
await show("tracks");
const reach = await page.evaluate(() => {
  const det = document.querySelector("section.detail");
  return { scroll: det.scrollHeight, client: det.clientHeight, overflowY: getComputedStyle(det).overflowY, vh: innerHeight, h: det.getBoundingClientRect().height };
});
check("short viewport: the detail panel stays within the viewport and scrolls inside itself", reach.h <= reach.vh && (reach.scroll <= reach.client + 1 || reach.overflowY === "auto"), reach);

check("no console errors", problems.length === 0, problems);
await browser.close();
server.kill();
console.log(failures === 0 ? "ALL OK" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
