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
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // A per-window statusline (laststatus=2 — the default lualine/bemtvi-line scope, and
  // the one that renders on the pure wasm build) whose content has a Nerd glyph on the
  // left, then `%=` so "ENDMARK" is right-aligned hard against the bar's right edge —
  // exactly where the crop bit. Setting `'statusline'` makes the server ship styled
  // segments (`w.status`) on the wasm build, taking the `renderSegmentBar` path.
  await page.evaluate((g) => window.__bemtvi.execLua(
    "vim.o.laststatus = 2\n" +
    `vim.o.statusline = " ${g} branch  %= ENDMARK "`), GLYPH);
  await sleep(250);

  const r = await page.evaluate((g) => {
    const { cw } = window.__bemtvi.cellMetrics();
    const bars = [...document.querySelectorAll("#grid .row.statusline")];
    // The status bar is the one carrying our content (the glyph + ENDMARK).
    const bar = bars.find((b) => b.textContent.includes(g));
    if (!bar) return { found: false, barCount: bars.length };
    // Leaf cell/run spans only (skip the two `.sl-layer` wrappers).
    const spans = [...bar.querySelectorAll("span")].filter((s) => !s.classList.contains("sl-layer"));
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

  // ---- wide-glyph fit: a two-column glyph drawn wider than its `2*cw` box used to
  // overflow on the right and get overpainted by the next segment's background ("wide
  // glyphs partially covered on the right"). It must now be scaled to fit its box.
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.o.laststatus = 2\n" +
    "vim.o.statusline = 'L🚀R %= W'")); // 🚀 = emoji, ink wider than two cells
  await sleep(200);
  const w = await page.evaluate(() => {
    const { cw } = window.__bemtvi.cellMetrics();
    const bar = [...document.querySelectorAll("#grid .row.statusline")].find((b) => b.textContent.includes("🚀"));
    if (!bar) return { found: false };
    const span = [...bar.querySelectorAll("span")].filter((s) => !s.classList.contains("sl-layer")).find((s) => s.textContent.includes("🚀"));
    if (!span) return { found: false };
    const boxW = 2 * cw;
    // Measure the glyph's rasterised ink (the same metric the fix keys on), then apply
    // the span's transform scale: the scaled ink must fit inside its two-cell box.
    const ctx = document.createElement("canvas").getContext("2d");
    ctx.font = `${getComputedStyle(bar).fontSize} ${getComputedStyle(bar).fontFamily}`;
    const m = ctx.measureText("🚀");
    const inkW = m.actualBoundingBoxLeft + m.actualBoundingBoxRight;
    const tr = getComputedStyle(span).transform; // "none" or "matrix(a, ...)"
    const scale = tr && tr.startsWith("matrix") ? parseFloat(tr.slice(7).split(",")[0]) : 1;
    return { found: true, boxW, inkW, scale, scaledInk: inkW * scale, display: getComputedStyle(span).display };
  });
  check("statusline: wide glyph renders", w.found, JSON.stringify(w));
  if (w.found) {
    check("statusline: wide glyph is a boxed cell", w.display === "inline-block", JSON.stringify(w));
    check("statusline: wide glyph ink fits its two-cell box (not covered)",
      w.scaledInk <= w.boxW + 0.6, JSON.stringify(w));
  }

  // ---- two-layer bar: backgrounds first, glyph text on top, so an over-wide width-1
  // glyph (powerline separator / Nerd icon) overhangs instead of being overpainted by
  // the next segment's background. A statusline with a Nerd separator + a coloured
  // segment after it; assert the bar is two stacked layers and the glyph is in the
  // upper (text) layer while all backgrounds sit in the lower layer.
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, 'SEP', { fg='#ff0000' })\n" +
    "vim.api.nvim_set_hl(0, 'BTVT', { fg='#000000', bg='#00ddaa' })\n" +
    "vim.o.laststatus = 2\n" +
    "vim.o.statusline = 'A%#SEP#\\u{e0b0}%#BTVT#BBB'")); // U+E0B0 = powerline separator
  await sleep(200);
  const L = await page.evaluate(() => {
    const bar = [...document.querySelectorAll("#grid .row.statusline")].find((b) => b.textContent.includes("\u{e0b0}"));
    if (!bar) return { found: false };
    const layers = [...bar.children].filter((c) => c.classList.contains("sl-layer"));
    if (layers.length !== 2) return { found: true, layerCount: layers.length };
    const bgLayer = layers[0], fgLayer = layers[1];
    const bgText = bgLayer.textContent;
    const sepInFg = [...fgLayer.querySelectorAll("span")].some((s) => s.textContent === "\u{e0b0}");
    // Background rectangles carry no text; the separator glyph is in the upper layer.
    return { found: true, layerCount: 2, bgEmpty: bgText.length === 0, sepInFg };
  });
  check("statusline: bar renders two stacked layers", L.found && L.layerCount === 2, JSON.stringify(L));
  if (L.layerCount === 2) {
    check("statusline: background layer carries no glyphs", L.bgEmpty, JSON.stringify(L));
    check("statusline: glyph sits in the upper (text) layer, above backgrounds", L.sepInFg, JSON.stringify(L));
  }

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
