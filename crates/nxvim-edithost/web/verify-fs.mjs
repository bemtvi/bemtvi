// Playwright verifier for the real-local-filesystem picker family (`:eo` / `:wo` / bare
// `:w` on an unnamed buffer). These open *real* local files through the browser's File
// System Access API — `showOpenFilePicker` / `showSaveFilePicker` — instead of the OPFS
// sandbox. The native picker can't be driven by Playwright, so we stub it (via
// `addInitScript`, before the page's module evaluates) with fake in-memory handles, then
// drive the full round trips through `window.__nxvim` and assert the bytes truly move
// between the editor buffer and the (fake) handle's backing store.
//
// This exercises the SAB transport (the page is cross-origin isolated), including the run
// loop staying event-loop-live while a realfs read/write is in flight at the UI thread.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-fs.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8100;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
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

// The fake File System Access picker. Installed before any page script so the editor's
// `fsApiAvailable` check sees it. `__fakeFS` is the backing store the test inspects; a
// handle's getFile()/createWritable() read/write it. `__nextOpenName`/`__nextSaveName`
// choose which file the next picker "returns".
function installFakeFsApi() {
  const enc = new TextEncoder();
  window.__fakeFS = new Map(); // name -> { bytes: Uint8Array }
  window.__seedFile = (name, text) => window.__fakeFS.set(name, { bytes: enc.encode(text) });
  window.__fileText = (name) => {
    const e = window.__fakeFS.get(name);
    return e ? new TextDecoder().decode(e.bytes) : null;
  };
  window.__nextOpenName = null;
  window.__nextSaveName = null;
  const makeHandle = (name) => ({
    kind: "file",
    name,
    async queryPermission() { return "granted"; },
    async requestPermission() { return "granted"; },
    async getFile() {
      const e = window.__fakeFS.get(name) || { bytes: new Uint8Array(0) };
      return new File([e.bytes], name);
    },
    async createWritable() {
      const chunks = [];
      return {
        async write(chunk) {
          if (chunk && chunk.type === "write") chunk = chunk.data;
          chunks.push(chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk));
        },
        async truncate() {},
        async close() {
          let len = 0;
          for (const c of chunks) len += c.byteLength;
          const out = new Uint8Array(len);
          let off = 0;
          for (const c of chunks) { out.set(c, off); off += c.byteLength; }
          window.__fakeFS.set(name, { bytes: out });
        },
      };
    },
  });
  self.showOpenFilePicker = async () => {
    if (!window.__nextOpenName) throw new DOMException("cancelled", "AbortError");
    return [makeHandle(window.__nextOpenName)];
  };
  self.showSaveFilePicker = async () => {
    if (!window.__nextSaveName) throw new DOMException("cancelled", "AbortError");
    return makeHandle(window.__nextSaveName);
  };
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// Poll the page until `pred(value)` holds (or timeout), returning the last value.
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

  // Install the fake picker BEFORE the page's module runs (so `fsApiAvailable` is true).
  await page.addInitScript(installFakeFsApi);
  await page.goto(`http://localhost:${PORT}/web/index.html`);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // ── 1. Bare `:w` on the unnamed startup buffer pops the SAVE picker ──────────────────
  // Type a marker into the [No Name] buffer, then `:w<CR>` — with no file to write to, that
  // must pop the save picker (mirrors nxvim-gui), here returning "from-unnamed.txt".
  await page.evaluate(() => window.__nxvim.feed("ihello unnamed<Esc>"));
  await page.evaluate(() => { window.__nextSaveName = "from-unnamed.txt"; });
  await page.evaluate(() => window.__nxvim.feed(":w"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const savedUnnamed = await until(page, () => window.__fileText("from-unnamed.txt"), (v) => v != null);
  check(
    "save picker: bare :w on an unnamed buffer writes to the picked real file",
    savedUnnamed != null && savedUnnamed.includes("hello unnamed"),
    `from-unnamed.txt=${JSON.stringify(savedUnnamed)}`,
  );

  // The buffer is now bound to that file (nxvim's `:w <path>` renames it), so [+] clears.
  const namedNow = await until(
    page,
    () => (window.__nxvim.frame()?.windows?.find((w) => w.focused) || {}).file_name || "",
    (v) => String(v).includes("from-unnamed.txt"),
  );
  check("save picker: buffer is renamed to the picked file", String(namedNow).includes("from-unnamed.txt"), `name=${JSON.stringify(namedNow)}`);

  // ── 2. A subsequent bare `:w` writes straight back (no picker — buffer is bound) ──────
  await page.evaluate(() => window.__nxvim.feed("oSECOND-LINE<Esc>"));
  await page.evaluate(() => { window.__nextSaveName = null; }); // a picker now would throw/abort
  await page.evaluate(() => window.__nxvim.feed(":w"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const rewritten = await until(
    page,
    () => window.__fileText("from-unnamed.txt"),
    (v) => v != null && v.includes("SECOND-LINE"),
  );
  check(
    "write-back: bare :w on the bound buffer rewrites the same real file (no picker)",
    rewritten != null && rewritten.includes("hello unnamed") && rewritten.includes("SECOND-LINE"),
    `from-unnamed.txt=${JSON.stringify(rewritten)}`,
  );

  // ── 3. `:eo` opens the OPEN picker and reads the real file's bytes into a buffer ──────
  await page.evaluate(() => { window.__seedFile("open-me.txt", "alpha\nbeta\ngamma"); window.__nextOpenName = "open-me.txt"; });
  await page.evaluate(() => window.__nxvim.feed(":eo"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const opened = await until(page, () => window.__nxvim.lines(), (v) => v === "alpha\nbeta\ngamma");
  check("open picker: :eo reads the picked real file into the buffer", opened === "alpha\nbeta\ngamma", `lines=${JSON.stringify(opened)}`);

  // ── 4. Editing the opened file + bare `:w` writes back to its real handle ─────────────
  await page.evaluate(() => window.__nxvim.feed("GoDELTA<Esc>"));
  await page.evaluate(() => window.__nxvim.feed(":w"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const openedRewritten = await until(
    page,
    () => window.__fileText("open-me.txt"),
    (v) => v != null && v.includes("DELTA"),
  );
  check(
    "write-back: :w on the opened file persists the edit to its real handle",
    openedRewritten != null && openedRewritten.includes("alpha") && openedRewritten.includes("DELTA"),
    `open-me.txt=${JSON.stringify(openedRewritten)}`,
  );

  // ── 5. `:wo` (save-as) writes the current buffer to a newly picked file ───────────────
  await page.evaluate(() => { window.__nextSaveName = "copy-as.txt"; });
  await page.evaluate(() => window.__nxvim.feed(":wo"));
  await page.evaluate(() => window.__nxvim.pressEnter());
  const savedAs = await until(page, () => window.__fileText("copy-as.txt"), (v) => v != null);
  check(
    "save-as: :wo writes the buffer to a freshly picked real file",
    savedAs != null && savedAs.includes("alpha") && savedAs.includes("DELTA"),
    `copy-as.txt=${JSON.stringify(savedAs)}`,
  );

  // ── 6. An explicit-path `:e` is NOT intercepted — it still uses OPFS ──────────────────
  // Real-FS is opt-in via the picker (the bare/`…o` forms). An explicit `:e <path>` carries
  // an argument, so the picker interception leaves it alone and it takes the OPFS leg: a
  // brand-new path opens an empty buffer (NotFoundError → new file), proving the routing.
  await page.evaluate(() => window.__nxvim.feed(":e ghost-no-handle.txt"));
  await page.evaluate(() => window.__nxvim.pressEnter()); // not a dialog verb → feeds <CR>, runs it
  const ghost = await until(page, () => window.__nxvim.lines(), (v) => v === "");
  check(
    "routing: an explicit-path :e still uses OPFS (real-FS is opt-in via the picker)",
    ghost === "",
    `lines=${JSON.stringify(ghost)}`,
  );

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — real local filesystem picker (:eo/:wo/bare :w) driven in a real browser via the File System Access API"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
