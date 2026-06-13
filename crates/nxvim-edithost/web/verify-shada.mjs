// Playwright verifier for serverless shada persistence (cross-session state over OPFS).
// The editor's registers / search & ex history / etc. are exported to a single JSON blob
// in OPFS and restored at boot. This drives real state into the editor, flushes it, RELOADS
// the page (a fresh Worker + editor, but the same origin's OPFS), and asserts the state came
// back — so it can only have survived through storage, not in-memory.
//
// Runs over the SAB transport (cross-origin isolated), exercising the run loop's debounced
// checkpoint + the flush-with-exit-cursor path.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-shada.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8101;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// Wait for window.__nxvim to exist + be ready (after the initial load and after a reload).
async function waitReady(page) {
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  // A single context across the reload, so its OPFS persists (shada lives there).
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await waitReady(page);

  // Start from a clean OPFS so a previous run's blob can't mask a regression.
  await page.evaluate(async () => {
    try { await (await navigator.storage.getDirectory()).removeEntry(".nxvim", { recursive: true }); } catch {}
  });

  // ── Set cross-session state in session #1 ────────────────────────────────────────────
  // A register: type a marker line and yank it (linewise) into register `a`.
  await page.evaluate(() => window.__nxvim.feed("ggdGiHELLO-REG-LINE<Esc>"));
  await page.evaluate(() => window.__nxvim.feed('"ayy'));
  // Search history: a distinctive `/` search (matches the buffer, so it's a real search).
  await page.evaluate(() => window.__nxvim.feed("/HELLO-REG<CR>"));
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  // Ex history: a harmless ex command.
  await page.evaluate(() => window.__nxvim.feed(":set wrap<CR>"));

  // Flush shada to OPFS and wait for the write to land (resolves on `shada_written`).
  const flushed = await page.evaluate(() => window.__nxvim.shadaFlush().then(() => true).catch((e) => String(e)));
  check("flush: shada written to OPFS (shada_written acked)", flushed === true, `flush=${JSON.stringify(flushed)}`);

  // Confirm the blob is actually in OPFS (a path the editor's persistence wrote).
  const blob = await page.evaluate(async () => {
    try {
      const dir = await (await navigator.storage.getDirectory()).getDirectoryHandle(".nxvim");
      const fh = await dir.getFileHandle("shada");
      return await (await fh.getFile()).text();
    } catch (e) { return `ERR:${e}`; }
  });
  check("storage: the shada blob exists in OPFS", blob.includes("HELLO-REG-LINE"), `blob[0..80]=${JSON.stringify(blob.slice(0, 80))}`);

  // ── Reload → fresh Worker + editor, same OPFS ────────────────────────────────────────
  await page.reload();
  await waitReady(page);

  // The reloaded buffer is a fresh [No Name] — prove the state came from storage, not memory.
  const freshLines = await page.evaluate(() => window.__nxvim.lines());
  check("reload: starts from a fresh empty buffer", freshLines === "", `lines=${JSON.stringify(freshLines)}`);

  // Register `a` survived: paste it into the fresh buffer.
  await page.evaluate(() => window.__nxvim.feed('"ap'));
  const pasted = await page.evaluate(() => window.__nxvim.lines());
  check(
    "restore: register `a` survived the reload (paste yields the yanked line)",
    pasted.includes("HELLO-REG-LINE"),
    `lines=${JSON.stringify(pasted)}`,
  );

  // Search history survived: `/` then `<Up>` recalls the last search into the search line.
  await page.evaluate(() => window.__nxvim.feed("/"));
  await page.evaluate(() => window.__nxvim.feed("<Up>"));
  const searchLine = await page.evaluate(() => window.__nxvim.cmdline());
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  check(
    "restore: search history survived the reload (`/<Up>` recalls it)",
    searchLine.includes("HELLO-REG"),
    `cmdline=${JSON.stringify(searchLine)}`,
  );

  // Ex history survived: `:` then `<Up>` recalls the last ex command.
  await page.evaluate(() => window.__nxvim.feed(":"));
  await page.evaluate(() => window.__nxvim.feed("<Up>"));
  const exLine = await page.evaluate(() => window.__nxvim.cmdline());
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
  check(
    "restore: ex history survived the reload (`:<Up>` recalls it)",
    exLine.includes("set wrap"),
    `cmdline=${JSON.stringify(exLine)}`,
  );

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless shada (registers + search/ex history) persisted across a page reload via OPFS"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
