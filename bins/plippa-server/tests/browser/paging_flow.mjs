// NB_VERIFY NB_10: lists beyond 200 rows are windowed ("Show more"), the filter still sees every row.
//   cargo build -p plippa-server --example portal_fixture
//   LIB=/tmp/lib_paging PORT=8840 node paging_flow.mjs
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "/opt/node22/lib/node_modules/playwright");
const here = path.dirname(fileURLToPath(import.meta.url));
const LIB = process.env.LIB || fs.mkdtempSync(path.join(os.tmpdir(), "plippa-paging-"));
const PORT = process.env.PORT || "8840";
const BASE = `http://127.0.0.1:${PORT}`;
const BIN = process.env.BIN || path.join(here, "../../../../target/debug/examples/portal_fixture");
let failures = 0;
const check = (n, ok, x) => { console.log(`${ok ? "PASS" : "FAIL"}  ${n}${ok || x === undefined ? "" : "  -> " + JSON.stringify(x)}`); if (!ok) failures++; };
fs.rmSync(LIB, { recursive: true, force: true });
fs.mkdirSync(path.join(LIB, "Loose"), { recursive: true });
for (let i = 0; i < 450; i++) fs.writeFileSync(path.join(LIB, "Loose", `song ${String(i).padStart(3, "0")}.mp3`), "");
const server = spawn(BIN, [LIB, PORT], { stdio: "ignore" });
process.on("exit", () => server.kill());
for (let i = 0; i < 200; i++) { try { if ((await fetch(BASE + "/api/kinds")).ok) break; } catch {} await new Promise((r) => setTimeout(r, 100)); }
const browser = await chromium.launch();
const page = await (await browser.newContext({ viewport: { width: 1200, height: 900 } })).newPage();
const errs = [];
page.on("console", (m) => m.type() === "error" && errs.push(m.text()));
page.on("pageerror", (e) => errs.push(e.message));
await page.goto(BASE + "/#/tracks");
await page.waitForSelector("table.tracks tbody tr");
const rows = () => page.locator("table.tracks tbody tr").count();
check("first page has 200 rows", (await rows()) === 200, await rows());
check("a 'Show more' button announces the 250 hidden rows", (await page.locator("button.more").textContent()).includes("250"));
await page.click("button.more");
check("second page: 400 rows", (await rows()) === 400, await rows());
await page.click("button.more");
check("third page: all 450 rows, no more button", (await rows()) === 450 && (await page.locator("button.more").count()) === 0);
await page.fill("#lib-filter", "song 449");
check("filter sees rows beyond the first page", (await rows()) === 1, await rows());
await page.fill("#lib-filter", "");
check("clearing the filter goes back to a windowed list", (await rows()) === 200, await rows());
check("no console errors", errs.length === 0, errs);
await browser.close();
console.log(failures ? `${failures} FAILED` : "ALL OK");
process.exit(failures ? 1 : 0);
