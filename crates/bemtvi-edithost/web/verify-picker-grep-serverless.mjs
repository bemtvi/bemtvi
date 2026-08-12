// Playwright verifier for the GREP PICKER on the pure web client (serverless, no daemon).
// `btv.picker`'s `live_grep` streams `rg --vimgrep`, falling back to `grep` and then to a
// transport-agnostic `btv.fs.grep` (walk + in-Lua substring match). On the serverless build
// there is no process host, so the rg/grep spawns fail loud (code -1) and the picker must
// land on the btv.fs match over OPFS — the bug was that it showed nothing. This seeds files
// under the cwd, opens live_grep, types a query, and asserts the matching lines appear.
//
// Faithfulness: the files are seeded through the same btv.fs/OPFS seam btv.fs.grep reads, the
// picker runs its real dynamic source through the production tick, and the assertion reads
// the picker's live candidate list (`btv._picker.items`).
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm + vendor/msgpack) and a Chromium for Playwright.
// Run:  node verify-picker-grep-serverless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8147;

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

const luaResult = (page, code) =>
  page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

async function settle(page, g, code, ms = 8000) {
  await luaResult(page, `${code}\nreturn 1`);
  const start = Date.now();
  for (;;) {
    const v = await page.evaluate(
      (n) => window.__bemtvi.execLua(`return tostring(_G.${n})`).then((r) => r.result), g);
    if (!/Ok\("nil"\)/.test(String(v))) return v;
    if (Date.now() - start > ms) return v;
    await sleep(40);
  }
}

// Poll the picker's live candidate list until it contains a NEEDLE match (or timeout).
async function pollPickerItems(page, ms = 8000) {
  const start = Date.now();
  for (;;) {
    const v = await luaResult(
      page,
      `local p = btv._picker\n` +
        `if not p then return "NOPICKER" end\n` +
        `local t = {}\n` +
        // A live_grep row is two-column: the `path:line:col: ` head plus the matched
        // line as `text` — the label the widget renders is their concatenation.
        `for i = 1, (p.nitems or 0) do\n` +
        `  local it = p.items[i]\n` +
        `  t[#t + 1] = (it.head or "") .. it.text\n` +
        `end\n` +
        `return table.concat(t, "\\n")`,
    );
    const s = String(v);
    if (/NEEDLE/.test(s)) return s;
    if (Date.now() - start > ms) return s;
    await sleep(60);
  }
}

const ROOT_REL = `greptest-${Date.now()}`;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/`); break; } catch { await sleep(100); }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => { if (m.type() === "error") console.log("  [page error]", m.text()); });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`); // serverless — no process host
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  check("Worker booted (serverless — no process host)", true);

  // Seed files under the cwd: one with the needle on two lines, one without.
  const seeded = await settle(page, "__seed", `btv.async(function()
       local base = vim.fn.getcwd() .. "/${ROOT_REL}"
       btv.await(btv.fs.mkdir(base .. "/sub", { recursive = true }))
       btv.await(btv.fs.write(base .. "/a.txt", "alpha NEEDLE one\\nbeta\\n"))
       btv.await(btv.fs.write(base .. "/sub/b.txt", "no match\\nx NEEDLE y\\n"))
       btv.await(btv.fs.write(base .. "/c.txt", "nothing here\\n"))
       _G.__seed = "ok"
     end)()`);
  check("seeded files under cwd via btv.fs (OPFS)", /ok/.test(String(seeded)), `seed=${JSON.stringify(seeded)}`);

  // Open live_grep and type the query — the dynamic source re-runs and, with no rg/grep,
  // falls back to btv.fs.grep, which must surface both NEEDLE lines.
  await luaResult(page, `btv.picker.open('live_grep')`);
  await page.evaluate(() => window.__bemtvi.feed("NEEDLE"));
  const items = await pollPickerItems(page);
  // The lua result arrives as a debug repr, so the joiner shows up as the two-char
  // escape `\n` rather than a newline — split on either.
  const hits = String(items).split(/\\n|\n/).filter((l) => /NEEDLE/.test(l));
  check(
    "live_grep falls back to btv.fs.grep and lists the matching lines (no rg/grep)",
    hits.length >= 2 && hits.some((l) => /a\.txt/.test(l)) && hits.some((l) => /b\.txt/.test(l)),
    `hits=${JSON.stringify(hits)}`,
  );

  // …and RENDERS them as two-column rows: the location head, the matched line, and the
  // hit bolded. A live_grep row bypasses the fuzzy matcher, so this highlight can only
  // come from the source's own match range riding the `layouts` projection.
  const rendered = await page.evaluate(() =>
    // The list rows only — the prompt row and the preview pane also carry the query.
    // Matched chars are one span each, so join them back into the highlighted text.
    [...document.querySelectorAll("#grid .pmenu .row")]
      .map((r) => ({
        text: r.textContent,
        marked: [...r.querySelectorAll(".pmenu-match, span[style]")]
          .map((s) => s.textContent)
          .join(""),
      }))
      .filter((r) => /\.txt:\d+:\d+:/.test(r.text)),
  );
  check(
    "the rendered rows keep the file name AND the matched line",
    rendered.length >= 2 && rendered.every((r) => /\.txt:\d+:\d+: +\S.*NEEDLE/.test(r.text)),
    `rendered=${JSON.stringify(rendered)}`,
  );
  check(
    "the hit itself is highlighted on the row",
    rendered.length >= 2 && rendered.every((r) => r.marked === "NEEDLE"),
    `rendered=${JSON.stringify(rendered)}`,
  );

  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await settle(page, "__rm", `btv.fs.remove(vim.fn.getcwd() .. "/${ROOT_REL}", { recursive = true })
       :next(function() _G.__rm = "gone" end, function(e) _G.__rm = "err:" .. e.code end)`);
} catch (e) {
  check("harness ran without throwing", false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — serverless grep picker lists matches via the btv.fs.grep fallback"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
