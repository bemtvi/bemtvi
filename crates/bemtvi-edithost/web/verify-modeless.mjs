// Playwright verifier for the modeless editing model (`'modeless'`) in the web edit-host.
//
// The browser is one of the two clients this model is for, and its key path is its own:
// the page encodes keydown events into notation, Chrome keeps some chords for itself, and
// on a Mac the layout's modifier is Command, which the page otherwise leaves to the
// browser. So this drives REAL keydown events (page.keyboard, through the focused #kbd
// proxy) rather than the `window.__bemtvi.feed` hook, which would bypass all of that.
// The Mac branch spoofs `navigator.platform`/`userAgentData` before the page's script runs.
//
// What it covers: the page reports its client kind and platform, Control+Shift chords
// reach the editor with their Shift (`<C-S-z>`, not `<C-z>`), Shift+arrows select and
// typing replaces, and on a Mac the layout is on Command. Then the look: the cursor is a
// blinking bar caret (at a selection's head), and the pointer is an I-beam over editable
// text but the arrow over the gutter and over a popup.
//
// Prereqs: ./build.sh (dist/eh.mjs + eh.wasm) and a Chromium for Playwright. Run:
//   node verify-modeless.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8137;
const ORIGIN = `http://localhost:${PORT}`;

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

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

// Run a Lua chunk and return the string it returned (see verify-alt-ctrl.mjs).
async function luaStr(page, code) {
  const rendered = await page.evaluate(
    (c) => window.__bemtvi.execLua(c).then((r) => String(r.result)), code);
  if (!rendered.startsWith("ok:")) return null;
  const m = rendered.match(/Ok\("((?:[^"\\]|\\.)*)"\)/);
  return m ? m[1] : null;
}

const text = (page) => luaStr(page, "return table.concat(btv.buf.lines(0, 0, -1, false), '|')");

// Poll until the buffer reads `want` (keys land a tick after the press); return what it
// ended on, so a failure reports the real text.
async function waitText(page, want) {
  let got = null;
  for (let i = 0; i < 60; i++) {
    got = await text(page);
    if (got === want) return got;
    await sleep(50);
  }
  return got;
}

