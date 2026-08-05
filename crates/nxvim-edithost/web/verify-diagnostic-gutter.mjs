// Repro/regression for the bug "diagnostic gutters still don't show up in the web
// build". The server already merges the diagnostic signs and ships them on BOTH builds
// (verify-diagnostic-signs.mjs proves the `diagnostics_signs` payload reaches the
// redraw frame), yet the gutter sign column stayed blank in the browser. The defect was
// purely client-side: every render path gated the sign column on `w.sign_column` — a key
// the server NEVER sends. The server ships the rendered column width as `sign_width`
// (cells, 0 when no column is reserved), so `w.sign_column` was always `undefined` and
// renderGutterCell()'s sign branch never ran. The payload-only test couldn't catch this
// because it never rendered the DOM. This drives a client-set diagnostic through the REAL
// web build and asserts (a) `sign_width` reaches the redraw frame and (b) the E glyph
// actually paints in the gutter, in the Error severity colour.
//
//   node verify-diagnostic-gutter.mjs   (needs a built dist/eh.mjs — run build.sh first)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8097;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  page.on("console", (m) => { if (m.type() === "error") console.log("  [console.error]", m.text()); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Type a buffer so line 1 has text the sign anchors to.
  await page.evaluate(() => window.__nxvim.feed("ggdGihello world<Esc>"));

  // Drive a client-set Error diagnostic on line 1 (0-based) — the `vim.diagnostic.set`
  // path, no LSP needed. This reserves the sign column (signcolumn=auto → width 2).
  await page.evaluate(() => window.__nxvim.execLua(`
    local ns = nx.ns.create("diag-gutter-test")
    nx.diagnostic.set(ns, 0, { { lnum = 0, col = 0, severity = 1, message = "boom" } })
  `));
  await sleep(300);
  // Nudge so a fresh frame is posted.
  await page.evaluate(() => window.__nxvim.feed("0"));
  await sleep(200);

  // (a) the server reserved a sign column and shipped the per-row sign in the frame.
  const frame = await page.evaluate(() => {
    const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused) || {};
    return { sign_width: w.sign_width, signs: (w.diagnostics_signs || []).filter(Boolean) };
  });
  check("gutter: server reserved a sign column (sign_width > 0) in the redraw frame",
    frame.sign_width > 0, `sign_width=${JSON.stringify(frame.sign_width)}`);
  check("gutter: the Error sign (glyph 'E', sev 1) reached the frame",
    frame.signs.some((c) => Array.isArray(c) && c[0] === "E" && c[1] === 1),
    `diagnostics_signs=${JSON.stringify(frame.signs)}`);

  // (b) DOM: a gutter span actually paints the 'E' glyph in the Error colour (#e06c75).
  const gutter = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")]
      .map((s) => ({ text: s.textContent, style: s.getAttribute("style").toLowerCase() }))
      .filter((s) => s.text.includes("E")));
  check("gutter: an 'E' sign glyph painted in the DOM",
    gutter.length >= 1, `E-bearing styled spans=${JSON.stringify(gutter)}`);
  check("gutter: the sign glyph used the Error severity colour (#e06c75)",
    gutter.some((s) => s.style.includes("#e06c75")), `E-bearing styled spans=${JSON.stringify(gutter)}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — diagnostic gutter signs render on the web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
