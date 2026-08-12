// Playwright verifier for the browser edit-host's LSP leg (Phase 6e) against a REAL
// `bemtvi --daemon --listen` over WebTransport. The browser has no process host, so a
// language server can't run locally — `vim.lsp.start` crosses the wire: the in-Worker
// `SyncLspClient`'s `lsp_spawn`/`lsp_stdin`/`lsp_kill` go to the daemon, which runs the
// real server (`serve_one_lsp`, the same wire the native `RemoteLspTransport` uses), and
// the server's `lsp_stdout`/`lsp_stderr`/`lsp_exited` return as pushes the Worker feeds
// back into the client. The browser twin of the native `lsp_config.rs` / `lsp_float.rs`
// black-box tests.
//
// The scripted mock server (`bemtvi --__lsp-mock <json>`, `bemtvi_lsp::mock`) is what the
// daemon spawns — a real child on the daemon's machine, configured verbatim via the
// config's `cmd` (the browser has no `$BEMTVI_LSP_CMD` env hook; `lsp_spawn` falls through
// to the config cmd). Faithfulness (not a no-op):
//   1. diagnostics round-trip: each mock pushes `textDocument/publishDiagnostics` on
//      `didOpen` — a SERVER→client push that only a real `didOpen` over the wire triggers
//      — and the scripted messages land in `btv.diagnostic.get()`'s queryable editor state.
//   2. hover round-trip: `btv.lsp.hover()` fires a request whose reply opens the hover
//      float window with the scripted markup (a round-trip the opposite direction).
//
// TWO servers are enabled for the same filetype (Phase 6 of
// docs/plans/2026-07-25-multi-server-lsp-attach.md), so the multi-server layer is
// verified over the wire and not merely locally: two children on the daemon, two
// tunnels, two documents. Each check is a merge or a routing decision that a
// one-server session cannot satisfy —
//   * both servers' pushed diagnostics merge (each holds its own document);
//   * the hover routes to the one advertising `hoverProvider` (`mock2` withholds it);
//   * completion fans out and merges both servers' candidates (Phase 3c).
//   * a server→client `workspace/applyEdit` applies and is ANSWERED down the tunnel, and
//     its `create`/`rename`/`delete` resource operations make, move and remove real
//     files on the daemon's disk —
//     the direction every other check runs backwards, and the only one where the browser
//     must frame a response of its own (`SyncLspClient`) and reach a filesystem it does
//     not have (`docs/plans/2026-07-25-lsp-apply-edit.md`).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack), `cargo build -p bemtvi`
// (target/debug/bemtvi — the daemon AND the mock server), and a Chromium for Playwright.
// Run:  node verify-lsp.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { existsSync, globSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8143;
const BEMTVI = process.env.BEMTVI_BIN || `${here}../../../target/debug/bemtvi`;

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

// Poll the page until `pred(value)` holds (or timeout), returning the last value.
async function until(page, fn, pred, ms = 10000) {
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(fn);
    if (pred(v)) return v;
    if (Date.now() - start > ms) return v;
    await sleep(50);
  }
}
const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

