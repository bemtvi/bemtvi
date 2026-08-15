// Focused verifier for `'guiglyphoverflow'` in the web client — wezterm's
// `allow_square_glyphs_to_overflow_width`, ported to bemtvi's option surface.
//
// A square one-cell glyph (a Nerd Font icon) is drawn by a fallback font at its own
// design size — a full em where the coding font's cell is ~0.6em — so it either spills
// over its neighbour or is shrunk to fit. Which one is the option's call: `never` always
// shrinks, `always` never does, and the default `when-followed-by-space` lets it keep its
// natural size only when the cell to its right holds a space (nothing to paint over).
//
// Two halves, because a browser's font stack decides how much the DOM can show:
//   1. The option reaches the renderer: set from Lua, read back off the live client.
//      Font-independent, so this always runs.
//   2. The rendering actually changes with it: the same glyph, once with a space after
//      it and once with a letter, across all three modes. Needs a font whose one-cell
//      icons really are over-wide and square; when the browser has none the check says
//      so and is skipped rather than passing vacuously.
//
// Drives the real wasm edit-host in headless Chromium.
//
//   node verify-glyph-overflow.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8144;

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

// Candidate one-cell glyphs, in the order we'd rather test them: two Nerd Font icons
// (what the feature exists for), then geometric shapes every DejaVu-class font carries.
// All are >= U+2190, so the client boxes them (`cellNeedsBox`).
const CANDIDATES = ["", "", "◼", "●", "◆", "■"];

// Set the option and let the frame that carries it land.
async function setMode(page, value) {
  await page.evaluate((v) => window.__bemtvi.execLua(`btv.o.guiglyphoverflow = ${JSON.stringify(v)}`), value);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(150);
}

// The inline styles of the two probe glyphs on the rendered line: `[followed by a space,
// followed by a letter]`. `scale` is the shrink-to-fit transform (1 = drawn at its
// natural size), `align` where the ink sits in its one-cell box.
async function probeStyles(page, glyph) {
  return page.evaluate((g) => {
    const spans = [...document.querySelectorAll("#grid .win .row span")]
      .filter((e) => e.textContent === g);
    return spans.map((e) => {
      const m = /scale\(([\d.]+)\)/.exec(e.style.transform || "");
      return { scale: m ? Number(m[1]) : 1, align: e.style.textAlign, indent: e.style.textIndent };
    });
  }, glyph);
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // ── 1. The option reaches the renderer ──────────────────────────────────────
  check("unset, the client renders in its own default mode (when-followed-by-space)",
    (await page.evaluate(() => window.__bemtvi.glyphOverflow())) === "space");

  for (const [set, want] of [["always", "always"], ["never", "never"],
                             ["when-followed-by-space", "space"], ["WhenFollowedBySpace", "space"]]) {
    await setMode(page, set);
    const got = await page.evaluate(() => window.__bemtvi.glyphOverflow());
    check(`btv.o.guiglyphoverflow = ${JSON.stringify(set)} reaches the renderer`, got === want,
      `got ${JSON.stringify(got)}`);
    const relayed = await page.evaluate(() => window.__bemtvi.frame().guiglyphoverflow);
    check(`  …and rides the redraw frame verbatim`, relayed === set, `frame said ${JSON.stringify(relayed)}`);
  }

  // A value no client can parse falls back to this client's own default — and doesn't
  // stick the renderer on whatever was set before. (`:set` rejects such a value with
  // E474; only a raw `btv.o` write gets here.)
  await setMode(page, "always");
  await setMode(page, "sideways");
  check("an unparseable mode falls back to the default, not the previous mode",
    (await page.evaluate(() => window.__bemtvi.glyphOverflow())) === "space");
  await setMode(page, "");

  // ── 2. The rendering follows the mode ───────────────────────────────────────
  // Pick a glyph this browser actually draws square and over-wide: without one, every
  // mode renders identically (correctly — there is nothing to shrink) and asserting on
  // the DOM would prove nothing.
  const glyph = await page.evaluate((cands) => {
    const { cw } = window.__bemtvi.cellMetrics();
    const grid = document.getElementById("grid");
    const ctx = document.createElement("canvas").getContext("2d");
    ctx.font = `${getComputedStyle(grid).fontSize} ${getComputedStyle(grid).fontFamily}`;
    for (const g of cands) {
      if (window.__bemtvi.clusterWidth(g) !== 1) continue;
      const t = ctx.measureText(g);
      const inkW = t.actualBoundingBoxLeft + t.actualBoundingBoxRight;
      const inkH = t.actualBoundingBoxAscent + t.actualBoundingBoxDescent;
      if (inkH <= 0 || inkW <= 0) continue;
      const ratio = inkW / inkH;
      if (inkW > cw && ratio >= 0.7 && ratio <= 1.4) return g;
    }
    return null;
  }, CANDIDATES);

  if (!glyph) {
    console.log("SKIP  rendering checks: this browser's fonts draw no over-wide square "
      + "one-cell glyph (every mode renders identically, which is correct)");
  } else {
    console.log(`      probe glyph U+${glyph.codePointAt(0).toString(16).toUpperCase()}`);
    // `<glyph> x<glyph>y`: the first is followed by a space, the second by a letter.
    await page.evaluate((g) => window.__bemtvi.feed(`i${g} x${g}y<Esc>`), glyph);
    await sleep(200);

    await setMode(page, "when-followed-by-space");
    let [spaced, crammed] = await probeStyles(page, glyph);
    check("default: the icon before a space keeps its natural size",
      spaced && spaced.scale === 1, JSON.stringify(spaced));
    // Pinned left, so the ink hangs into the blank on its right rather than straddling
    // both neighbours the way centring would. (The text-indent that puts the ink's left
    // edge exactly on the box's is only emitted when the glyph has a left bearing to
    // cancel; a glyph whose ink starts at the pen needs none.)
    check("default: …and is pinned left, so it grows into the blank on its right (not over both neighbours)",
      spaced && spaced.align === "left", JSON.stringify(spaced));
    check("default: the icon before a letter is shrunk into its own cell instead",
      crammed && crammed.scale < 1, JSON.stringify(crammed));

    await setMode(page, "never");
    [spaced, crammed] = await probeStyles(page, glyph);
    check("never: both icons shrink, the trailing space notwithstanding",
      spaced.scale < 1 && crammed.scale < 1, JSON.stringify([spaced, crammed]));

    await setMode(page, "always");
    [spaced, crammed] = await probeStyles(page, glyph);
    check("always: neither shrinks, even the one with a letter next to it",
      spaced.scale === 1 && crammed.scale === 1, JSON.stringify([spaced, crammed]));
  }

  await browser.close();
} catch (e) {
  console.log("FAIL  harness:", e.message);
  failures++;
}

srv.kill();
console.log(failures === 0 ? "\nAll glyph-overflow checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
