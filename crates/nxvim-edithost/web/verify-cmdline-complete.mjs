// Verifier for command-line completion (`:`+<Tab> — nx.cmdline_complete) auto-enabling
// on the pure serverless web build. The native binary turns the engine on by default via
// `cmdline_complete_default`; the wasm edit-host had no equivalent, so `:`+<Tab> did
// nothing. `eh_new` now calls `host.enable_cmdline_complete()` before init.lua, so typing
// `:e` then <Tab> must pop the wildmenu with `edit` as the leading candidate — WITHOUT
// any user config calling `nx.cmdline_complete.setup{}`.
//
//   node verify-cmdline-complete.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8161;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`),
  ].sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Type `:e` then <Tab> — NO user config enabling the engine. With the fix, the engine
  // is on by default and the wildmenu opens with `edit` ranked first.
  await page.evaluate(() => window.__nxvim.feed(":e"));
  await sleep(80);
  await page.evaluate(() => window.__nxvim.feed("<Tab>"));
  await sleep(150);

  // The engine's output rides the redraw frame's unified `menu` widget (the `pmenu`
  // key was retired in Phase 4-C). The cmdline wildmenu is flagged `cmdline: true`.
  const frame = await page.evaluate(() => {
    const m = (window.__nxvim.frame() || {}).menu || null;
    return {
      hasMenu: !!m,
      isCmdline: !!(m && m.cmdline),
      items: m ? m.items.slice() : [],
      domMenu: !!document.querySelector("#grid .pmenu, #grid .popup-chrome, #grid .menu"),
    };
  });
  check("`:e`+<Tab> opens the wildmenu (engine auto-enabled)",
    frame.hasMenu && frame.isCmdline, JSON.stringify({ hasMenu: frame.hasMenu, isCmdline: frame.isCmdline }));
  check("wildmenu offers `edit` as the leading candidate", frame.items[0] === "edit",
    JSON.stringify(frame.items.slice(0, 8)));
  check("wildmenu paints into the grid DOM", frame.domMenu, String(frame.domMenu));

  // A second <Tab> selects the first row (`edit`) and previews it into the command
  // line — so what <CR> would run is what the line shows.
  await page.evaluate(() => window.__nxvim.feed("<Tab>"));
  await sleep(120);
  const cmdline = await page.evaluate(() => window.__nxvim.cmdline());
  check("selecting the row previews the completion into the line (`:edit`)",
    /^:edit\b/.test(cmdline), JSON.stringify(cmdline));

  await browser.close();
} finally {
  cleanup();
}

process.exit(failures ? 1 : 0);
