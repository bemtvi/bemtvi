// Playwright verifier for the PICKER PROGRESS READOUT on the pure web client
// (serverless).
//
// The right end of a picker's prompt row carries the count of rows on show, led by a
// spinner frame while its source is still running — the picker's only "it is working"
// signal, and the difference between a long search and an apparently frozen box.
//
// The browser is a tier-1 target, so it must animate exactly like a native session,
// and that is the interesting part here: the spinner clock is an editor-owned timer
// (`PICKER_SPIN_TIMER_ID`) riding the *Worker's* timer wheel rather than the tokio
// event loop. A leg that only armed natively would leave the web client with a frozen
// glyph — worse than none, since it reads as a hung editor.
//
// This asserts, against the real rendered DOM:
//   1. while the source runs, the prompt row ends in a spinner frame + the count so far;
//   2. the spinner actually TURNS (the Worker wheel drives it) rather than freezing;
//   3. results that arrived before the source finished are already listed;
//   4. when the run completes the spinner goes and the bare count stays.
//
// Faithfulness (not a no-op): the picker is opened through the real `btv.picker` API
// over a real async source whose completion the test controls with a promise, and
// every assertion reads the rendered `#grid` DOM rather than a mock.
//
// Prereqs: ../build.sh (../dist/eh.mjs + eh.wasm) and a Chromium for Playwright.
// Run:  node verify-picker-progress.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8167;
const SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  if (found.length) return found[found.length - 1];
  return undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

// Every row of the picker box — read straight from the DOM.
const menuRows = (page) =>
  page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return null;
    return [...box.children].map((el) => el.textContent.replace(/\s+$/, ""));
  });

// The prompt row's trailing readout (the text after the query, right-aligned).
const readout = (rows) => {
  const prompt = (rows || []).find((r) => r.startsWith(">")) || "";
  const m = prompt.match(/\s(\S+(?:\s\d+(?:\/\d+)?)?)$/);
  return m ? m[1] : "";
};

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => {
  try {
    srv.kill();
  } catch {}
};
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`http://localhost:${PORT}/web/`);
      break;
    } catch {
      await sleep(100);
    }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => {
    if (m.type() === "error") console.log("  [page error]", m.text());
  });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted (serverless)", true);

  // A source that streams two rows and then keeps working until the test lets it
  // finish — the shape of every real search that takes a while.
  await page.evaluate(() =>
    window.__bemtvi.execLua(`
      _G.gate = btv.promise.new(function(resolve) _G.open_gate = resolve end)
      btv.picker.source {
        name = "webslow",
        items = btv.async(function(ctx)
          ctx.push { text = "alpha" }
          ctx.push { text = "beta" }
          btv.await(_G.gate)
        end),
        confirm = function(item) end,
      }
    `),
  );
  await page.evaluate(() => window.__bemtvi.execLua(`btv.picker.open('webslow')`));
  await sleep(300);

  // ── 1/2. Running: a spinner frame plus the count, and the frame advances.
  const frames = new Set();
  let running = "";
  for (let i = 0; i < 10; i++) {
    const r = readout(await menuRows(page));
    if (SPINNER.includes(r[0])) {
      running = r;
      frames.add(r[0]);
    }
    await sleep(60);
  }
  check("running: the prompt row shows a spinner and the count", /^.\s2$/u.test(running), running);
  check(
    "running: the spinner animates off the Worker timer wheel",
    frames.size > 1,
    [...frames].join(""),
  );

  // ── 3. The rows pushed before the run finished are already on show.
  const midRows = await menuRows(page);
  check(
    "running: the early results are already listed",
    midRows.some((r) => r.includes("alpha")) && midRows.some((r) => r.includes("beta")),
    JSON.stringify(midRows),
  );

  // ── 4. Finished: the spinner goes, the count stays.
  await page.evaluate(() => window.__bemtvi.execLua(`_G.open_gate()`));
  let settled = "";
  for (let i = 0; i < 20; i++) {
    settled = readout(await menuRows(page));
    if (settled === "2") break;
    await sleep(50);
  }
  check("finished: the bare count, no spinner left turning", settled === "2", settled);
} finally {
  if (browser) await browser.close();
  cleanup();
}

if (failures) {
  console.log(`\n${failures} CHECK(S) FAILED`);
  process.exit(1);
}
console.log("\nALL PASS — the web client paints the picker's progress readout and animates it");
