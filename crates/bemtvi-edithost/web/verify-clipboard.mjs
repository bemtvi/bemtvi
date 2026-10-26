// Playwright verifier for the serverless `"+` / `"*` clipboard registers in the browser
// edit-host. The synchronous `Clipboard` seam can't await `navigator.clipboard`, so the wasm
// build bridges through the Sink: a `"+`/`"*` yank/delete is forwarded to the UI thread and
// written to `navigator.clipboard` (eh_take_clipboard_writes → clipboard_write); the UI reads
// the OS clipboard back into the mirror a `"+p` consumes (eh_clipboard_push). This drives both
// directions through a real (headless Chromium) browser against the actual OS clipboard.
//
// Runs over the SAB transport (cross-origin isolated), so it also covers the ring type-8
// clipboard-push frame and the run loop's drainClipboard convergence. The async Lua API
// (`btv.clipboard.read` / `write`, answered by the page) runs over both SAB and postMessage,
// plus a read the browser refuses.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-clipboard.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8108;
const ORIGIN = `http://localhost:${PORT}`;

// Prefer an explicit Chromium (PW_CHROMIUM), else the newest ms-playwright build for this
// platform (linux `chrome-linux*/chrome` or macOS `chrome-mac/Chromium.app/...`), else
// Playwright's bundled default.
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

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

// Poll `navigator.clipboard.readText()` until it equals `want` (the write is fire-and-forget,
// so it lands a tick after the feed resolves). Returns the last value seen.
async function clipboardEquals(page, want, tries = 50) {
  let last = "";
  for (let i = 0; i < tries; i++) {
    last = await page.evaluate(() => navigator.clipboard.readText());
    if (last === want) return last;
    await sleep(20);
  }
  return last;
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

async function waitReady(page) {
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
}

// Fire a real `paste` gesture (Cmd/Ctrl+V, Edit▸Paste, middle-click all surface as this):
// a ClipboardEvent on the grid carrying `text` in its clipboardData, exactly as the browser
// delivers an OS paste. No `clipboardRefresh()` priming — the gesture itself carries the data.
async function pasteGesture(page, text) {
  await page.evaluate((t) => {
    const dt = new DataTransfer();
    dt.setData("text/plain", t);
    // Dispatch on the editable input proxy (#kbd) — a real paste fires on whichever proxy is
    // focused (#kbd on Firefox, the EditContext-host #ime div on Chrome), and the handler is
    // bound to both, so dispatching on #kbd exercises the same onPaste path.
    document.getElementById("kbd").dispatchEvent(
      new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }),
    );
  }, text);
}

// Poll the joined buffer text until it contains `want` (the paste feed is fire-and-forget).
async function linesContain(page, want, tries = 50) {
  let last = "";
  for (let i = 0; i < tries; i++) {
    last = await page.evaluate(() => window.__bemtvi.lines());
    if (typeof last === "string" && last.includes(want)) return last;
    await sleep(20);
  }
  return last;
}

// Run `code` in the editor's Lua, returning its `return` value (rendered by rmpv).
const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

// `btv.clipboard` settles on a later pass — poll `_G.<key>` until it's no longer nil.
async function pollGlobal(page, key, tries = 100) {
  for (let i = 0; i < tries; i++) {
    const v = String(await luaResult(page, `return _G.${key}`));
    if (!v.includes("Nil")) return v;
    await sleep(30);
  }
  return "Nil";
}

