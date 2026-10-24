// Playwright verifier for `'guifont'` in the web edit-host: the font list, tried in
// order for each character, its size, and each font's own options.
//
// The page loads each font in the list as a `FontFace` over `local()` sources (its
// weight's face, its scale as `size-adjust`), applies the primary's OpenType
// features to the grid, re-measures the cell for the size, and reports on the message
// line what isn't installed or can't apply in a browser. This drives it with fonts the
// machine has (Noto Sans Mono, Symbols Nerd Font Mono), skipping when it hasn't them,
// and one that no machine has.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-guifont.mjs
import { chromium } from "playwright";
import { spawn, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8175;
const ORIGIN = `http://localhost:${PORT}`;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const pats = [
    `${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`,
    `${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/*.app/Contents/MacOS/*`,
  ];
  for (const p of pats) {
    const found = globSync(p).sort();
    if (found.length) return found[found.length - 1];
  }
  return undefined;
}

function installed(family) {
  try {
    return execFileSync("fc-list", [`:family=${family}`, "family"], { encoding: "utf8" }).trim() !== "";
  } catch {
    return false;
  }
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

if (!installed("Noto Sans Mono") || !installed("Symbols Nerd Font Mono")) {
  console.log("skip: needs Noto Sans Mono and Symbols Nerd Font Mono installed");
  process.exit(0);
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

async function luaStr(page, code) {
  const rendered = await page.evaluate(
    (c) => window.__bemtvi.execLua(c).then((r) => String(r.result)), code);
  if (!rendered.startsWith("ok:")) return null;
  const m = rendered.match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1] : null;
}

// CSS reads back normalized: quotes dropped around a plain name, a feature's default
// value `1` dropped.
const unquote = (css) => css.replace(/"/g, "");

const grid = (page) => page.evaluate(() => {
  const g = document.getElementById("grid");
  return {
    family: g.style.fontFamily,
    features: g.style.fontFeatureSettings,
    ch: g.style.getPropertyValue("--ch"),
    faces: [...document.fonts].filter((f) => f.family.startsWith("btv-font"))
      .map((f) => ({ family: f.family, sizeAdjust: f.sizeAdjust, status: f.status })),
    message: window.__bemtvi.message(),
  };
});

async function waitGrid(page, ok) {
  let g = null;
  for (let i = 0; i < 60; i++) {
    g = await grid(page);
    if (ok(g)) return g;
    await sleep(50);
  }
  return g;
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  await page.goto(`${ORIGIN}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  let g = await grid(page);
  check("no guifont: the page's own font", g.family === "" && g.ch === "18px", JSON.stringify(g));

  await luaStr(page,
    "btv.o.guifont = 'Noto Sans Mono[weight=bold],Symbols Nerd Font Mono[scale=0.8;features=+zero],"
    + "Nonexistent Font Xyz:h18' return 'ok'");
  g = await waitGrid(page, (x) => x.family.includes("btv-font-1") && x.message.includes("guifont"));
  check("the list becomes the grid's font family, in order",
    unquote(g.family) === "btv-font-0, btv-font-1, monospace", JSON.stringify(g));
  check("each installed font loaded from its local() face",
    g.faces.length === 2 && g.faces.every((f) => f.status === "loaded"), JSON.stringify(g.faces));
  check("a font's scale is its size-adjust",
    g.faces.find((f) => f.family === "btv-font-1")?.sizeAdjust === "80%", JSON.stringify(g.faces));
  check("the size sets the cell", g.ch === "25px", g.ch);
  check("a font that isn't installed is reported",
    g.message.includes("not installed: Nonexistent Font Xyz"), JSON.stringify(g.message));
  check("…and so are features the browser can't give a later font",
    g.message.includes("features on Symbols Nerd Font Mono"), JSON.stringify(g.message));

  await luaStr(page, "btv.o.guifont = 'Noto Sans Mono[features=+zero -calt]' return 'ok'");
  g = await waitGrid(page, (x) => x.features !== "");
  check("the primary's features apply to the grid",
    unquote(g.features).replace(/ 1(,|$)/g, "$1") === "zero, calt 0" && g.ch === "18px",
    JSON.stringify(g));

  await luaStr(page, "btv.o.guifont = '' return 'ok'");
  g = await waitGrid(page, (x) => x.family === "");
  check("clearing guifont goes back to the page's own font",
    g.family === "" && g.features === "" && g.faces.length === 0, JSON.stringify(g));

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
