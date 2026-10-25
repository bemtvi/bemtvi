// Repro/regression for the bug "the web version doesn't draw diagnostic decorations":
// on the wasm build the squiggle underlines (`diagnostics`) and the inline message
// (`diagnostics_virt`) never reached the screen. Two defects, both fixed here:
//   1. redraw.rs gated both payloads `#[cfg(feature = "native")]`, so the wasm build
//      shipped empty arrays (the gutter SIGN merge had already been un-gated; these two
//      overlays were left behind even though they read the same core/tick-shared
//      `diagnostics_merged` store, which includes client-set `btv.diagnostic.set`).
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
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // Type a buffer so line 1 has text to underline.
  await page.evaluate(() => window.__bemtvi.feed("ggdGihello world<Esc>"));

  // Drive a client-set Error diagnostic over cols 0..5 on line 1 (0-based), with
  // virtual_text enabled (it's off by default in neovim 0.10). This is exactly the
  // `vim.diagnostic.set` / `vim.diagnostic.config` path — no LSP needed.
  await page.evaluate(() => window.__bemtvi.execLua(`
    btv.diagnostic.config({ virtual_text = true })
    local ns = btv.ns.create("diag-deco-test")
    btv.diagnostic.set(ns, 0, { { lnum = 0, col = 0, end_col = 5, severity = 1, message = "boom" } })
  `));
  await sleep(300);
  // Nudge so a fresh frame is posted.
  await page.evaluate(() => window.__bemtvi.feed("0"));
  await sleep(200);

  // (a)+(b) render-side: both overlays reached the redraw frame the renderer paints.
  const frame = await page.evaluate(() => {
    const w = (window.__bemtvi.frame()?.windows || []).find((x) => x.focused) || {};
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

  // (c) DOM: a span actually paints the wavy underline in the error colour, the
  //     default `bemtvi` scheme's `DiagnosticUnderlineError` (#f08a7e).
  const decos = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")]
      .map((s) => s.getAttribute("style").toLowerCase())
      .filter((st) => st.includes("underline") && st.includes("wavy")));
  check("diag: the squiggle (underline wavy) painted in the DOM",
    decos.length >= 1, `wavy spans=${JSON.stringify(decos)}`);
  check("diag: the squiggle used the Error severity colour (#f08a7e)",
    decos.some((st) => st.includes("#f08a7e")), `wavy spans=${JSON.stringify(decos)}`);

  // (d) DOM: the trailing virtual-text message actually rendered.
  const texts = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span")].map((s) => s.textContent));
  check("diag: the virtual-text message ('boom') rendered in the DOM",
    texts.some((t) => t && t.includes("boom")), `row spans=${JSON.stringify(texts)}`);

  // (d2) The message line shows the diagnostic under the cursor, as natively.
  const underMsg = await page.evaluate(async () => {
    const r = await window.__bemtvi.feed("gg0");
    return r.frame?.message ?? null;
  });
  check("diag: the message line shows the diagnostic under the cursor ('boom')",
    String(underMsg).includes("boom"), `message=${JSON.stringify(underMsg)}`);

  // (e) A scroll slide's band carries the same overlays as the settled window, so the
  //     squiggle and the message slide with the text instead of blanking for the slide.
  await page.evaluate(() => window.__bemtvi.execLua(`
    local lines = { "hello world" }
    for i = 2, 200 do lines[i] = "line " .. i end
    btv.buf.set_lines(0, 0, -1, false, lines)
  `));
  await sleep(200);
  await page.evaluate(() => window.__bemtvi.execLua(`
    local ns = btv.ns.create("diag-deco-test")
    btv.diagnostic.set(ns, 0, { { lnum = 0, col = 0, end_col = 5, severity = 1, message = "boom" } })
  `));
  await page.evaluate(() => window.__bemtvi.feed(":set scrollanim<CR>gg0"));
  await sleep(200);
  const band = await page.evaluate(async () => {
    const r = await window.__bemtvi.feed("<C-d>");
    const w = (r.frame?.windows || []).find((x) => x.focused) || {};
    const s = w.scroll;
    if (!s) return null;
    return {
      diag: (s.diagnostics || []).flat(),
      virt: (s.diagnostics_virt || []).filter(Boolean),
    };
  });
  check("band: the <C-d> frame carries a scroll band", band !== null, "no `scroll` on the frame");
  check("band: the diagnostic underline rides the scroll band",
    !!band && band.diag.some((s) => Array.isArray(s) && s[0] === 0 && s[1] === 5 && s[2] === 1),
    `band.diagnostics=${JSON.stringify(band?.diag)}`);
  check("band: the diagnostic virtual text rides the scroll band",
    !!band && band.virt.some((v) => Array.isArray(v) && String(v[0]).includes("boom")),
    `band.diagnostics_virt=${JSON.stringify(band?.virt)}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — diagnostic underline + virtual text render on the web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
