// Playwright verifier for treesitter QUERIES on the wasm edit-host: `btv.treesitter.query`
// and a decor provider's `query`. Drives the real editor in headless Chromium; the
// worker's query runner (web/ts-query.js), reached synchronously from the Rust tick
// through eh_js_ts_query (web/eh-lib.js → WasmSyntax::query in src/lib.rs), must answer
// with the nodes the markdown grammar actually built: the table outside a code fence and
// not the one inside, byte columns past multi-byte text, results that follow an edit
// (the runner's per-buffer tree, updated incrementally), and a provider's ctx.query.
//
// The first query after boot finds the grammar still loading, so it also covers the
// wake path: the runner reports the load and the core re-runs what was parked.
//
// Hermetic: markdown is a BUNDLED grammar, so nothing is fetched at runtime.
//
//   node verify-treesitter-query.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8107;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Chromium.app/Contents/MacOS/Chromium`),
  ].sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

// Run a Lua chunk and return the string it returned. `execLua`'s `result` is a Rust-Debug
// rendering of the value (`ok:String(Utf8String { s: Ok("…") })`), so unwrap it to the
// bare string, or null on an error / a non-string result (as verify-alt-ctrl.mjs does).
async function lua(page, code) {
  const rendered = await page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => String(r.result)), code);
  if (!rendered.startsWith("ok:")) return null;
  const m = rendered.match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1].replace(/\\(.)/g, "$1") : null;
}

// Poll `code` (a Lua chunk returning a string) until `ok(value)` or the budget runs out.
async function until(page, code, ok, tries = 100) {
  let v;
  for (let i = 0; i < tries; i++) {
    v = await lua(page, code);
    if (ok(v)) return v;
    await sleep(100);
  }
  return v;
}

// Start `btv.treesitter.query(<args>)`, then wait for it to settle. Resolves to the
// captured nodes as `name:row,col-endRow,endCol kind` joined by `;`, or `error:<msg>`.
async function query(page, args) {
  await lua(page, `
    _Q = nil
    btv.treesitter.query(${args}):next(function(res)
      local out = {}
      for _, m in ipairs(res.matches) do
        local names = {}
        for name in pairs(m.captures) do names[#names + 1] = name end
        table.sort(names)
        for _, name in ipairs(names) do
          for _, n in ipairs(m.captures[name]) do
            out[#out + 1] = ("%s:%d,%d-%d,%d %s"):format(name, n[1], n[2], n[3], n[4], n.kind)
          end
        end
      end
      _Q = "ok:" .. table.concat(out, ";")
    end, function(err) _Q = "error:" .. tostring(err) end)
    return ""`);
  const v = await until(page, "return _Q or ''", (r) => /^(ok|error):/.test(r || ""));
  if (!/^(ok|error):/.test(v || "")) return "<never settled>";
  return v.startsWith("ok:") ? v.slice(3) : v;
}

// A table inside a fence (rows 3-4) and a real one (rows 7-10).
const SAMPLE = [
  "# Tables",
  "",
  "```",
  "| not | a table |",
  "|-----|---------|",
  "```",
  "",
  "| Queue | Upstream |",
  "|---|---|",
  "| a | b |",
  "| c | d |",
];