// A fresh page on the given spoofed platform, booted, focused, and modeless.
async function open(browser, { mac }) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  await page.addInitScript((isMac) => {
    const platform = isMac ? "MacIntel" : "Linux x86_64";
    Object.defineProperty(navigator, "platform", { get: () => platform });
    Object.defineProperty(navigator, "userAgentData", {
      get: () => ({ platform: isMac ? "macOS" : "Linux" }),
    });
  }, mac);
  await page.goto(`${ORIGIN}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await page.evaluate(() => document.getElementById("kbd").focus());
  await luaStr(page, "btv.cmd('enew!') btv.o.modeless = true return 'ok'");
  return page;
}

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`${ORIGIN}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });

  // ── Linux: the Control layout ─────────────────────────────────────────────────
  {
    const page = await open(browser, { mac: false });
    const caps = await luaStr(page,
      "local c = btv.ui.caps() return c.client .. '|' .. c.platform .. '|' .. tostring(btv.o.modeless)");
    check("the page reports a web client on linux, and the session is modeless",
      caps === "web|linux|true", `caps = ${JSON.stringify(caps)}`);

    await page.keyboard.type("foo bar");
    check("typing edits straight away", (await waitText(page, "foo bar")) === "foo bar",
      `text = ${JSON.stringify(await text(page))}`);

    await page.keyboard.press("Control+z");
    check("Control+Z undoes a word", (await waitText(page, "foo")) === "foo",
      `text = ${JSON.stringify(await text(page))}`);

    // The Shift has to survive the page's encoding: `<C-S-z>` is redo, a bare `<C-z>`
    // would undo again.
    await page.keyboard.press("Control+Shift+Z");
    check("Control+Shift+Z redoes (the page sends `<C-S-z>`)",
      (await waitText(page, "foo bar")) === "foo bar",
      `text = ${JSON.stringify(await text(page))}`);

    await page.keyboard.press("Home");
    await page.keyboard.press("Shift+ArrowRight");
    await page.keyboard.press("Shift+ArrowRight");
    await page.keyboard.press("Shift+ArrowRight");
    await page.keyboard.type("X");
    check("Shift+arrows select and typing replaces",
      (await waitText(page, "X bar")) === "X bar",
      `text = ${JSON.stringify(await text(page))}`);

    await page.keyboard.press("Escape");
    const mode = await page.evaluate(() => window.__bemtvi.mode());
    check("Escape stays in Insert", mode === "INSERT", `mode = ${JSON.stringify(mode)}`);

    // ── the caret and the pointer ──
    const caret = () => page.evaluate(() => {
      const el = document.getElementById("btv-cursor");
      return el ? { cls: el.className, anim: getComputedStyle(el).animationName, text: el.textContent } : null;
    });
    const caretIs = async (cls) => {
      let got = null;
      for (let i = 0; i < 40; i++) {
        got = await caret();
        if (got && got.cls === cls) return got;
        await sleep(50);
      }
      return got;
    };
    let c = await caretIs("caret");
    check("the cursor is a blinking caret while typing",
      c?.cls === "caret" && c?.anim === "caret-blink", JSON.stringify(c));
    // "X bar": selecting `X b` left to right leaves the caret on `a`, past the `b`.
    await page.keyboard.press("Home");
    for (let i = 0; i < 3; i++) await page.keyboard.press("Shift+ArrowRight");
    await sleep(200);
    c = await caret();
    check("a left-to-right selection puts the caret past its last character",
      c?.cls === "caret" && c?.text === "a", JSON.stringify(c));
    await page.keyboard.press("Escape");

    // Move the pointer to the middle of grid cell (row, col) and read the grid's cursor.
    const pointerAt = async (row, col) => {
      const at = await page.evaluate(() => {
        const g = document.getElementById("grid");
        const r = g.getBoundingClientRect();
        return { left: r.left, top: r.top, cw: parseFloat(g.style.getPropertyValue("--cw")),
          ch: parseFloat(g.style.getPropertyValue("--ch")) };
      });
      await page.mouse.move(at.left + (col + 0.5) * at.cw, at.top + (row + 0.5) * at.ch);
      return page.evaluate(() => document.getElementById("grid").style.cursor);
    };
    // The page's own config numbers the lines, so the text starts past a gutter.
    await luaStr(page, "btv.cmd('set number') return 'ok'");
    await sleep(200);
    check("the pointer is an I-beam over the text", (await pointerAt(0, 8)) === "text");
    check("…and the arrow over the line numbers", (await pointerAt(0, 0)) === "");
    await luaStr(page, "btv.ui.float({ 'a hover popup', 'over the text' }, { relative = 'editor', persist = true }) return 'ok'");
    await page.waitForSelector(".pmenu", { timeout: 3000 }).catch(() => {});
    const box = await page.evaluate(() => {
      const r = document.querySelector(".pmenu")?.getBoundingClientRect();
      return r ? { x: r.left + r.width / 2, y: r.top + r.height / 2 } : null;
    });
    let over = null;
    if (box) {
      await page.mouse.move(box.x, box.y);
      over = await page.evaluate(() => document.getElementById("grid").style.cursor);
    }
    check("…and the arrow over a popup", box !== null && over === "", `box = ${JSON.stringify(box)}, cursor = ${JSON.stringify(over)}`);

    // The wheel scrolls the view and leaves the caret where it was: off screen it
    // isn't drawn, and typing goes in at it and brings the view back.
    await luaStr(page, "local t = {} for i = 1, 300 do t[i] = 'row' .. i end " +
      "btv.buf.set_lines(0, 0, -1, false, t) btv.win.set_cursor(0, 1, 0) return 'ok'");
    await sleep(200);
    const g = await page.evaluate(() => {
      const r = document.getElementById("grid").getBoundingClientRect();
      return { x: r.left + 200, y: r.top + 100 };
    });
    await page.mouse.move(g.x, g.y);
    for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 300); await sleep(30); }
    await sleep(300);
    const away = await page.evaluate(() => ({
      cursor: !!document.getElementById("btv-cursor"),
      top: window.__bemtvi.frame().windows.find((w) => w.focused)?.numbers?.[0],
    }));
    check("the wheel scrolls past the caret, which isn't drawn off screen",
      away.cursor === false && away.top > 20, JSON.stringify(away));
    await page.keyboard.type("Z");
    const first = await luaStr(page, "return btv.buf.lines(0, 0, 1, false)[1]");
    await sleep(200);
    const shown = await page.evaluate(() => !!document.getElementById("btv-cursor"));
    check("typing goes in at the caret and brings it back into view",
      first === "Zrow1" && shown, JSON.stringify({ first, shown }));

    // An input method composing while the caret is scrolled away brings it back
    // first, so the composition shows where it will go in.
    await page.mouse.move(g.x, g.y);
    for (let i = 0; i < 10; i++) { await page.mouse.wheel(0, 300); await sleep(30); }
    await sleep(300);
    const hidden = await page.evaluate(() => !document.getElementById("btv-cursor"));
    await page.evaluate(() => (document.getElementById("ime") || document.getElementById("kbd")).focus());
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Input.imeSetComposition", { text: "ぺ", selectionStart: 1, selectionEnd: 1 });
    let revealed = false;
    for (let i = 0; i < 40 && !revealed; i++) {
      await sleep(50);
      revealed = await page.evaluate(() => !!document.getElementById("btv-cursor"));
    }
    await cdp.send("Input.insertText", { text: "日" });
    await sleep(300);
    const composed = await luaStr(page, "return btv.buf.lines(0, 0, 1, false)[1]");
    check("composing while scrolled away brings the caret back, and commits there",
      hidden && revealed && composed === "Z日row1", JSON.stringify({ hidden, revealed, composed }));
    await page.evaluate(() => document.getElementById("kbd").focus());

    await luaStr(page, "btv.o.modeless = false return 'ok'");
    await sleep(200);
    c = await caret();
    check("vim mode keeps vim's cursor", c !== null && !c.cls.includes("caret"), JSON.stringify(c));
    await page.close();
  }

  // ── Mac: the Command layout ───────────────────────────────────────────────────
  {
    const page = await open(browser, { mac: true });
    const platform = await luaStr(page, "return btv.ui.caps().platform");
    check("the page reports macos", platform === "macos", `platform = ${JSON.stringify(platform)}`);

    await page.keyboard.type("foo bar");
    await waitText(page, "foo bar");
    await page.keyboard.press("Meta+z");
    check("Command+Z undoes", (await waitText(page, "foo")) === "foo",
      `text = ${JSON.stringify(await text(page))}`);
    await page.keyboard.press("Meta+Shift+Z");
    check("Command+Shift+Z redoes", (await waitText(page, "foo bar")) === "foo bar",
      `text = ${JSON.stringify(await text(page))}`);

    await page.keyboard.press("Meta+ArrowLeft");
    await page.keyboard.type("[");
    check("Command+Left goes to the line's start",
      (await waitText(page, "[foo bar")) === "[foo bar",
      `text = ${JSON.stringify(await text(page))}`);

    await page.keyboard.press("Meta+a");
    await page.keyboard.type("Y");
    check("Command+A selects all and typing replaces it", (await waitText(page, "Y")) === "Y",
      `text = ${JSON.stringify(await text(page))}`);

    // A Command chord outside the layout stays with the browser, so the editor never
    // types its letter.
    await page.keyboard.press("Meta+e");
    await page.keyboard.type("!");
    check("a Command chord the layout doesn't bind never reaches the editor",
      (await waitText(page, "Y!")) === "Y!",
      `text = ${JSON.stringify(await text(page))}`);
    await page.close();
  }

  await browser.close();
} catch (e) {
  console.log("FAIL  harness error:", e.message);
  failures++;
} finally {
  cleanup();
}

process.exit(failures === 0 ? 0 : 1);