// ── The daemon's working tree (real disk; the .rs file + the mock script live here) ────────
const root = mkdtempSync(join(tmpdir(), "bemtvi-lsp-"));
const rsFile = join(root, "a.rs");
writeFileSync(rsFile, "let foo = bar()\n");
// Where the `rename` resource operation moves it, and a file the `delete` operation
// removes — both on the daemon's real disk (checks 6 and 7 stat them from here).
const movedFile = join(root, "moved.rs");
const doomedFile = join(root, "doomed.rs");
writeFileSync(doomedFile, "let doomed = 1\n");
// Where the `create` resource operation puts a file that does not exist yet (check 6).
const extractedFile = join(root, "extracted.rs");
// The scripted mock: diagnostics pushed on didOpen + a hover reply. Cargo.toml-style
// absolute paths so the daemon (same machine) can spawn it and read the script.
const mockJson = join(root, "mock.json");
// Where the mock appends what it received, including the editor's answer to the
// `workspace/applyEdit` it pushes (check 5 reads it back off disk — the daemon runs on
// this machine, so the file is right here).
const mockRecord = join(root, "rec.jsonl");
writeFileSync(
  mockJson,
  JSON.stringify({
    record: mockRecord,
    // A refactor delivered as a bare `command`: the `executeCommand` reply carries
    // nothing and the edit comes back as a server→client `workspace/applyEdit` — the
    // one inbound REQUEST the editor answers, and the one place the browser's
    // `SyncLspClient` has to frame a response of its own rather than distil a reply.
    // Three actions, each with its own kind so a `context.only` request picks exactly
    // one (the mock filters by kind the way a compliant server does) and `apply` makes
    // it a one-shot — no chooser to drive from a verifier.
    code_action: [
      { title: "Rewrite via applyEdit", kind: "refactor.rewrite", command: { title: "run", command: "mock.rewrite" } },
      { title: "Move file", kind: "refactor.move", command: { title: "run", command: "mock.move" } },
      { title: "Remove file", kind: "refactor.remove", command: { title: "run", command: "mock.remove" } },
      { title: "Extract to new file", kind: "refactor.extract", command: { title: "run", command: "mock.extract" } },
    ],
    // Three server-initiated edits, one per `executeCommand`: a text edit, then the
    // two operations that move real bytes on the DAEMON's filesystem — which is the
    // whole reason they run off-tick (nothing on the browser's editor tick can rename
    // a file that lives across a WebTransport link).
    apply_edit_by_command: {
      "mock.rewrite": {
        changes: {
          [`file://${rsFile}`]: [
            {
              range: { start: { line: 0, character: 4 }, end: { line: 0, character: 7 } },
              newText: "baz",
            },
          ],
        },
      },
      "mock.move": {
        documentChanges: [{ kind: "rename", oldUri: `file://${rsFile}`, newUri: `file://${movedFile}` }],
      },
      "mock.remove": { documentChanges: [{ kind: "delete", uri: `file://${doomedFile}` }] },
      // create + the edits that fill it — gopls's extract-to-new-file shape. The file
      // has to end up on the daemon's disk with the extracted text in it.
      "mock.extract": {
        documentChanges: [
          { kind: "create", uri: `file://${extractedFile}` },
          {
            textDocument: { uri: `file://${extractedFile}`, version: 0 },
            edits: [
              {
                range: { start: { line: 0, character: 0 }, end: { line: 0, character: 0 } },
                newText: "let extracted = 1\n",
              },
            ],
          },
        ],
      },
    },
    diagnostics: [
      {
        range: { start: { line: 0, character: 4 }, end: { line: 0, character: 7 } },
        severity: 1,
        message: "scripted-diag-from-daemon",
      },
    ],
    hover: { contents: { kind: "markdown", value: "`foo`: a scripted hover symbol" } },
    completion: [{ label: "from_mock_one", insertText: "from_mock_one" }],
  }),
);
// A SECOND scripted server for the same filetype (the `pyright` + `ruff` shape), so the
// multi-server layer is exercised over the wire and not just locally: two children on
// the daemon, two tunnels, two documents. It withholds `hoverProvider` on purpose —
// with both attached, a hover must still reach the one that advertises it.
// (`mock` sorts before `mock2` in ServerKey order, so a first-server pick would answer
// from `mock` by luck; the capability gate is what the completion check below pins down,
// since only `mock2` offers `from_mock_two`.)
const mock2Json = join(root, "mock2.json");
writeFileSync(
  mock2Json,
  JSON.stringify({
    capabilities: { hoverProvider: false },
    diagnostics: [
      {
        range: { start: { line: 0, character: 10 }, end: { line: 0, character: 13 } },
        severity: 2,
        message: "scripted-diag-from-second-server",
      },
    ],
    completion: [{ label: "from_mock_two", insertText: "from_mock_two" }],
  }),
);

