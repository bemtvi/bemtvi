// Playwright verifier: a grabbing float opened at startup must NOT leave the file
// behind it un-highlighted (the serverless-web half of the native
// `highlight_under_float` regression).
//
// An `init.lua` opens a rust file and then `nx.schedule`s a grabbing `nx.view` float —
// so the float grabs focus on the first convergence and the file behind it is *never*
// the focused buffer. The web client highlights each window from its own buffer text;
// `eh_lines` only ships the FOCUSED buffer, so before the fix the background file had
// no text for the JS highlighter and rendered dark until the float closed. The fix
// ships every visible background buffer's text too (`eh_aux_lines` → `aux` → the UI's
// `textByFile` cache), so the file behind the float colours in while it's still open.
//
// Hermetic: rust is a BUNDLED grammar (offline, no CDN). Companion to verify-treesitter
// (install/persistence) and verify-ui (renderer).
//
//   node verify-float-highlight.mjs
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
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

// Write `text` to OPFS path `/name` from inside the page.
async function writeOpfs(page, name, text) {
  await page.evaluate(async ({ name, text }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(name, { create: true });
    const w = await fh.createWritable();
    await w.write(text);
    await w.close();
  }, { name, text });
}

// Poll until some window paints a colored span (~6s), and report which window.
async function waitColored(page) {
  let detail = "";
  for (let i = 0; i < 60; i++) {
    const r = await page.evaluate(() => {
      const spans = [...document.querySelectorAll("#grid .win .row span[style]")];
      const styled = spans.filter((s) => /color\s*:/.test(s.getAttribute("style")));
      return { any: styled.length, sample: styled.slice(0, 6).map((s) => s.textContent) };
    });
    if (r.any > 0) return { ok: true, detail: JSON.stringify(r.sample) };
    detail = JSON.stringify(r);
    await sleep(100);
  }
  return { ok: false, detail };
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // First load to get an OPFS scope, then seed the file + config and reload so the
  // Worker boots fresh and sources the config at startup.
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  await writeOpfs(page, "demo.rs", "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n");
  await writeOpfs(page, "init.lua", [
    "nx.cmd('edit demo.rs')",
    "nx.schedule(function()",
    "  local vw = nx.view.create{}",
    "  vw:set_lines{ 'checklist dialog' }",
    "  vw:mount{ float = { width = 30, height = 4, grab = true } }",
    "end)",
  ].join("\n"));

  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // The grabbing float must be open AND focused (so demo.rs behind it is never focused).
  let scenario = { wins: 0, focusedUnnamed: false };
  for (let i = 0; i < 60; i++) {
    scenario = await page.evaluate(() => {
      const wins = window.__nxvim.frame()?.windows || [];
      const fw = wins.find((w) => w.focused);
      return { wins: wins.length, focusedUnnamed: !!fw && !!fw.unnamed };
    });
    if (scenario.wins >= 2 && scenario.focusedUnnamed) break;
    await sleep(100);
  }
  check("scenario: a grabbing float is open and focused over the file",
    scenario.wins >= 2 && scenario.focusedUnnamed, JSON.stringify(scenario));

  // The file behind the float colours in while the float is still up.
  const colored = await waitColored(page);
  check("background demo.rs is highlighted while the grabbing float is open",
    colored.ok, colored.detail);

  await browser.close();
} finally {
  cleanup();
}

process.exit(failures ? 1 : 0);
