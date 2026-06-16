// Playwright verifier for image previews (`'imagepreview'`) in the web edit-host.
//
// A preview window carries an `image` marker ({ path, size, mtime_ms }) instead of text — the
// editor core never reads the bytes (the never-freeze invariant), so the UI fetches them
// out-of-band and paints an <img> over the window body. This drives the full path in a real
// browser: seed an image into OPFS, turn the option on, `:e` it, and assert (a) the redraw
// frame carries the marker, (b) a real <img> with a blob: URL renders and decodes (its
// naturalWidth > 0), (c) the option off opens the same file as text with no marker, and (d) a
// non-decodable file paints the loud `[image: …]` placeholder rather than a silent blank.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-images.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8103;

// A valid 1×1 PNG (red). Small is fine — the assertions are about the pipeline (marker →
// fetch → <img> decode), not the picture's content.
const PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

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

async function until(page, fn, pred, ms = 5000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Seed two files into OPFS (shared origin storage the editor's Worker reads too): a real PNG
  // and a non-image file with a `.png` extension (so it opens as a preview but fails to decode).
  await page.evaluate(async (b64) => {
    const root = await navigator.storage.getDirectory();
    const pngBytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    let fh = await root.getFileHandle("sample.png", { create: true });
    let w = await fh.createWritable(); await w.write(pngBytes); await w.close();
    fh = await root.getFileHandle("broken.png", { create: true });
    w = await fh.createWritable(); await w.write(new TextEncoder().encode("this is not a PNG")); await w.close();
    // A second non-image `.png`, opened fresh while the option is on (step 4) — a re-`:e` of
    // an already-open buffer switches to it without re-reading, so the fail-loud case needs a
    // file not opened earlier under the option-off path.
    fh = await root.getFileHandle("corrupt.png", { create: true });
    w = await fh.createWritable(); await w.write(new TextEncoder().encode("also not a PNG")); await w.close();
  }, PNG_B64);

  // ── 1. Option OFF (default): an image file opens as TEXT, no marker ───────────────────
  await page.evaluate(() => window.__nxvim.feed(":e /broken.png<CR>"));
  const offFrame = await until(
    page,
    () => { const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused); return w ? { img: !!w.image, lines: window.__nxvim.lines() } : null; },
    (v) => v && v.lines && v.lines.includes("not a PNG"),
  );
  check(
    "option off: an image-extension file opens as text, no image marker",
    offFrame && offFrame.img === false && offFrame.lines.includes("not a PNG"),
    `frame=${JSON.stringify(offFrame)}`,
  );

  // ── 1b. `:set imagepreview` (the ex-command path the user reaches) enables it ─────────
  // Regression guard: this arm was missing from `apply_set_bool`, so `:set imagepreview`
  // silently no-op'd and an image still opened as text. Seed + open a fresh file after it.
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const png = Uint8Array.from(atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="), (c) => c.charCodeAt(0));
    const fh = await root.getFileHandle("viaset.png", { create: true });
    const w = await fh.createWritable(); await w.write(png); await w.close();
  });
  await page.evaluate(() => window.__nxvim.feed(":set imagepreview<CR>"));
  await page.evaluate(() => window.__nxvim.feed(":e /viaset.png<CR>"));
  const viaSet = await until(
    page,
    () => { const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused); return w && w.image ? { path: w.image.path } : null; },
    (v) => v != null,
  );
  check(
    "`:set imagepreview` enables previews (the ex-command path), not just nx.o",
    viaSet != null && /viaset\.png$/.test(viaSet.path),
    `viaSet=${JSON.stringify(viaSet)}`,
  );

  // Keep the option on via the canonical nx.* surface for the remaining checks.
  await page.evaluate(() => window.__nxvim.execLua("nx.o.imagepreview = true"));

  // ── 2. Option ON: opening the PNG carries the marker AND an empty buffer ──────────────
  await page.evaluate(() => window.__nxvim.feed(":e /sample.png<CR>"));
  const marker = await until(
    page,
    () => {
      const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused);
      return w && w.image ? { path: w.image.path, size: w.image.size, lines: window.__nxvim.lines() } : null;
    },
    (v) => v != null,
  );
  check(
    "option on: opening a PNG carries the image marker (path + version), bytes not loaded as text",
    marker != null && /sample\.png$/.test(marker.path) && (marker.lines === "" || marker.lines == null),
    `marker=${JSON.stringify(marker)}`,
  );

  // ── 3. The UI fetched the bytes and a real <img> rendered and DECODED ─────────────────
  const imgState = await until(
    page,
    () => {
      const img = document.querySelector("#grid img");
      if (!img) return { present: false };
      return { present: true, blob: String(img.src).startsWith("blob:"), w: img.naturalWidth, h: img.naturalHeight, complete: img.complete };
    },
    (v) => v.present && v.complete && v.w > 0,
  );
  check(
    "preview renders: a blob-backed <img> appears in the grid and decodes (naturalWidth > 0)",
    imgState.present && imgState.blob && imgState.w > 0 && imgState.h > 0,
    `img=${JSON.stringify(imgState)}`,
  );

  // ── 4. A non-decodable file paints the loud placeholder, not a silent blank ───────────
  await page.evaluate(() => window.__nxvim.feed(":e /corrupt.png<CR>"));
  const placeholder = await until(
    page,
    () => {
      const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused);
      const hasMarker = !!(w && w.image);
      const text = document.querySelector("#grid").textContent || "";
      return { hasMarker, placeholder: text.includes("[image:") };
    },
    (v) => v.hasMarker && v.placeholder,
  );
  check(
    "fail loud: a non-decodable image paints a visible [image: …] placeholder",
    placeholder.hasMarker && placeholder.placeholder,
    `state=${JSON.stringify(placeholder)}`,
  );

  // ── 5. The reported scenario: `imagepreview` set in /init.lua, then open an image ────
  // Both halves had to be fixed for this to work: `bootWithConfig` decoding the config's raw
  // OPFS bytes, and the off-tick open honoring the option. Seed the config, reload so it's
  // sourced at boot (no addInitScript race), then open a fresh image.
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    let fh = await root.getFileHandle("init.lua", { create: true });
    let w = await fh.createWritable(); await w.write(new TextEncoder().encode("nx.o.imagepreview = true\n")); await w.close();
    const png = Uint8Array.from(atob("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="), (c) => c.charCodeAt(0));
    fh = await root.getFileHandle("viaconfig.png", { create: true });
    w = await fh.createWritable(); await w.write(png); await w.close();
  });
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  await page.evaluate(() => window.__nxvim.feed(":e /viaconfig.png<CR>"));
  const viaConfig = await until(
    page,
    () => { const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused); return w && w.image ? { path: w.image.path } : null; },
    (v) => v != null,
  );
  check(
    "config: `nx.o.imagepreview` in /init.lua enables previews after boot (the reported scenario)",
    viaConfig != null && /viaconfig\.png$/.test(viaConfig.path),
    `viaConfig=${JSON.stringify(viaConfig)}`,
  );

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — image previews (marker → out-of-band byte fetch → decoded <img>) driven in a real browser"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
