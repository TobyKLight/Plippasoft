// XCHECK browser test (Playwright + Chromium): a TAGLESS single-file upload chosen as Mixtape /
// Audiobook / Podcast is a loose *track* (classification) that is GROUPED under Mixes /
// Audiobooks / Podcasts; a tagless Music file stays under Tracks. Timestamps can be edited and
// played there. Not part of `cargo test`; starts its own portal_fixture server.
//
//   cargo build -p plippa-server --example portal_fixture
//   LIB=/tmp/lib_group SHOTS=/tmp/shots_group node grouping_flow.mjs      # prints PASS/FAIL, ALL OK
//
// (mkfix.py builds the fixture library; the server runs on PORT, default 8850.)
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));

const LIB = process.env.LIB || "/tmp/plippa_xcheck_group_lib";
const SHOTS = process.env.SHOTS || ".";
const PORT = process.env.PORT || "8850";
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

// Tagless WAVs: no RIFF INFO chunk at all (so there is no artist/album tag to make a release).
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
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "plippa-group-"));
for (const [n, s] of [["tagless set", 300], ["tagless book", 60], ["tagless cast", 60], ["tagless song", 30]]) {
  fs.writeFileSync(path.join(tmp, n + ".wav"), wav(s));
}

spawnSync("python3", [path.join(here, "mkfix.py"), LIB], { stdio: "inherit" });
const server = spawn(BIN, [LIB, PORT], { stdio: ["ignore", "ignore", "inherit"] });
process.on("exit", () => server.kill());
await until(async () => fetch(BASE + "/api/status").then((r) => r.ok).catch(() => false), 20000, 100);

const J = { "Content-Type": "application/json" };
const list = async (kind) => {
  const r = await (await fetch(`${BASE}/api/collections?kind=${kind}`)).json();
  return Array.isArray(r) ? r : r.collections || [];
};
const kinds = async () => Object.fromEntries(((await (await fetch(`${BASE}/api/kinds`)).json()).kinds || []).map((k) => [k.kind, k.count]));
const walk = (d, out = []) => {
  for (const e of fs.existsSync(d) ? fs.readdirSync(d, { withFileTypes: true }) : []) {
    const p = path.join(d, e.name);
    if (e.name === ".plippa") continue;
    e.isDirectory() ? walk(p, out) : out.push(path.relative(LIB, p));
  }
  return out.sort();
};
const readDoc = (rel) => JSON.parse(fs.readFileSync(path.join(LIB, rel), "utf8"));

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1200, height: 900 } });
const page = await ctx.newPage();
const problems = [];
page.on("console", (m) => m.type() === "error" && problems.push("console: " + m.text()));
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
page.on("dialog", (d) => d.dismiss());
const shot = (n) => page.screenshot({ path: path.join(SHOTS, n + ".png") });

const before = await kinds();
await page.goto(BASE + "/");
await page.waitForSelector("#lib-nav .tab-btn");

async function add(file, kind, cues) {
  await page.click("#add-open-btn");
  await page.setInputFiles("#add-file", path.join(tmp, file));
  await until(async () => !(await page.locator("#add-ok").isDisabled()), 3000);
  await page.check(`input[name="add-kind"][value="${kind}"]`);
  if (cues) await page.fill("#add-cue-text", cues);
  await page.click("#add-ok");
  await page.waitForFunction(() => !document.getElementById("add-dialog").open, null, { timeout: 20000 });
}

// 1. tagless mix -> Mixes tab
await add("tagless set.wav", "mix", "0:00 Intro\n1:30 Middle part\n3:00 End");
await page.waitForFunction(() => location.hash === "#/mixes", null, { timeout: 10000 });
check("lands on the Mixes tab (the tab that lists it)", true);
await page.waitForSelector("#lib-view table.tracks");
check("the loose mix has a row in Mixes", (await page.locator('#lib-view table.tracks tr:has-text("tagless set")').count()) === 1);
const mixRel = walk(LIB).find((f) => f.endsWith("tagless set.noudata"));
const doc = mixRel ? readDoc(mixRel) : {};
check("file is a looseTrack with profile mix and no releaseData (classification unchanged)", doc.noudataType === "looseTrack" && doc.playableItemProfile === "mix" && !doc.releaseData, doc);
const mixRows = await list("mix");
const mine = mixRows.find((c) => c.title === "tagless set");
check("API ?kind=mix lists it; kind says track, group mix, noudata_type looseTrack", !!mine && mine.kind === "track" && mine.group === "mix" && mine.noudata_type === "looseTrack", mine);
check("API ?kind=track does not list it", !(await list("track")).some((c) => c.title === "tagless set"));
const k1 = await kinds();
check("tab counts: Mixes +1, Tracks unchanged", k1.mix === (before.mix || 0) + 1 && k1.track === (before.track || 0), [before, k1]);
await page.waitForSelector(`#lib-nav .tab-btn[data-tab="mixes"]`);
check("Mixes tab label shows the new count", (await page.textContent('#lib-nav .tab-btn[data-tab="mixes"]')).includes(`(${k1.mix})`), await page.textContent('#lib-nav .tab-btn[data-tab="mixes"]'));
await shot("40-mixes-with-loose-mix");

