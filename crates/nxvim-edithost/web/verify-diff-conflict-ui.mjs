// End-to-end regression for two nxvim-diff-on-web bugs, driving the REAL plugin bundle
// via the demo site exactly as a user would (open shapes.py → :NxDiffConflict):
//   * gutter signs (signs=true) render on a changed row once a hunk is in view; and
//   * the keys-helper / which-key popup (an `editor_relative` content float) stays
//     ON-SCREEN when a RIGHT-side diff pane is focused — the bug anchored it to the
//     focused window, pushing it off the right edge so it was invisible.
//
// Prereqs: ./build-demo.sh (assembles ../demo-site) and a Chromium for Playwright.
//   node verify-diff-conflict-ui.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, readFileSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8126;
const DEMO_SITE = `${here}../demo-site`;
const DEMO_INIT = readFileSync(`${here}demo-seed/init.lua`, "utf8");
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
async function writeOpfs(page, name, text) {
  await page.evaluate(async ({ name, text }) => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(name, { create: true });
    const w = await fh.createWritable(); await w.write(text); await w.close();
  }, { name, text });
}
const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit", env: { ...process.env, NXVIM_SERVE_ROOT: DEMO_SITE } });
process.on("exit", () => { try { srv.kill(); } catch {} });
try {
  for (let i = 0; i < 50; i++) { try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); } }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);
  // The demo init.lua sets `nxvim-diff` `signs = true`, so a fresh conflict view shows
  // the per-hunk gutter signs — no need to override here.
  await writeOpfs(page, "init.lua", DEMO_INIT);
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);
  const lua = (c) => page.evaluate((x) => window.__nxvim.execLua(x).then((r) => r.result), c);

  const loaded = await lua(`return tostring(package.loaded["nxvim-diff"] ~= nil and package.loaded["nxvim-keys-helper"] ~= nil)`);
  check("demo: nxvim-diff + keys-helper loaded", /true/.test(String(loaded)), `loaded=${JSON.stringify(loaded)}`);

  // The user's flow: open the seeded conflict file, open the 3-way, jump to a hunk.
  await page.evaluate(() => window.__nxvim.feed(":e /shapes.py<CR>"));
  await sleep(800);
  await page.evaluate(() => window.__nxvim.feed(":NxDiffConflict<CR>"));
  await sleep(1500);
  await page.evaluate(() => window.__nxvim.feed("]czz"));
  await sleep(500);

  // (1) Signs reach the frame: with a hunk in view, at least one diff pane carries a
  // `+`/`~`/`-` hunk sign in its `diagnostics_signs` (the gutter-render correctness for
  // signs is covered separately by verify-sign-gutter-no-number.mjs / verify-view-sign.mjs).
  const signs = await page.evaluate(() => {
    const diffPanes = (window.__nxvim.frame()?.windows || [])
      .filter((w) => ["HEAD", "base", "feature/triangle-area"].includes(w.file_name));
    const withSign = diffPanes.filter((w) => (w.diagnostics_signs || []).some((s) => Array.isArray(s) && /[~+\-]/.test(s[0])));
    return { panes: diffPanes.length, withSign: withSign.map((w) => w.file_name) };
  });
  check("issue 2 — diff panes carry hunk signs in the redraw frame once a hunk is in view",
    signs.withSign.length > 0, `signs=${JSON.stringify(signs)}`);

  // (2) keys-helper: with the RIGHTMOST pane focused (`]c` left focus there), press `c`
  // and assert the popup float renders fully within the viewport (not off the right edge).
  await page.evaluate(() => window.__nxvim.feed("c"));
  await sleep(700);
  const kh = await page.evaluate(() => {
    const f = window.__nxvim.frame();
    const box = [...document.querySelectorAll("#grid .pmenu")].map((e) => e.getBoundingClientRect())
      .sort((a, b) => b.width - a.width)[0];
    const text = box ? [...document.querySelectorAll("#grid .pmenu")].map((e) => e.textContent).join(" ") : "";
    return {
      editor_relative: f?.float?.editor_relative,
      x: box ? Math.round(box.x) : null, right: box ? Math.round(box.right) : null,
      vpW: window.innerWidth, hasMaps: /choose_ours|choose_both|choose|nxvim-diff/.test(text),
    };
  });
  console.log("  keys-helper:", JSON.stringify(kh));
  check("issue 3 — keys-helper float carries the diff maps",
    kh.hasMaps, `kh=${JSON.stringify(kh)}`);
  check("issue 3 — keys-helper float renders within the viewport (was off-screen right)",
    kh.x != null && kh.x >= 0 && kh.right <= kh.vpW + 1, `kh=${JSON.stringify(kh)}`);
  await page.evaluate(() => window.__nxvim.feed("<Esc>"));

  await browser.close();
} finally { srv.kill(); }
console.log(failures === 0 ? "\nALL PASS — diff signs render + keys-helper stays on-screen on web"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
