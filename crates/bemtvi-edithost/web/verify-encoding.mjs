// Playwright verifier for the wasm READ path's encoding seam (the follow-up to
// docs/plans/2026-06-14-encoding-and-invalid-utf8.md). Seeds OPFS with raw, NON-UTF-8
// bytes through the browser's own File System API (a path the editor never touches),
// then `:e`s them in the real wasm edit-host and asserts the buffer decodes through the
// shared `decode_to_rope` seam (latin1 detection, invalid-UTF-8 resilience) and that
// `:w` reproduces the original bytes EXACTLY — proving the bytes now cross the FFI raw
// (no JS-side `TextDecoder` mangling them before Rust sees them).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-encoding.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8103;

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

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // Seed OPFS directly (raw bytes, the editor never touches this path) with a `dir/name`
  // file, returning nothing. Uses the main-thread writable-stream API.
  async function opfsSeed(dirName, fileName, bytes) {
    await page.evaluate(async ({ dirName, fileName, bytes }) => {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(dirName, { create: true });
      const fh = await dir.getFileHandle(fileName, { create: true });
      const w = await fh.createWritable();
      await w.write(new Uint8Array(bytes));
      await w.close();
    }, { dirName, fileName, bytes });
  }
  // Read raw OPFS bytes back as a plain number[] (a path the editor never touches).
  async function opfsBytes(dirName, fileName) {
    return await page.evaluate(async ({ dirName, fileName }) => {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle(dirName);
      const fh = await dir.getFileHandle(fileName);
      const buf = await (await fh.getFile()).arrayBuffer();
      return Array.from(new Uint8Array(buf));
    }, { dirName, fileName });
  }
  const lua = async (src) =>
    String((await page.evaluate((s) => window.__bemtvi.execLua(s).then((r) => r.result), src)) ?? "")
      .replace(/^ok:/, "");
  const eq = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

  // 1. A real latin1 file: byte 0xe9 is é. Seed it, `:e` it.
  const LATIN1 = [0x43, 0x61, 0x66, 0xe9, 0x0a]; // "Caf\xe9\n"
  await opfsSeed("enc", "latin1.txt", LATIN1);
  await page.evaluate(() => window.__bemtvi.feed(":e /enc/latin1.txt<CR>"));
  await sleep(150); // the off-tick OPFS read lands a tick later
  const latin1Line = await page.evaluate(() => window.__bemtvi.lines());
  check("wasm read: latin1 0xe9 decodes to é (not mangled by TextDecoder)",
    latin1Line === "Café", `got ${JSON.stringify(latin1Line)}`);
  const fenc = await lua("return vim.bo.fileencoding");
  check("wasm read: the buffer carries the detected fileencoding=latin1",
    fenc.includes("latin1"), `fileencoding=${JSON.stringify(fenc)}`);

  // 2. Invalid-UTF-8 file: opens (no refusal) and `:w` round-trips byte-identically.
  const INVALID = [0x68, 0x69, 0x20, 0xff, 0xfe, 0x0a]; // "hi \xff\xfe\n"
  await opfsSeed("enc", "invalid.txt", INVALID);
  await page.evaluate(() => window.__bemtvi.feed(":e /enc/invalid.txt<CR>"));
  await sleep(150);
  const invalidLine = await page.evaluate(() => window.__bemtvi.lines());
  check("wasm read: invalid-UTF-8 file opens non-empty (latin1 fallback)",
    invalidLine === "hi ÿþ", `got ${JSON.stringify(invalidLine)}`);

  // Save it back (no edits) and read the raw OPFS bytes — must be byte-identical.
  await page.evaluate(() => window.__bemtvi.feed(":w<CR>"));
  await sleep(150);
  const after = await opfsBytes("enc", "invalid.txt");
  check("wasm write: invalid-UTF-8 round-trips byte-identical through the seam",
    eq(after, INVALID), `orig=${JSON.stringify(INVALID)} after=${JSON.stringify(after)}`);

  // 3. An embedded C0 control (0x01) renders as the `^A` caret token AND gets the
  //    SpecialKey colour. The native build overlays the `SpecialKey` highlight group
  //    via server-computed spans; the wasm build paints JS-side, so the server hands
  //    it the token's display columns in `special_key` and renderLine() colours them.
  const CONTROL = [0x61, 0x01, 0x62, 0x0a]; // "a\x01b\n"
  await opfsSeed("enc", "control.txt", CONTROL);
  await page.evaluate(() => window.__bemtvi.feed(":e /enc/control.txt<CR>"));
  await sleep(150);
  // The display row substitutes the control char (`a^Ab`); the buffer keeps the raw scalar.
  const dispLine = await page.evaluate(() => {
    const fw = (window.__bemtvi.frame()?.windows || []).find((w) => w.focused);
    return fw ? fw.lines[0] : null;
  });
  check("wasm display: 0x01 substitutes to the ^A caret token",
    dispLine === "a^Ab", `got ${JSON.stringify(dispLine)}`);
  // The server marks the token's display columns (^A starts at col 1, width 2 → [1,3]).
  const sk = await page.evaluate(() => {
    const fw = (window.__bemtvi.frame()?.windows || []).find((w) => w.focused);
    return fw ? fw.special_key?.[0] : null;
  });
  check("wasm redraw: special_key carries the token's display columns",
    Array.isArray(sk) && sk.length === 1 && sk[0][0] === 1 && sk[0][1] === 3,
    `special_key[0]=${JSON.stringify(sk)}`);
  // The rendered DOM paints the token in the SpecialKey colour (#d787ff = rgb(215,135,255)).
  const skColored = await page.evaluate(() => {
    const want = "rgb(215, 135, 255)";
    return [...document.querySelectorAll("#grid span")].some(
      (s) => s.textContent.includes("^A") && getComputedStyle(s).color === want);
  });
  check("wasm paint: the ^A token is coloured as SpecialKey", skColored,
    "no #grid span with text ^A and color rgb(215, 135, 255)");

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — wasm read path decodes through the encoding seam (latin1 + invalid-UTF-8 round-trip)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
