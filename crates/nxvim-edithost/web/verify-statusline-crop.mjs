// Regression verifier for the web statusline crop bug: a status / tabline segment
// containing a Nerd-Font glyph used to be dumped into a flowing <span>, so a fallback
// font drew the glyph wider than its one server column and dragged the rest of the bar
// past its `width*cw` box — the last few characters got clipped off the right edge
// (lualine-on-web "cropped ~3-4 chars"). The fix routes segment text through the same
// boxed-cell model the content lines use (`styledCellsHtml` → `cellBoxSpan`): every
// Nerd glyph draws in a fixed `cells*cw` box, so the column model is exact and nothing
// overflows. Drives the real wasm edit-host in headless Chromium.
//
//   node verify-statusline-crop.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8099;
const GLYPH = ""; // Nerd-Font branch icon (PUA, server-counted as ONE column)

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
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // A per-window statusline (laststatus=2 — the default lualine/nxvim-line scope, and
  // the one that renders on the pure wasm build) whose content has a Nerd glyph on the
  // left, then `%=` so "ENDMARK" is right-aligned hard against the bar's right edge —
  // exactly where the crop bit. Setting `'statusline'` makes the server ship styled
  // segments (`w.status`) on the wasm build, taking the `renderSegmentBar` path.
  await page.evaluate((g) => window.__nxvim.execLua(
    "vim.o.laststatus = 2\n" +
    `vim.o.statusline = " ${g} branch  %= ENDMARK "`), GLYPH);
  await sleep(250);

  const r = await page.evaluate((g) => {
    const { cw } = window.__nxvim.cellMetrics();
    const bars = [...document.querySelectorAll("#grid .row.statusline")];
    // The status bar is the one carrying our content (the glyph + ENDMARK).
    const bar = bars.find((b) => b.textContent.includes(g));
    if (!bar) return { found: false, barCount: bars.length };
    const spans = [...bar.querySelectorAll("span")];
    const glyphSpan = spans.find((s) => s.textContent.includes(g));
    const gs = glyphSpan ? getComputedStyle(glyphSpan) : null;
    const barBox = bar.getBoundingClientRect();
    // Right edge of the painted content vs the bar's right edge (the crop boundary).
    const contentRight = Math.max(...spans.map((s) => s.getBoundingClientRect().right));
    return {
      found: true,
      cw,
      text: bar.textContent,
      hasEndmark: bar.textContent.includes("ENDMARK"),
      glyphDisplay: gs ? gs.display : null,
      glyphIsolated: glyphSpan ? glyphSpan.textContent === g : false,
      glyphWidth: glyphSpan ? glyphSpan.getBoundingClientRect().width : null,
      scrollWidth: bar.scrollWidth,
      clientWidth: bar.clientWidth,
      overflowRight: contentRight - barBox.right,
    };
  }, GLYPH);

  check("statusline: global bar with the glyph renders", r.found, JSON.stringify(r));
  if (r.found) {
    // Mechanism: the Nerd glyph is now its own fixed-width box (was a flowing span).
    check("statusline: glyph drawn in its own boxed cell", r.glyphIsolated, JSON.stringify(r));
    check("statusline: glyph cell is inline-block", r.glyphDisplay === "inline-block", JSON.stringify(r));
    check("statusline: glyph box is exactly one column wide",
      r.glyphWidth != null && Math.abs(r.glyphWidth - r.cw) < 0.6, JSON.stringify(r));
    // Symptom: nothing overflows the bar — the content fits in its `width*cw` box, so
    // the right-aligned ENDMARK is fully painted and not clipped.
    check("statusline: ENDMARK present (not cropped away)", r.hasEndmark, JSON.stringify(r));
    check("statusline: content does not overflow the bar (no crop)",
      r.scrollWidth <= r.clientWidth + 1 && r.overflowRight < 1, JSON.stringify(r));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
