// Playwright verifier for the python demo's first-boot OPFS seeding + the pre-bundled python
// tree-sitter grammar (Phase 7 of docs/plans/2026-06-23-web-python-demo.md). Runs against the
// assembled demo site (build-demo.sh → demo-site/), where build-config flips `demoSeed: true`
// so the Worker seeds web/demo-seed/ (project + tour + init.lua) into OPFS on first boot.
//
// From a CLEARED OPFS (a genuine first boot) it asserts:
//   - the project + tour + config are seeded into OPFS (read straight back from storage);
//   - the guided tour opens as the startup buffer (init.lua's `edit /TOUR.md`);
//   - main.py highlights with NO :TSInstall — the python grammar is in the offline bundle
//     (colored spans appear, like a bundled grammar should);
//   - seeding is ONE-TIME: an edit survives a reload (the sentinel suppresses re-seed), so a
//     user's changes are never clobbered.
//
// Prereqs: ./build-demo.sh (assembles demo-site/) and a Chromium for Playwright. Run:
//   node verify-demo-seed.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8124;
const DEMO_SITE = `${here}../demo-site`;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`).sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

// Recursively wipe OPFS so the next boot is a genuine first boot (re-seeds from scratch).
async function clearOpfs(page) {
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    for await (const [name] of root.entries()) {
      try { await root.removeEntry(name, { recursive: true }); } catch {}
    }
  });
}

async function readOpfs(page, path) {
  return page.evaluate(async (p) => {
    try {
      let dir = await navigator.storage.getDirectory();
      const parts = p.split("/").filter(Boolean);
      const name = parts.pop();
      for (const d of parts) dir = await dir.getDirectoryHandle(d);
      const fh = await dir.getFileHandle(name);
      return await (await fh.getFile()).text();
    } catch (e) { return null; }
  }, path);
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], {
  stdio: "inherit",
  env: { ...process.env, NXVIM_SERVE_ROOT: DEMO_SITE },
});
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));
  const seedErrors = [];
  page.on("console", (m) => { const t = m.text(); if (/demo seed|config_error/i.test(t)) seedErrors.push(t); });

  // Boot once to get an OPFS scope, wipe it, then reload → a genuine first boot that re-seeds.
  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);
  await clearOpfs(page);
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);

  // 1. The project + tour + config were seeded into OPFS.
  const main = await readOpfs(page, "/main.py");
  const geom = await readOpfs(page, "/geometry.py");
  const tour = await readOpfs(page, "/TOUR.md");
  const init = await readOpfs(page, "/init.lua");
  check("seed: the demo project is present in OPFS after first boot",
    !!main && /from geometry import/.test(main) && !!geom && /class Circle/.test(geom),
    `main=${JSON.stringify(main?.slice(0, 40))} geom=${JSON.stringify(geom?.slice(0, 40))}`);
  check("seed: the tour + init.lua are present in OPFS",
    !!tour && /Welcome to nxvim/.test(tour) && !!init && /catppuccin/.test(init),
    `tour=${JSON.stringify(tour?.slice(0, 30))}`);

  // 2. The guided tour opened as the startup buffer (init.lua's `edit /TOUR.md`; the content
  //    loads from OPFS a tick later, so poll).
  let tourOpen = false;
  for (let i = 0; i < 60 && !tourOpen; i++) {
    if (/Welcome to nxvim/.test(await page.evaluate(() => window.__nxvim.lines()))) tourOpen = true;
    else await sleep(100);
  }
  check("seed: the guided tour opens as the startup buffer", tourOpen,
    `lines=${JSON.stringify((await page.evaluate(() => window.__nxvim.lines())).slice(0, 60))}`);

  // 3. main.py highlights with no :TSInstall — the python grammar is in the offline bundle.
  await page.evaluate(() => window.__nxvim.feed(":edit /main.py<CR>"));
  let styled = 0;
  for (let i = 0; i < 60; i++) {
    styled = await page.evaluate(() =>
      [...document.querySelectorAll("#grid .win .row span[style]")]
        .filter((s) => /color\s*:/.test(s.getAttribute("style"))).length);
    if (styled > 0) break;
    await sleep(100);
  }
  check("grammar: main.py highlights offline (python grammar pre-bundled, no :TSInstall)",
    styled > 0, `colored spans=${styled}`);

  // 4. Seeding is one-time: edit init.lua, reload, the edit survives (sentinel suppresses re-seed).
  await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle("main.py", { create: true });
    const w = await fh.createWritable();
    await w.write("# user edit\n");
    await w.close();
  });
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 20000 });
  await page.evaluate(() => window.__nxvim.ready);
  const afterReload = await readOpfs(page, "/main.py");
  check("seed: one-time — a user edit survives reload (sentinel suppresses re-seed)",
    afterReload === "# user edit\n", `main.py=${JSON.stringify(afterReload?.slice(0, 40))}`);

  if (seedErrors.length) console.log("  seed/config console output:\n   " + seedErrors.join("\n   "));

  await clearOpfs(page); // leave OPFS clean for re-runs
  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — first boot seeds the demo project + tour + init.lua into OPFS, the tour opens, python highlights offline, and seeding is one-time"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
