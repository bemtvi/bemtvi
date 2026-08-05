// Repro/regression for the bug "the web version doesn't draw diagnostic decorations":
// on the wasm build the squiggle underlines (`diagnostics`) and the inline message
// (`diagnostics_virt`) never reached the screen. Two defects, both fixed here:
//   1. redraw.rs gated both payloads `#[cfg(feature = "native")]`, so the wasm build
//      shipped empty arrays (the gutter SIGN merge had already been un-gated; these two
//      overlays were left behind even though they read the same core/tick-shared
//      `diagnostics_merged` store, which includes client-set `nx.diagnostic.set`).
//   2. the wasm render path `renderLine()` never read `w.diagnostics` /
//      `w.diagnostics_virt` (only the server-styled `renderLineServer()` did).
// This drives a client-set diagnostic through the REAL web build and asserts (a) the
// underline span + (b) the virtual text reach the redraw frame, then (c) the wavy
// underline + (d) the trailing message actually paint in the DOM.
//
//   node verify-diagnostic-decorations.mjs   (needs a built dist/eh.mjs — run build.sh first)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8096;

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

  // Type a buffer so line 1 has text to underline.
  await page.evaluate(() => window.__nxvim.feed("ggdGihello world<Esc>"));

  // Drive a client-set Error diagnostic over cols 0..5 on line 1 (0-based), with
  // virtual_text enabled (it's off by default in neovim 0.10). This is exactly the
  // `vim.diagnostic.set` / `vim.diagnostic.config` path — no LSP needed.
  await page.evaluate(() => window.__nxvim.execLua(`
    nx.diagnostic.config({ virtual_text = true })
    local ns = nx.ns.create("diag-deco-test")
    nx.diagnostic.set(ns, 0, { { lnum = 0, col = 0, end_col = 5, severity = 1, message = "boom" } })
  `));
  await sleep(300);
  // Nudge so a fresh frame is posted.
  await page.evaluate(() => window.__nxvim.feed("0"));
  await sleep(200);

  // (a)+(b) render-side: both overlays reached the redraw frame the renderer paints.
  const frame = await page.evaluate(() => {
    const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused) || {};
    const diag = (w.diagnostics || []).flat();
    const virt = (w.diagnostics_virt || []).filter(Boolean);
    return { diag, virt };
  });
  check("diag: underline span (cols 0..5, sev 1) reached the redraw frame",
    frame.diag.some((s) => Array.isArray(s) && s[0] === 0 && s[1] === 5 && s[2] === 1),
    `frame.diagnostics=${JSON.stringify(frame.diag)}`);
  check("diag: virtual text ('boom', sev 1) reached the redraw frame",
    frame.virt.some((v) => Array.isArray(v) && String(v[0]).includes("boom") && v[1] === 1),
    `frame.diagnostics_virt=${JSON.stringify(frame.virt)}`);

  // (c) DOM: a span actually paints the wavy underline in the error colour (#e06c75).
  const decos = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")]
      .map((s) => s.getAttribute("style").toLowerCase())
      .filter((st) => st.includes("underline") && st.includes("wavy")));
  check("diag: the squiggle (underline wavy) painted in the DOM",
    decos.length >= 1, `wavy spans=${JSON.stringify(decos)}`);
  check("diag: the squiggle used the Error severity colour (#e06c75)",
    decos.some((st) => st.includes("#e06c75")), `wavy spans=${JSON.stringify(decos)}`);

  // (d) DOM: the trailing virtual-text message actually rendered.
  const texts = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span")].map((s) => s.textContent));
  check("diag: the virtual-text message ('boom') rendered in the DOM",
    texts.some((t) => t && t.includes("boom")), `row spans=${JSON.stringify(texts)}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — diagnostic underline + virtual text render on the web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