try {
  const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
  const cleanup = () => { try { srv.kill(); } catch {} };
  process.on("exit", cleanup);
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  page.on("console", (m) => { const t = m.text(); if (m.type() === "error" || t.includes("bemtvi")) console.log("  [console]", t); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  await page.evaluate(() => window.__bemtvi.feed(":e /q.md<CR>"));
  await lua(page, `vim.api.nvim_buf_set_lines(0, 0, -1, false, ${JSON.stringify(SAMPLE).replace(/^\[/, "{").replace(/\]$/, "}")}); return ""`);
  const ft = await lua(page, "return btv.bo.filetype");
  check("the buffer is markdown", ft === "markdown", ft);

  // 1. The first query parks on the loading grammar and settles when it lands.
  const tables = await query(page, `0, "(pipe_table) @table"`);
  check("finds the table outside the fence, not the one inside", tables === "table:7,0-11,0 pipe_table", tables);

  // 2. A row range keeps a crossing node whole and drops the rest.
  const crossing = await query(page, `0, "(pipe_table) @table", { rows = { 9, 10 } }`);
  check("a row range keeps a crossing table whole", crossing === "table:7,0-11,0 pipe_table", crossing);
  const none = await query(page, `0, "(pipe_table) @table", { rows = { 0, 2 } }`);
  check("a row range with no table finds nothing", none === "", none);
  // `{0, 0}` in particular: tree-sitter reads an end point of (0, 0) as "no limit".
  const empty = await query(page, `0, "(pipe_table) @table", { rows = { 0, 0 } }`);
  check("an empty row range matches nothing", empty === "", empty);

  // 3. Byte columns past multi-byte text: `| 名前 | x |` — the second cell starts after
  //    `| 名前 |` (1 + 1 + 6 + 1 + 1 = 10 bytes in), past the leading space.
  await lua(page, `vim.api.nvim_buf_set_lines(0, 7, 8, false, { "| 名前 | x |" }); return ""`);
  const cells = await query(page, `0, "(pipe_table_header (pipe_table_cell) @cell)"`);
  check(
    "cell columns are UTF-8 bytes, after an edit (incremental tree)",
    cells === "cell:7,2-7,9 pipe_table_cell;cell:7,11-7,13 pipe_table_cell",
    cells,
  );

  // 4. An edit that removes a body row shortens the table.
  await lua(page, `vim.api.nvim_buf_set_lines(0, 10, 11, false, {}); return ""`);
  const shorter = await query(page, `0, "(pipe_table) @table"`);
  check("the result follows an edit", shorter === "table:7,0-10,0 pipe_table", shorter);

  // 5. A bad query rejects with the reason.
  const bad = await query(page, `0, "(no_such_node) @x"`);
  check("an invalid query rejects", /^error:invalid query/.test(bad), bad);

  // 5b. The incremental tree agrees with a fresh parse. Edits that change the structure
  //     (a table row becomes a fence opener and back, a heading appears mid-table, a
  //     multi-byte cell grows) go through the runner's cached tree for /q.md; after each,
  //     the same text is queried in a second buffer, parsed from scratch, and the two
  //     answers must match. A wrong edit range would leave stale nodes in the first.
  const STRUCT_Q = `"(pipe_table) @t (fenced_code_block) @f (atx_heading) @h (pipe_table_cell) @c"`;
  // The buffer under test, by id: the fresh parses happen in other buffers.
  await lua(page, `_QB = btv.buf.current(); return ""`);
  const edits = [
    `btv.buf.set_lines(_QB, 8, 9, false, { "\`\`\`" })`,
    `btv.buf.set_lines(_QB, 8, 9, false, { "|---|---|" })`,
    `btv.buf.set_lines(_QB, 9, 9, false, { "## mid", "| e | f |" })`,
    `btv.buf.set_text(_QB, 7, 2, 7, 2, { "ラーメン" })`,
    `btv.buf.set_lines(_QB, 0, 3, false, {})`,
  ];
  let agree = true, detail = "";
  for (const e of edits) {
    await lua(page, `${e}; return ""`);
    const inc = await query(page, `_QB, ${STRUCT_Q}`);
    // A new buffer each time (`:new`, a split, so the edited buffer needn't be written
    // first), so its tree is a fresh parse of exactly this text. Closed again after.
    await page.evaluate(() => window.__bemtvi.feed(":new<CR>"));
    await lua(page, `btv.bo.filetype = "markdown"
      btv.buf.set_lines(0, 0, -1, false, btv.buf.lines(_QB, 0, -1)); return ""`);
    const full = await query(page, `0, ${STRUCT_Q}`);
    await page.evaluate(() => window.__bemtvi.feed(":bd!<CR>"));
    if (inc !== full || /^error|never/.test(inc)) {
      agree = false;
      detail = `after ${e}:\n  incremental ${inc}\n  fresh       ${full}`;
      break;
    }
  }
  check("the incremental tree matches a fresh parse after structural edits", agree, detail);

  // 6. A decor provider's `query` reaches it as ctx.query (on the sample text again).
  // `:bd!` leaves the splits `:new` opened, so go back to one full-height window.
  await page.evaluate(() => window.__bemtvi.feed(":b /q.md<CR>:only!<CR>"));
  await lua(page, `vim.api.nvim_buf_set_lines(0, 0, -1, false, ${JSON.stringify(SAMPLE).replace(/^\[/, "{").replace(/\]$/, "}")}); return ""`);
  await lua(page, `
    _D = nil
    btv.decor.provider({
      name = "probe",
      bufs = { filetype = { "markdown" } },
      query = "(pipe_table) @table (fenced_code_block) @fence",
      on_range = function(ctx, publish)
        local q = ctx.query
        if q.matches then
          local out = {}
          for _, m in ipairs(q.matches) do
            for name, nodes in pairs(m.captures) do
              for _, n in ipairs(nodes) do out[#out + 1] = name .. ":" .. n[1] .. "-" .. n[3] end
            end
          end
          table.sort(out)
          _D = table.concat(out, ";")
        else
          _D = "error:" .. q.error
        end
        publish({})
      end,
    })
    btv.decor.invalidate()
    return ""`);
  const decor = await until(page, "return _D or ''", (v) => v === "fence:2-6;table:7-11");
  check("a decor provider gets its query answered in ctx", decor === "fence:2-6;table:7-11", decor);

  // 6b. One very long line (a minified file's shape): a table row of 20,000 cells. Each
  //     cell's byte column is converted in one sweep, not by re-walking the row per
  //     capture (which would be ~10^9 character reads here and freeze the worker).
  await page.evaluate(() => window.__bemtvi.feed(":new<CR>"));
  await lua(page, `btv.bo.filetype = "markdown"
    local cells = {}
    for i = 1, 20000 do cells[i] = " c" .. i .. " " end
    local dashes = {}
    for i = 1, 20000 do dashes[i] = "---" end
    btv.buf.set_lines(0, 0, -1, false, { "|" .. table.concat(cells, "|") .. "|", "|" .. table.concat(dashes, "|") .. "|" })
    return ""`);
  const t0 = Date.now();
  await lua(page, `_L = nil
    btv.treesitter.query(0, "(pipe_table_header (pipe_table_cell) @c)", { rows = { 0, 1 } }):next(function(res)
      local caps = res.matches[1] and res.matches[1].captures.c or {}
      local n = 0
      for _, m in ipairs(res.matches) do n = n + #m.captures.c end
      local lastc = res.matches[#res.matches].captures.c[1]
      _L = n .. " last@" .. lastc[2]
    end, function(e) _L = "error:" .. tostring(e) end)
    return ""`);
  const longLine = await until(page, "return _L or ''", (v) => v !== "" && v !== null, 200);
  const longMs = Date.now() - t0;
  // The last cell starts after 19,999 cells of " cN |" plus the leading "|".
  let expectCol = 1;
  for (let i = 1; i < 20000; i++) expectCol += (" c" + i + " ").length + 1;
  check(
    "a 20,000-cell line answers with every cell, quickly",
    longLine === `20000 last@${expectCol + 1}` && longMs < 10000,
    `${longLine} in ${longMs}ms (expected 20000 last@${expectCol + 1})`,
  );
  await page.evaluate(() => window.__bemtvi.feed(":only!<CR>"));

  // 7. A large buffer: the table at its very end is found, at the right rows, from a
  //    row range near the end. (That a repeat query reuses the tree rather than
  //    re-parsing can't be timed from the page: the execLua round trips on a buffer this
  //    size dwarf it. Measured in the worker instead: the first query parses in ~460ms,
  //    each repeat with the text unchanged takes 0.0ms in the tree step.)
  await page.evaluate(() => window.__bemtvi.feed(":new<CR>"));
  await lua(page, `btv.bo.filetype = "markdown"; local t = {}
    for i = 1, 40000 do t[#t + 1] = "paragraph " .. i .. " with *some* \`inline\` text"; t[#t + 1] = "" end
    t[#t + 1] = "| k | v |"; t[#t + 1] = "|---|---|"; t[#t + 1] = "| a | 1 |"
    btv.buf.set_lines(0, 0, -1, false, t); return ""`);
  const count = await until(page, "return tostring(btv.buf.line_count(0))", (v) => v === "80003");
  check("the large buffer holds 80003 lines", count === "80003", count);
  const tail = await query(page, `0, "(pipe_table) @t", { rows = { 79990, 80003 } }`);
  check("finds the table at the end of a large buffer", tail === "t:80000,0-80003,0 pipe_table", tail);

  // 8. Edits that land while a parse is still running are queued and applied after it:
  //    a fresh large buffer starts parsing in the background on its first query, and on
  //    the next loop turn two rows are inserted above the table and another query makes
  //    the core hand that edit to the runner (it forwards edits when it next syncs the
  //    engine) while the parse is still running. The answer must reflect them (the
  //    table two rows lower),
  //    not the text the parse started from. The markdown probe provider above is
  //    re-scoped to nothing first, so it doesn't parse the buffer ahead of the query.
  await lua(page, `btv.decor.provider({ name = "probe", bufs = { filetype = { "none" } },
    on_range = function() end }); return ""`);
  await page.evaluate(() => window.__bemtvi.feed(":new<CR>"));
  await lua(page, `btv.bo.filetype = "markdown"; local t = {}
    for i = 1, 160000 do t[#t + 1] = "paragraph " .. i .. " with *some* \`inline\` text"; t[#t + 1] = "" end
    t[#t + 1] = "| k | v |"; t[#t + 1] = "|---|---|"; t[#t + 1] = "| a | 1 |"
    btv.buf.set_lines(0, 0, -1, false, t); return ""`);
  await until(page, "return tostring(btv.buf.line_count(0))", (v) => v === "320003");
  await lua(page, `_MID = nil
    btv.treesitter.query(0, "(pipe_table) @t", { rows = { 319990, 320010 } }):next(function(res)
      local m = res.matches[1]; _MID = m and tostring(m.captures.t[1][1]) or "none"
    end, function(e) _MID = "error:" .. tostring(e) end)
    btv.on_next_tick(function()
      btv.buf.set_lines(0, 10, 10, false, { "inserted one", "inserted two" })
      -- The core hands edits to the engine when it next syncs it, i.e. at the next
      -- query: this one makes the edit reach the runner while the parse is running.
      btv.treesitter.query(0, "(pipe_table) @t", { rows = { 0, 1 } })
    end)
    return ""`);
  const mid = await until(page, "return _MID or ''", (v) => v !== "" && v !== null, 200);
  const after = await query(page, `0, "(pipe_table) @t", { rows = { 319990, 320010 } }`);
  check(
    "edits during a parse are applied after it (table two rows down)",
    after === "t:320002,0-320005,0 pipe_table",
    `after: ${after} (the first query settled with row ${mid})`,
  );
  await page.evaluate(() => window.__bemtvi.feed(":only!<CR>"));

  await browser.close();
  cleanup();
} catch (e) {
  console.error(e);
  failures++;
}

console.log(failures === 0
  ? "\nALL PASS — tree-sitter queries on the edit-host (btv.treesitter.query + decor query)"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