// The `btv.clipboard` read / write round trips, on whichever transport `page` runs.
async function clipboardApiChecks(page, label) {
  await page.evaluate(() => navigator.clipboard.writeText(`OTHER-APP-${Math.random()}`));
  const external = await page.evaluate(() => navigator.clipboard.readText());
  await luaResult(page, `_G.clip_read = nil
    btv.clipboard.read():next(function(t) _G.clip_read = "ok:" .. t end,
      function(e) _G.clip_read = "err:" .. tostring(e) end)`);
  const read = await pollGlobal(page, "clip_read");
  check(
    `[${label}] btv.clipboard.read resolves with a copy made outside, no mirror refresh`,
    read.includes(`ok:${external}`),
    `result=${read} want=${external}`,
  );

  await luaResult(page, `_G.clip_write = nil
    btv.clipboard.write("LUA-WROTE"):next(function(v) _G.clip_write = "ok:" .. tostring(v) end,
      function(e) _G.clip_write = "err:" .. tostring(e) end)`);
  const write = await pollGlobal(page, "clip_write");
  const onClip = await clipboardEquals(page, "LUA-WROTE");
  check(
    `[${label}] btv.clipboard.write settles and puts the text on navigator.clipboard`,
    write.includes("ok:nil") && onClip === "LUA-WROTE",
    `result=${write} clipboard=${JSON.stringify(onClip)}`,
  );

  // A linewise write with no trailing newline: the mirror keeps the flag, so `"+p` puts the
  // text on a line of its own.
  await luaResult(page, `_G.clip_lw = nil
    btv.clipboard.write("LW-LINE", { linewise = true }):next(function() _G.clip_lw = "ok" end,
      function(e) _G.clip_lw = "err:" .. tostring(e) end)`);
  const lw = await pollGlobal(page, "clip_lw");
  await page.evaluate(() => window.__bemtvi.feed("ggdGianchor<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('"+p'));
  const lwLines = await page.evaluate(() => window.__bemtvi.lines());
  check(
    `[${label}] a linewise btv.clipboard.write pastes with \"+p on its own line`,
    lw.includes("ok") && lwLines === "anchor\nLW-LINE",
    `result=${lw} lines=${JSON.stringify(lwLines)}`,
  );
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }

  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const context = await browser.newContext();
  // The `"+` registers need both read (paste) and write (yank) permission for this origin.
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: ORIGIN });
  const page = await context.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`${ORIGIN}/web/index.html`);
  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);
  await waitReady(page);

  // ── Yank to `"+` writes the OS clipboard (linewise) ──────────────────────────────────────
  await page.evaluate(() => window.__bemtvi.feed("ggdGiCOPY-FROM-BEMTVI<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('"+yy'));
  const wrote = await clipboardEquals(page, "COPY-FROM-BEMTVI\n");
  check(
    'yank: `"+yy` wrote the line to navigator.clipboard (linewise → trailing \\n)',
    wrote === "COPY-FROM-BEMTVI\n",
    `clipboard=${JSON.stringify(wrote)}`,
  );

  // ── Charwise yank to `"+` writes without a trailing newline ──────────────────────────────
  await page.evaluate(() => window.__bemtvi.feed("ggdGiword charwise<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('0"+yw')); // yank "word " (charwise, no \n)
  const wroteChar = await clipboardEquals(page, "word ");
  check(
    'yank: charwise `"+yw` wrote the word with no trailing newline',
    wroteChar === "word ",
    `clipboard=${JSON.stringify(wroteChar)}`,
  );

  // ── Paste from `"+` reads an external copy ───────────────────────────────────────────────
  // Put text on the OS clipboard "from another app", refresh the mirror (the focus/click
  // triggers do this in normal use), then `"+p` it into a fresh buffer.
  await page.evaluate(() => navigator.clipboard.writeText("PASTED-FROM-OUTSIDE"));
  await page.evaluate(() => window.__bemtvi.clipboardRefresh());
  await page.evaluate(() => window.__bemtvi.feed("ggdG"));
  await page.evaluate(() => window.__bemtvi.feed('"+p'));
  const pasted = await page.evaluate(() => window.__bemtvi.lines());
  check(
    'paste: `"+p` inserted the external clipboard text',
    pasted.includes("PASTED-FROM-OUTSIDE"),
    `lines=${JSON.stringify(pasted)}`,
  );

  // ── Linewise external copy pastes as a whole line ────────────────────────────────────────
  await page.evaluate(() => navigator.clipboard.writeText("EXTERNAL-LINE\n"));
  await page.evaluate(() => window.__bemtvi.clipboardRefresh());
  await page.evaluate(() => window.__bemtvi.feed("ggdGianchor<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed('"+p')); // linewise paste lands on a new line below
  const pastedLine = await page.evaluate(() => window.__bemtvi.lines());
  check(
    'paste: linewise external copy `"+p` lands on its own line below the cursor',
    pastedLine === "anchor\nEXTERNAL-LINE",
    `lines=${JSON.stringify(pastedLine)}`,
  );

  // ── Paste gesture in Normal mode → `"+p`, no refresh dance ───────────────────────────────
  // The whole point of the friendly path: a plain Cmd/Ctrl+V (here, the paste event) drops the
  // clipboard text straight in, with NO clipboardRefresh()/click-out-and-back to wake it.
  await page.evaluate(() => window.__bemtvi.feed("ggdG<Esc>")); // empty buffer, Normal mode
  await pasteGesture(page, "GESTURE-NORMAL");
  const gNormal = await linesContain(page, "GESTURE-NORMAL");
  check(
    "paste gesture: Cmd/Ctrl+V in Normal mode drops the clipboard text in (no refresh dance)",
    gNormal.includes("GESTURE-NORMAL"),
    `lines=${JSON.stringify(gNormal)}`,
  );

  // ── Paste gesture in Insert mode → typed in literally at the cursor ───────────────────────
  await page.evaluate(() => window.__bemtvi.feed("ggdGi")); // empty buffer, Insert mode
  await pasteGesture(page, "GESTURE-INSERT");
  const gInsert = await linesContain(page, "GESTURE-INSERT");
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  check(
    "paste gesture: Cmd/Ctrl+V in Insert mode types the clipboard text at the cursor",
    gInsert.includes("GESTURE-INSERT"),
    `lines=${JSON.stringify(gInsert)}`,
  );

  // ── Multi-line paste gesture in Insert mode folds CRLF/LF to real line breaks ─────────────
  await page.evaluate(() => window.__bemtvi.feed("ggdGi"));
  await pasteGesture(page, "line-a\r\nline-b\nline-c");
  const gMulti = await linesContain(page, "line-c");
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  check(
    "paste gesture: multi-line text splits into lines (CRLF folds to one break)",
    gMulti === "line-a\nline-b\nline-c",
    `lines=${JSON.stringify(gMulti)}`,
  );

  // ── Indented multi-line paste keeps its own indentation ──────────────────────────────────
  // The browser leg of the bracketed-paste guard: `encodePaste` wraps the payload in
  // `<PasteStart>`/`<PasteEnd>`, so the pasted `<CR>`s take no auto-indent and each line
  // lands at the column it carried. Without the brackets, `smartindent` stacks an indent on
  // top of the payload's own and every line drifts further right.
  await page.evaluate(() => window.__bemtvi.feed(":set expandtab smartindent<CR>ggdGi"));
  await pasteGesture(page, "if x {\n    body;\n}");
  const gIndent = await linesContain(page, "body;");
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  check(
    "paste gesture: an indented payload keeps its own indentation (no auto-indent stacking)",
    gIndent === "if x {\n    body;\n}",
    `lines=${JSON.stringify(gIndent)}`,
  );

  // ── The built-in Ctrl+C / Ctrl+V chords, as REAL key presses ─────────────────────────────
  // The prelude ships `<C-c>` (copy the selection) and `<C-v>` (paste at the cursor) as
  // default keymaps. Driven here through `page.keyboard`, not the `feed` hook, because the
  // browser is the one client where the chord has to get past the page at all: the keydown
  // handler encodes and `preventDefault()`s it, so the browser's own copy/paste never runs
  // and the editor's keymap is what fires.
  await page.evaluate(() => window.__bemtvi.feed("ggdGiCHORD-COPY<Esc>"));
  await page.evaluate(() => navigator.clipboard.writeText("stale")); // must be overwritten
  await page.evaluate(() => window.__bemtvi.feed("0v$"));            // select the line
  await page.keyboard.press("Control+c");
  const chordCopied = await clipboardEquals(page, "CHORD-COPY");
  check(
    "chord: Ctrl+C copies the visual selection to the OS clipboard",
    chordCopied === "CHORD-COPY",
    `clipboard=${JSON.stringify(chordCopied)}`,
  );

  await page.evaluate(() => window.__bemtvi.feed("<Esc>ggdGianchor<Esc>0"));
  await page.keyboard.press("Control+v");
  const chordPasted = await linesContain(page, "CHORD-COPY");
  check(
    "chord: Ctrl+V pastes the clipboard at the cursor (P semantics, before the cursor)",
    chordPasted === "CHORD-COPYanchor",
    `lines=${JSON.stringify(chordPasted)}`,
  );

  // Insert mode inserts at the caret and stays in insert, so typing continues after it.
  await page.evaluate(() => window.__bemtvi.feed("ggdGi["));
  await page.keyboard.press("Control+v");
  await page.evaluate(() => window.__bemtvi.feed("]<Esc>"));
  const chordInsert = await linesContain(page, "CHORD-COPY");
  check(
    "chord: Ctrl+V in insert mode types the clipboard in at the caret",
    chordInsert === "[CHORD-COPY]",
    `lines=${JSON.stringify(chordInsert)}`,
  );

  // ── btv.clipboard: the async Lua API, round-tripped through the page ──────────────────────
  // The mirror is never primed here: `btv.clipboard.read` has to reach navigator.clipboard
  // itself, so a copy made "in another app" reads back with no refresh / focus dance.
  await clipboardApiChecks(page, "SAB");

  // ── btv.clipboard over the postMessage transport (5c) ────────────────────────────────────
  // Strip the isolation headers so the page falls back to postMessage: the op's answer then
  // arrives as a `clipboard_op_result` message rather than a ring frame.
  const plain = await browser.newContext();
  await plain.grantPermissions(["clipboard-read", "clipboard-write"], { origin: ORIGIN });
  await plain.route("**/*", async (route) => {
    const resp = await route.fetch();
    const headers = { ...resp.headers() };
    delete headers["cross-origin-opener-policy"];
    delete headers["cross-origin-embedder-policy"];
    await route.fulfill({ response: resp, headers });
  });
  const page5c = await plain.newPage();
  page5c.on("pageerror", (e) => console.log("  [pageerror 5c]", e.message));
  await page5c.goto(`${ORIGIN}/web/index.html`);
  const isolated5c = await page5c.evaluate(() => self.crossOriginIsolated);
  check("5c page is not cross-origin isolated (postMessage transport)", isolated5c === false, `isolated=${isolated5c}`);
  await waitReady(page5c);
  await clipboardApiChecks(page5c, "5c");
  await plain.close();

  // ── btv.clipboard.read rejects when the browser refuses ──────────────────────────────────
  const denied = await browser.newContext();
  await denied.grantPermissions(["clipboard-write"], { origin: ORIGIN }); // no clipboard-read
  const pageDenied = await denied.newPage();
  await pageDenied.goto(`${ORIGIN}/web/index.html`);
  await waitReady(pageDenied);
  await luaResult(pageDenied, `_G.clip_denied = nil
    btv.clipboard.read():next(function(t) _G.clip_denied = "resolved:" .. t end,
      function(e) _G.clip_denied = "rejected:" .. tostring(e) end)`);
  const deniedRes = await pollGlobal(pageDenied, "clip_denied");
  check(
    "btv.clipboard.read rejects, naming the op, when the page may not read the clipboard",
    deniedRes.includes("rejected:btv.clipboard.read:"),
    `result=${deniedRes}`,
  );
  await denied.close();

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless `\"+`/`\"*` clipboard works both directions (yank→navigator.clipboard, external copy→`\"+p`)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
