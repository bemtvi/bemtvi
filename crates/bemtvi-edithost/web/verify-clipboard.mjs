// Playwright verifier for the serverless `"+` / `"*` clipboard registers in the browser
// edit-host. The synchronous `Clipboard` seam can't await `navigator.clipboard`, so the wasm
// build bridges through the Sink: a `"+`/`"*` yank/delete is forwarded to the UI thread and
// written to `navigator.clipboard` (eh_take_clipboard_writes → clipboard_write); the UI reads
// the OS clipboard back into the mirror a `"+p` consumes (eh_clipboard_push). This drives both
// directions through a real (headless Chromium) browser against the actual OS clipboard.
//
// Runs over the SAB transport (cross-origin isolated), so it also covers the ring type-8
// clipboard-push frame and the run loop's drainClipboardWrites convergence.
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

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless `\"+`/`\"*` clipboard works both directions (yank→navigator.clipboard, external copy→`\"+p`)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
