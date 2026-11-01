// The gutter text column on web: an `btv.view` pane (the SourceGit diff's shape) with
// NO line numbers puts old/new line numbers in the column via `gutter_text` marks on
// `:set_decor`. Check the column reaches the redraw frame (its width is the widest mark,
// a shorter one is padded to it), the rendered rows paint it between the gutter and the
// text, and the text origin the pointer hit-test reads counts it.
//
//   node verify-gutter-text.mjs   (serves web/ live; uses the built dist/eh.mjs)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8179;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const lin = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  if (lin.length) return lin[lin.length - 1];
  const mac = globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`).sort();
  return mac.length ? mac[mac.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
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
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  page.on("console", (m) => { if (m.type() === "error") console.log("  [console.error]", m.text()); });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  const luaResult = (code) => page.evaluate((c) => window.__bemtvi.execLua(c).then((r) => r.result), code);

  await page.evaluate(() => window.__bemtvi.feed("imain<Esc>"));
  await luaResult(`
    btv.hl.define(0, "GtOld", { fg = "#f38ba8" })
    btv.hl.define(0, "GtBg", { bg = "#402020" })
    vw = btv.view.create{ name = "diff" }
    vw:set_lines{ "alpha", "beta", "gamma" }
    vw:mount{ split = "vsplit" }
    return true
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await luaResult(`
    local win = vw:winid()
    if win then vim.wo[win].number = false; vim.wo[win].relativenumber = false; vim.wo[win].signcolumn = "no" end
    ns = btv.ns.create("gutter-text-test")
    vw:set_decor(ns, {
      { line = 0, col = 0, gutter_text = { { " 12", "GtOld" }, { " 13 " } } },
      { line = 2, col = 0, gutter_text = { { " 9", "GtBg" }, { " " } } },
    })
    return true
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(250);

  const frame = await page.evaluate(() => {
    const wins = window.__bemtvi.frame()?.windows || [];
    const w = wins.find((x) => (x.lines || []).includes("alpha")) || {};
    return {
      found: !!w.lines,
      width: w.gutter_text_width || 0,
      rows: (w.gutter_text || []).map((r) => (Array.isArray(r) ? r.map((c) => c[0]).join("") : null)),
    };
  });
  check("the frame carries the column, as wide as the widest mark", frame.found && frame.width === 7, `frame=${JSON.stringify(frame)}`);
  check("the frame carries each row's text", frame.rows[0] === " 12 13 " && frame.rows[1] === null && frame.rows[2] === " 9 ", `frame=${JSON.stringify(frame)}`);

  const painted = await page.evaluate(() => {
    const box = [...document.querySelectorAll("#grid .win")].find((b) => b.textContent.includes("alpha"));
    if (!box) return null;
    const rows = [...box.querySelectorAll(".row")].map((r) => r.textContent);
    return { rows: rows.slice(0, 3), tp: box.dataset.tp ?? null };
  });
  check("row 1 paints the column then the text", painted?.rows[0]?.startsWith(" 12 13 alpha"), `painted=${JSON.stringify(painted)}`);
  check("a row with no mark is padded blank", painted?.rows[1]?.startsWith("       beta"), `painted=${JSON.stringify(painted)}`);
  check("a shorter mark is padded on the right", painted?.rows[2]?.startsWith(" 9     gamma"), `painted=${JSON.stringify(painted)}`);

  // A chunk whose group sets only a background keeps the number column's foreground
  // (as the GUI and the terminal paint it), not the text colour.
  const bgOnly = await page.evaluate(() => {
    const box = [...document.querySelectorAll("#grid .win")].find((b) => b.textContent.includes("alpha"));
    const row = box && [...box.querySelectorAll(".row")][2];
    const span = row && [...row.querySelectorAll("span")].find((s) => s.textContent === " 9");
    const plain = row && [...row.querySelectorAll("span.gutter, span.gutter-cur")].find((s) => s.textContent.trim() === "");
    if (!span || !plain) return null;
    const cs = getComputedStyle(span);
    return { color: cs.color, bg: cs.backgroundColor, want: getComputedStyle(plain).color };
  });
  check("a background-only chunk keeps the number colour", bgOnly && bgOnly.color === bgOnly.want && bgOnly.bg === "rgb(64, 32, 32)", `bgOnly=${JSON.stringify(bgOnly)}`);

  // A fold column alone (no numbers, signs or gutter text) still paints its cells,
  // so the text sits where the server's text width and the hit-test put it.
  await luaResult(`
    vw:set_decor(ns, {})
    vim.wo[vw:winid()].foldcolumn = 2
    return true
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(250);
  const foldOnly = await page.evaluate(() => {
    const box = [...document.querySelectorAll("#grid .win")].find((b) => b.textContent.includes("alpha"));
    const w = (window.__bemtvi.frame()?.windows || []).find((x) => (x.lines || []).includes("alpha")) || {};
    return { fcw: w.foldcolumn_width || 0, gtw: w.gutter_text_width || 0, row: box && [...box.querySelectorAll(".row")][0]?.textContent };
  });
  check("a fold column alone still offsets the text", foldOnly.fcw === 2 && foldOnly.gtw === 0 && foldOnly.row?.startsWith("  alpha"), `foldOnly=${JSON.stringify(foldOnly)}`);

  // The text origin the pointer hit-test reads (`data-tp`) counts the column. Only an
  // editable window in a `'modeless'` session takes the I-beam, so this turns that on
  // and uses the ordinary buffer showing "main".
  await luaResult(`
    btv.o.modeless = true
    local buf = nil
    for _, b in ipairs(btv.buf.list()) do
      if btv.buf.lines(b, 0, 1)[1] == "main" then buf = b end
    end
    btv.buf.set_extmark(buf, ns, 0, 0, { gutter_text = { { "abcde" } } })
    return true
  `);
  await page.evaluate(() => window.__bemtvi.feed("<Esc>"));
  await sleep(250);
  const origin = await page.evaluate(() => {
    const box = [...document.querySelectorAll("#grid .win")].find((b) => b.textContent.includes("main"));
    const w = (window.__bemtvi.frame()?.windows || []).find((x) => (x.lines || []).includes("main")) || {};
    const number = (w.number || w.relativenumber) ? (w.number_width || 0) : 0;
    return { tp: box?.dataset.tp ?? null, want: String((w.foldcolumn_width || 0) + (w.sign_width || 0) + number + 5) };
  });
  check("the text origin counts the column", origin.tp !== null && origin.tp === origin.want, `origin=${JSON.stringify(origin)}`);

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — gutter text renders in its own column on web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
