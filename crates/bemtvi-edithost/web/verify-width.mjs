// Focused verifier for the web client's COLUMN MODEL: the display width it assigns
// each grapheme cluster must match the server's `unicode-width` exactly, or the cell
// grid the DOM builds sits on different columns than the highlight / selection /
// cursor spans the wire is measured in — and a line of emoji colours the wrong glyphs.
//
// Three checks, cheapest first:
//   1. `clusterWidth` against `width-corpus.json` (GENERATED from the server's own
//      `unicode-width`, see `crates/bemtvi-core/examples/width_corpus.rs`).
//   2. The whole-line widths, so the segmentation into clusters agrees too.
//   3. The real DOM: a visual selection over a known column range on a line mixing an
//      emoji-modifier cluster and a VS16 emoji must land on exactly the right glyphs.
//
// Drives the real wasm edit-host in headless Chromium.
//
//   node verify-width.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8131;
const corpus = JSON.parse(readFileSync(`${here}width-corpus.json`, "utf8"));

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

// The reported line, minus its comment marker: an emoji-modifier cluster (2 chars whose
// per-char widths are 2 each, but 2 cells), then `b`, then a VS16 heart (2 chars of
// width 1 and 0, but 2 cells), then plain ASCII. Columns: cluster 0–1, b 2, heart 3–4,
// then `emtvi` at 5–9. A per-codepoint walk drifts +2 on the first and −1 on the second.
const LINE = "\u{1f934}\u{1f3fc}b\u{2764}\u{fe0f}emtvi";

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

  // 1. Per-cluster widths.
  const clusterBad = await page.evaluate((cs) =>
    cs.filter((c) => window.__bemtvi.clusterWidth(c.cluster) !== c.width)
      .map((c) => `${JSON.stringify(c.cluster)} want ${c.width} got ${window.__bemtvi.clusterWidth(c.cluster)}`),
    corpus.clusters);
  check(`clusterWidth matches the server for all ${corpus.clusters.length} corpus clusters`,
    clusterBad.length === 0, clusterBad.slice(0, 6).join("; "));

  // 2. Whole-line widths — this also pins the segmentation, since a line's width is the
  //    sum over the clusters the client split it into.
  const lineBad = await page.evaluate((ls) =>
    ls.map((l) => {
      const got = window.__bemtvi.clusters(l.text)
        .reduce((n, g) => n + window.__bemtvi.clusterWidth(g), 0);
      return got === l.width ? null : `${JSON.stringify(l.text)} want ${l.width} got ${got}`;
    }).filter(Boolean),
    corpus.lines);
  check(`summed cluster widths match the server for all ${corpus.lines.length} corpus lines`,
    lineBad.length === 0, lineBad.slice(0, 6).join("; "));

  // 3. The rendered grid: select `emtvi` (columns 5–9) and read back which glyphs the
  //    `.sel` cells actually carry. `0fe` lands on the first `e` — the start of `emtvi`
  //    — and `v$` extends to end of line. The cursor owns the last cell and paints as
  //    the cursor rather than the selection, so `emtv` is the selected run. Walking
  //    codepoints instead of clusters puts the heart at column 5 and selects
  //    `\u{2764}\u{fe0f}emtv`; drifting the other way selects `mtvi`.
  await page.evaluate((line) => window.__bemtvi.feed(`i${line}<Esc>`), LINE);
  await page.evaluate(() => window.__bemtvi.feed("0fev$"));
  await sleep(200);
  const selected = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row .sel")].map((e) => e.textContent).join(""));
  check("a selection anchored at column 5 lands on exactly `emtv`",
    selected === "emtv", `selected ${JSON.stringify(selected)}`);

  await browser.close();
} catch (e) {
  console.log("FAIL  harness:", e.message);
  failures++;
}

srv.kill();
console.log(failures === 0 ? "\nAll width checks passed." : `\n${failures} check(s) failed.`);
process.exit(failures === 0 ? 0 : 1);
