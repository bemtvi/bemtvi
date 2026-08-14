// Playwright verifier for the BUNDLED markdown grammar and the injections that make it
// whole. Drives the real wasm edit-host in headless Chromium against a `.md` buffer and
// asserts, on the rendered DOM:
//   1. block structure highlights — a heading is coloured by the markdown grammar itself;
//   2. INLINE markup highlights — `**strong**` is coloured, which can only come from the
//      markdown_inline grammar the block grammar injects for every `(inline)` node;
//   3. FENCED code highlights — `fn` inside a ```rust fence is coloured as a rust keyword,
//      i.e. the fence's info string routed that region to the rust grammar;
//   4. plain prose stays uncoloured (the paint follows captures, it isn't blanket-tinted);
//   5. all of it offline — zero CDN traffic, since both markdown parsers ship in the bundle
//      (they have to: upstream publishes no markdown `.wasm`, so `:TSInstall` can't get one).
//
// Plus a direct probe of the highlighter module for the capture NAMES behind those colours,
// which pins the semantics a colour comparison can only imply — including that
// `markup.raw.block` is dropped (it's a full-line background group, and this build paints
// spans as foregrounds only).
//
//   node verify-markdown-highlight.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8173;

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

// The colours `web/highlight.js`'s built-in `FG` table gives these captures (no colorscheme
// is loaded in this test, so the static fallback is what paints).
const HEADING1 = "#61afef";
const STRONG = "#e5c07b";
const KEYWORD = "#c678dd";

const DOC = [
  "# Title",
  "",
  "plain and **strong** here",
  "",
  "```rust",
  "fn f() {}",
  "```",
];

// Every (text, colour) run of the focused window's rendered rows.
async function rows(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row")].map((row) =>
      [...row.querySelectorAll("span")].map((s) => ({
        text: s.textContent,
        color: (s.getAttribute("style") || "").match(/color\s*:\s*([^;]+)/)?.[1]?.trim() || null,
      })),
    ),
  );
}

// Poll the rendered rows until `done` accepts them (grammars load asynchronously, and
// markdown needs THREE: the block grammar, the inline one it injects, and rust for the
// fence — each landing on its own repaint).
async function waitRows(page, done) {
  let last = null;
  for (let i = 0; i < 80; i++) {
    last = await rows(page);
    if (done(last)) return { ok: true, rows: last };
    await sleep(100);
  }
  return { ok: false, rows: last };
}