// ── Spawn the real daemon; parse its connect URI from stdout ──────────────────────────────
const daemon = spawn(BEMTVI, ["--daemon", "--listen", "127.0.0.1:0"], { stdio: ["ignore", "pipe", "pipe"] });
let uri = null;
let daemonOut = "";
daemon.stdout.on("data", (d) => {
  daemonOut += d.toString();
  const m = daemonOut.match(/bemtvi:\/\/[^'\s]+/);
  if (m) uri = m[0];
});
daemon.stderr.on("data", (d) => process.stderr.write(`  [daemon] ${d}`));

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { daemon.kill(); } catch {} try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 100 && !uri; i++) await sleep(50);
  if (!uri) throw new Error(`daemon never printed a connect URI; stdout=${JSON.stringify(daemonOut)}`);
  console.log("daemon listening:", uri.replace(/\/[0-9a-f]{64}\?/, "/<token>?"));

  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  const pageUrl = `http://localhost:${PORT}/web/?daemon=${encodeURIComponent(uri)}`;
  await page.goto(pageUrl);

  const isolated = await page.evaluate(() => self.crossOriginIsolated);
  check("page is cross-origin isolated (SAB transport active)", isolated === true, `isolated=${isolated}`);

  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted + dialed the daemon (window.__bemtvi.ready resolved)", true);

  // ── Open the .rs buffer over the wire (filetype rust, `foo` under the cursor) ───────────
  await page.evaluate((f) => window.__bemtvi.feed(":e " + f), rsFile);
  await page.evaluate(() => window.__bemtvi.feed("<CR>"));
  const opened = await until(page, () => window.__bemtvi.lines(), (v) => /let foo = bar\(\)/.test(String(v)));
  check("setup: :e <file>.rs reads the daemon's buffer over the wire", /let foo = bar\(\)/.test(String(opened)), `lines=${JSON.stringify(opened)}`);
  // Cursor on `foo` so a hover request has a symbol (and the reply's cursor-staleness gate passes).
  await page.evaluate(() => window.__bemtvi.feed("0fw"));

  // ── Declare + enable the mock server. `enable` processes the already-open buffer's
  //    FileType (rust), so the SyncLspClient spawns the server on the daemon over the wire. ──
  await luaResult(
    page,
    `btv.lsp.config("mock", { cmd = { ${JSON.stringify(BEMTVI)}, "--__lsp-mock", ${JSON.stringify(mockJson)} }, filetypes = { "rust" } })
     btv.lsp.config("mock2", { cmd = { ${JSON.stringify(BEMTVI)}, "--__lsp-mock", ${JSON.stringify(mock2Json)} }, filetypes = { "rust" } })
     btv.lsp.enable({ "mock", "mock2" })
     return 1`,
  );

  // ── 1. BOTH servers started on the daemon (they appear in btv.lsp.clients) ───────────────
  //    Two children, two stdio tunnels, two documents — the multi-server layer over the
  //    wire. Before that layer existed this was structurally capped at one.
  // `execLua`'s `.result` arrives as a debug-formatted wrapper (`ok:String(Utf8String
  // { s: Ok("2") })`), so read the count out of it rather than coercing the whole string.
  const clientCount = (v) => {
    const m = /Ok\("(\d+)"\)/.exec(String(v)) || /^"?(\d+)"?$/.exec(String(v));
    return m ? Number(m[1]) : NaN;
  };
  const clients = await until(
    page,
    () => window.__bemtvi.execLua("return tostring(#btv.lsp.clients())").then((r) => r.result),
    (v) => {
      const m = /Ok\("(\d+)"\)/.exec(String(v)) || /^"?(\d+)"?$/.exec(String(v));
      return m ? Number(m[1]) >= 2 : false;
    },
  );
  check("lsp: BOTH mock servers started on the daemon (btv.lsp.clients() lists 2)",
    clientCount(clients) >= 2, `clients=${JSON.stringify(clients)}`);

  // ── 2. diagnostics round-trip: didOpen → server push → btv.diagnostic.get() editor state ──
  const diags = await until(
    page,
    () =>
      window.__bemtvi
        .execLua(
          `local ds = btv.diagnostic.get(0) or {}
           local msgs = {}
           for _, d in ipairs(ds) do msgs[#msgs+1] = d.message end
           return table.concat(msgs, "|")`,
        )
        .then((r) => r.result),
    (v) => /scripted-diag-from-daemon/.test(String(v)),
  );
  check("lsp: server-pushed diagnostics (didOpen → publishDiagnostics) land in btv.diagnostic.get() over the wire",
    /scripted-diag-from-daemon/.test(String(diags)), `diags=${JSON.stringify(diags)}`);

  // ── 2b. BOTH servers' pushes merge. `publishDiagnostics` is the sharpest probe there
  //    is: a push only happens if that server actually received `didOpen` over its own
  //    tunnel, and the two sets are stored per server and merged at projection — a
  //    shared slot would have each erase the other. ─────────────────────────────────────
  const bothDiags = await until(
    page,
    () =>
      window.__bemtvi
        .execLua(
          `local ds = btv.diagnostic.get(0) or {}
           local msgs = {}
           for _, d in ipairs(ds) do msgs[#msgs+1] = d.message end
           table.sort(msgs)
           return table.concat(msgs, "|")`,
        )
        .then((r) => r.result),
    (v) => /scripted-diag-from-second-server/.test(String(v)) && /scripted-diag-from-daemon/.test(String(v)),
  );
  check("lsp: BOTH servers' diagnostics merge over the wire (each holds its own document)",
    /scripted-diag-from-second-server/.test(String(bothDiags)) && /scripted-diag-from-daemon/.test(String(bothDiags)),
    `diags=${JSON.stringify(bothDiags)}`);

  // ── 3. hover round-trip: btv.lsp.hover() request → reply opens the content float ─────────
  //    Hover is a real float WINDOW (`windows[]` with `floating == true`, so it can
  //    scroll), not the content-float `float` surface — the same place the native
  //    `lsp_config.rs` helpers read it from. (This check used to read `frame().float`
  //    and had gone stale with that move: it failed against a single server too.)
  const hoverText = await until(
    page,
    () => {
      window.__bemtvi.execLua("btv.lsp.hover()");
      const wins = (window.__bemtvi.frame() || {}).windows || [];
      const win = wins.find((w) => w && w.floating === true);
      if (!win || !Array.isArray(win.lines)) return "";
      // A float window's `lines` are plain strings.
      return win.lines.map((row) => (typeof row === "string" ? row : "")).join("\n");
    },
    (v) => /scripted hover symbol/.test(String(v)),
  );
  check("lsp: btv.lsp.hover() reply opens the hover float with the server's markup (request/reply over the wire)",
    /scripted hover symbol/.test(String(hoverText)), `float=${JSON.stringify(hoverText)}`);
  // The hover reached `mock` even though BOTH are attached, because `mock2` withholds
  // `hoverProvider` — capability routing surviving the tunnel. `mock2` has no hover to
  // give, so a mis-routed request would render nothing at all.
  check("lsp: the hover routed by capability over the wire (mock2 withholds hoverProvider)",
    /scripted hover symbol/.test(String(hoverText)), `float=${JSON.stringify(hoverText)}`);

  // ── 4. completion fans out to BOTH servers over the wire (Phase 3c) ────────────────────
  //    Each server offers one candidate the other does not, so a popup carrying both can
  //    only have come from two round-trips merged into one menu.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await page.evaluate(() => window.__bemtvi.execLua("btv.complete.setup { sources = { { 'lsp' } }, min_chars = 1 }"));
  await page.evaluate(() => window.__bemtvi.feed("ofrom_"));
  const items = await until(
    page,
    () => {
      window.__bemtvi.execLua("btv.complete.trigger()");
      const m = (window.__bemtvi.frame() || {}).menu || null;
      return m ? m.items || [] : [];
    },
    (v) =>
      Array.isArray(v) &&
      v.some((it) => /from_mock_one/.test(String(it))) &&
      v.some((it) => /from_mock_two/.test(String(it))),
    15000,
  );
  check("lsp: completion merges BOTH servers' candidates over the wire (3c fan-out)",
    Array.isArray(items) &&
      items.some((it) => /from_mock_one/.test(String(it))) &&
      items.some((it) => /from_mock_two/.test(String(it))),
    `items=${JSON.stringify(items)}`);

  // ── 5. server→client `workspace/applyEdit` over the wire (the apply half) ─────────────
  //    Everything above is the editor asking and the server answering. This is the
  //    reverse: the server asks, the editor applies, and — uniquely — has to send a
  //    RESPONSE back down the tunnel or the server blocks forever. On this leg that
  //    response is framed by the browser's `SyncLspClient`, a completely different
  //    implementation from the native router, so it has to be driven here.
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await page.evaluate(() => window.__bemtvi.feed("gg"));
  const rewritten = await until(
    page,
    () => {
      window.__bemtvi.execLua(
        'btv.lsp.code_action({ context = { only = { "refactor.rewrite" } }, apply = true })',
      );
      return window.__bemtvi.lines();
    },
    (v) => /let baz = bar\(\)/.test(String(v)),
    15000,
  );
  check("lsp: a server-initiated workspace/applyEdit applies to the buffer over the wire",
    /let baz = bar\(\)/.test(String(rewritten)), `lines=${JSON.stringify(rewritten)}`);
  let answered = "";
  for (let i = 0; i < 100; i++) {
    answered = readFileSync(mockRecord, "utf8");
    if (/_apply_edit_response/.test(answered)) break;
    await sleep(50);
  }
  const answer = answered.split("\n").find((l) => l.includes("_apply_edit_response")) || "";
  check("lsp: and the editor's `applied: true` response travels back down the tunnel",
    /"applied":true/.test(answer), `response=${JSON.stringify(answer)}`);

  // ── 6. the `create` resource operation makes a NEW file on the DAEMON's disk ──────────
  //    The file itself has to appear on the far side — a browser session has no local
  //    filesystem to fall back on, so it can only have crossed the wire — and it appears
  //    EMPTY: a `create` creates the file, and the extracted text the edits put in its
  //    buffer stays there, modified and unsaved, for you to `:w` (neovim's model).
  await until(
    page,
    () => {
      window.__bemtvi.execLua(
        'btv.lsp.code_action({ context = { only = { "refactor.extract" } }, apply = true })',
      );
      return window.__bemtvi.execLua("return 1").then((r) => r.result);
    },
    () => existsSync(extractedFile),
    15000,
  );
  check("lsp: a workspace edit's `create` makes the new file on the daemon",
    existsSync(extractedFile), "<missing>");
  check("lsp: …empty — the content stays in the buffer, unsaved",
    existsSync(extractedFile) && readFileSync(extractedFile, "utf8") === "",
    existsSync(extractedFile) ? JSON.stringify(readFileSync(extractedFile, "utf8")) : "<missing>");
  // …while the content really is in that buffer, modified and unsaved — the other half of
  // the contract. Read it back through Lua rather than by switching to it: the buffer is
  // modified, so an `:edit` would (rightly) refuse with E37.
  const extractedBuf = await luaResult(page,
    `local n
     for _, id in ipairs(btv.buf.list()) do
       if tostring(btv.buf.name(id)):find("extracted") then n = id end
     end
     if not n then return "no buffer" end
     return table.concat(btv.buf.lines(n, 0, -1), "|") .. "  modified=" .. tostring(btv.bo[n].modified)`);
  check("lsp: …while the extracted text sits in its buffer, modified and unsaved",
    /let extracted = 1/.test(String(extractedBuf)) && /modified=true/.test(String(extractedBuf)),
    `buffer=${JSON.stringify(extractedBuf)}`);

  // And the daemon's watch leg did not report bemtvi's own placeholder back as somebody
  // else's change: the arm re-baselined it, so no W11/W12/E211 turns up on the message
  // line. Give the daemon's poll a couple of cycles to have pushed one if it were going to.
  let sawConflict = "";
  for (let i = 0; i < 40; i++) {
    const m = String(await page.evaluate(() => window.__bemtvi.message()));
    if (/W1[12]|E211/.test(m)) { sawConflict = m; break; }
    await sleep(100);
  }
  check("lsp: …and the placeholder write is not reported back as an external change",
    sawConflict === "", `message=${JSON.stringify(sawConflict)}`);

  // ── 7. the `rename` resource operation moves the file on the DAEMON's disk ────────────
  //    The buffer half runs in the browser, the filesystem half on the daemon, and the
  //    two have to meet: the file moves over there while the open buffer's name follows
  //    over here. A browser session has no filesystem of its own to fall back on, so
  //    this only passes if the op really crossed the wire.
  const renamed = await until(
    page,
    () => {
      window.__bemtvi.execLua(
        'btv.lsp.code_action({ context = { only = { "refactor.move" } }, apply = true })',
      );
      return window.__bemtvi.execLua("return tostring(btv.buf.name(0))").then((r) => r.result);
    },
    (v) => /moved\.rs/.test(String(v)),
    15000,
  );
  check("lsp: a workspace edit's `rename` moves the file on the daemon and the buffer name follows",
    /moved\.rs/.test(String(renamed)) && existsSync(movedFile) && !existsSync(rsFile),
    `name=${JSON.stringify(renamed)} moved=${existsSync(movedFile)} original=${existsSync(rsFile)}`);
  // The file's own bytes moved with it — `let foo = …`, not the buffer's unsaved
  // `let baz = …` (bemtvi never writes an edit to disk behind you) and not an empty
  // file (which is what a "rename" that only recreated the name would leave).
  check("lsp: …carrying the file's own bytes, not an empty file",
    existsSync(movedFile) && readFileSync(movedFile, "utf8") === "let foo = bar()\n",
    existsSync(movedFile) ? JSON.stringify(readFileSync(movedFile, "utf8")) : "<missing>");

  // ── 8. the `delete` resource operation removes the file on the DAEMON's disk ──────────
  await until(
    page,
    () => {
      window.__bemtvi.execLua(
        'btv.lsp.code_action({ context = { only = { "refactor.remove" } }, apply = true })',
      );
      return window.__bemtvi.execLua("return 1").then((r) => r.result);
    },
    () => !existsSync(doomedFile),
    15000,
  );
  check("lsp: a workspace edit's `delete` removes the file on the daemon",
    !existsSync(doomedFile), `still present=${existsSync(doomedFile)}`);

  await browser.close();
} catch (e) {
  console.error("verify-lsp error:", e);
  failures++;
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — browser edit-host runs TWO LSP servers on a real bemtvi --daemon over WebTransport (merged diagnostics pushes, capability-routed hover, fanned-out completion, server-initiated applyEdit incl. create/rename/delete on the daemon's filesystem)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