// 2. its page: still says it is a loose track; cues work; it can be edited and played
await page.click('#lib-view table.tracks .title-cell a:has-text("tagless set")');
await page.waitForSelector("#lib-view .collection");
check("header says mix (loose track)", (await page.textContent("#lib-view .coll-meta, #lib-view .collection")).includes("mix (loose track)"));
await page.waitForSelector('#lib-view .cues-toggle:has-text("Cues (3)")');
check("3 navigable cues listed", true);
await page.locator("#lib-view li.item").first().locator("button.kebab").click();
await page.click('.menu [role="menuitem"]:has-text("Edit timestamps")');
await page.waitForFunction(() => document.getElementById("cues-dialog").open && !document.getElementById("cues-text").disabled);
check("Edit timestamps pre-filled from the file", (await page.inputValue("#cues-text")).includes("Middle part"));
await page.fill("#cues-text", "0:00 Start\n2:00 Second half");
await page.click("#cues-save");
await page.waitForFunction(() => !document.getElementById("cues-dialog").open);
const saved = readDoc(mixRel).mediaList[0].cueLists.find((l) => l.cueListType === "nav");
check("edited timestamps written to the looseTrack file", saved && saved.cues.map((c) => c.cueTextSpans[0].text).join() === "Start,Second half", saved);
check("still a looseTrack after the edit", readDoc(mixRel).noudataType === "looseTrack");
await page.click("#coll-play");
check("Play: the now-playing bar shows it", !!(await until(async () => (await page.textContent("#np-collection")).includes("tagless set"), 8000, 100)), await page.textContent("#np-collection"));
await page.waitForSelector('#lib-view .cues-toggle:has-text("Cues (2)")');
await page.click("#lib-view .cues-toggle");
await page.waitForSelector("ul.cue-list .cue-btn");
await page.click("ul.cue-list .cue-btn >> nth=1");
check("cue click seeks (Cue shown in the bar)", !!(await until(async () => (await page.textContent("#np-cue")).includes("Second half"), 8000, 100)), await page.textContent("#np-cue"));

// 3. Tracks tab does not list it
await page.goto(BASE + "/#/tracks");
await page.waitForSelector("#lib-view table.tracks tbody tr");
check("Tracks tab does not list the loose mix", (await page.locator('#lib-view table.tracks tbody tr:has-text("tagless set")').count()) === 0);

// 4. audiobook and podcast likewise; music stays in Tracks
await add("tagless book.wav", "audiobook");
await page.waitForFunction(() => location.hash === "#/audiobooks");
await page.waitForSelector('#lib-view .card:has-text("tagless book")');
check("tagless audiobook is listed under Audiobooks", true);
await add("tagless cast.wav", "podcast");
await page.waitForFunction(() => location.hash === "#/podcasts");
await page.waitForSelector('#lib-view .card:has-text("tagless cast")');
check("tagless podcast is listed under Podcasts", true);
await add("tagless song.wav", "music");
await page.waitForFunction(() => location.hash === "#/tracks");
await page.waitForSelector('#lib-view table.tracks tbody tr:has-text("tagless song")');
check("tagless music file stays under Tracks", true);
const k2 = await kinds();
check("final tab counts", k2.mix === (before.mix || 0) + 1 && k2.audiobook === (before.audiobook || 0) + 1 && k2.podcast === (before.podcast || 0) + 1 && k2.track === (before.track || 0) + 1, [before, k2]);
check("the loose long-form rows all say kind track", (await Promise.all(["mix", "audiobook", "podcast"].map((k) => list(k)))).flat().filter((c) => c.title.startsWith("tagless")).every((c) => c.kind === "track" && c.noudata_type === "looseTrack"));

// 5. phone width
await page.setViewportSize({ width: 390, height: 800 });
await page.goto(BASE + "/#/mixes");
await page.waitForSelector("#lib-view table.tracks");
check("390 px: no horizontal page scroll", await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));

check("no console errors / page errors", problems.length === 0, problems);
await browser.close();
fs.rmSync(tmp, { recursive: true, force: true });
console.log(failures ? `\n${failures} FAILED` : "\nALL OK");
process.exit(failures ? 1 : 0);