// The rendered run containing `word` on `row`, or null. The renderer merges adjacent
// same-colour cells into one run and a capture often covers more than the word asserted
// (`markup.heading.1` spans the `#` marker, `markup.strong` its `**` delimiters), so this
// matches by substring rather than equality.
const runWith = (rs, row, word) => rs[row]?.find((r) => (r.text || "").includes(word)) ?? null;
const colorOf = (rs, row, word) => runWith(rs, row, word)?.color ?? null;

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  const errors = [];
  page.on("pageerror", (e) => { errors.push(e.message); console.log("  [pageerror]", e.message); });

  // Markdown must highlight with NO network: fail any CDN request rather than serving it,
  // so a grammar that quietly went missing from the bundle shows up as a failure here
  // instead of being fetched (and it couldn't be fetched — there is no markdown wasm
  // upstream to fetch).
  const cdn = [];
  await page.route("**/cdn.jsdelivr.net/**", (route) => {
    cdn.push(new URL(route.request().url()).pathname);
    return route.fulfill({ status: 404, body: "offline in this test" });
  });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  await page.evaluate(() => window.__bemtvi.feed(":e demo.md<CR>"));
  await page.evaluate((doc) => window.__bemtvi.feed(`ggdGi${doc.join("<CR>")}<Esc>`), DOC);

  const ft = await page.evaluate(() =>
    (window.__bemtvi.frame()?.windows || []).find((w) => w.focused)?.filetype ?? null);
  check("buffer: demo.md is filetype markdown", ft === "markdown", String(ft));

  // Wait for all three grammars to have painted. Each lands on its own async load and they
  // finish in no fixed order, so wait for one span from EACH — the block grammar's heading,
  // the injected markdown_inline's emphasis, and rust's keyword inside the fence — rather
  // than for whichever happens to be last.
  const painted = await waitRows(page, (rs) =>
    colorOf(rs, 0, "Title") !== null && colorOf(rs, 2, "strong") !== null && colorOf(rs, 5, "fn") !== null);
  const rs = painted.rows;
  const shown = JSON.stringify(rs.slice(0, 7).map((r) => r.filter((x) => x.color).map((x) => `${x.text}=${x.color}`)));

  check("block: heading text coloured by the markdown grammar",
    colorOf(rs, 0, "Title") === HEADING1, shown);
  check("inline: **strong** coloured by the injected markdown_inline grammar",
    colorOf(rs, 2, "strong") === STRONG, shown);
  check("fence: `fn` in a ```rust block coloured as a rust keyword",
    colorOf(rs, 5, "fn") === KEYWORD, shown);
  check("prose: plain words stay uncoloured",
    colorOf(rs, 2, "plain") === null, shown);
  check("offline: no CDN request for any of it", cdn.length === 0, JSON.stringify(cdn));

  // ---- Capture names behind those colours (via the page's own highlighter) ----
  const caps = await (async () => {
    for (let i = 0; i < 60; i++) {
      const sp = await page.evaluate((doc) => window.__bemtvi.tsSpans("markdown", doc.join("\n") + "\n"), DOC);
      if (sp && sp[5] && sp[5].length && sp[2] && sp[2].length) {
        return { heading: sp[0], inline: sp[2], fence: sp[5], all: sp.flat() };
      }
      await sleep(100);
    }
    return null;
  })();

  check("captures: heading line carries markup.heading.1",
    !!caps && caps.heading.includes("markup.heading.1"), JSON.stringify(caps?.heading));
  check("captures: inline line carries markup.strong (from markdown_inline)",
    !!caps && caps.inline.includes("markup.strong"), JSON.stringify(caps?.inline));
  check("captures: fence line carries rust's own keyword/function captures",
    !!caps && caps.fence.includes("keyword") && caps.fence.some((c) => c.startsWith("function")),
    JSON.stringify(caps?.fence));
  check("captures: markup.raw.block dropped (a line background, not a foreground)",
    !!caps && !caps.all.includes("markup.raw.block"), JSON.stringify(caps?.all?.slice(0, 20)));

  // ---- A colorscheme's markdown colours reach the browser ----
  // The wire only carries the capture names `SYNTAX_CAPTURES` (bemtvi-server/src/redraw.rs)
  // lists, so without the `markup.*` family there a theme's markdown colours would never
  // arrive and the client's static fallback would be the only paint. Set the group directly
  // (colorscheme-agnostic, as verify-colorscheme.mjs does) and watch the heading follow it.
  await page.evaluate(() => window.__bemtvi.execLua(
    "vim.api.nvim_set_hl(0, '@markup.heading.1', { fg = '#ff00cc' })\n" +
    "vim.api.nvim_set_hl(0, '@markup.strong',    { fg = '#00ddaa' })\n"));
  // The map only carries captures the active theme actually defines, and this test loads no
  // colorscheme — so the two groups just set are exactly what must appear. Absent from
  // `SYNTAX_CAPTURES`, neither would cross the wire however it was themed.
  const wireHasMarkup = await page.evaluate(() => {
    const t = window.__bemtvi.frame()?.theme || {};
    return ["markup.heading.1", "markup.strong"].filter((k) => t[k] == null);
  });
  check("wire: the redraw's `theme` map carries the markup captures the theme defines",
    wireHasMarkup.length === 0, `missing: ${JSON.stringify(wireHasMarkup)}`);

  const themed = await waitRows(page, (rs) => colorOf(rs, 0, "Title") === "#ff00cc");
  check("theme: a colorscheme's @markup.heading.1 recolours the heading",
    colorOf(themed.rows, 0, "Title") === "#ff00cc",
    JSON.stringify(themed.rows.slice(0, 3).map((r) => r.filter((x) => x.color).map((x) => `${x.text}=${x.color}`))));
  check("theme: and its @markup.strong recolours the injected emphasis",
    colorOf(themed.rows, 2, "strong") === "#00ddaa",
    JSON.stringify(themed.rows[2]?.filter((x) => x.color).map((x) => `${x.text}=${x.color}`)));

  check("no page errors", errors.length === 0, JSON.stringify(errors));
  await browser.close();
} catch (e) {
  console.log("FAIL  verifier threw");
  console.log(e);
  failures++;
}

cleanup();
console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exit(failures ? 1 : 0);
