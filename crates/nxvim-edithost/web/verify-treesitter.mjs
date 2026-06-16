// Playwright verifier for runtime `:TSInstall` (the browser grammar installer). Drives the
// real wasm edit-host in headless Chromium and asserts:
//   1. a BUNDLED grammar (rust) highlights offline, with no network;
//   2. a NON-bundled grammar (zig) installs at runtime — fetched from the CDN, sanitized,
//      cached in OPFS, and registered — and then highlights;
//   3. the install persists across a reload (highlights again from OPFS, with ZERO refetch);
//   4. the full standard query set is cached in OPFS (incl. indents.scm, consumed by the
//      worker indenter — see verify-treesitter-indent.mjs).
//
// Hermetic: the CDN (jsDelivr) is intercepted via page.route and served from the pinned
// `treesitter/node_modules/` tarballs — the same versions the registry declares — so the
// test never reaches the real network. Companion to verify-ui.mjs (renderer/highlighting).
//
//   node verify-treesitter.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, readFileSync } from "node:fs";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const TS_NM = join(here, "..", "treesitter", "node_modules");
const PORT = 8099;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Chromium.app/Contents/MacOS/Chromium`),
  ].sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

// Poll until `fn` (run in the page) reports colored spans, up to ~6s.
async function waitColored(page) {
  let detail = "";
  for (let i = 0; i < 60; i++) {
    const r = await page.evaluate(() => {
      const spans = [...document.querySelectorAll("#grid .win .row span[style]")];
      const styled = spans.filter((s) => /color\s*:/.test(s.getAttribute("style")));
      return { any: styled.length, sample: styled.slice(0, 4).map((s) => s.textContent) };
    });
    if (r.any > 0) return { ok: true, detail: JSON.stringify(r.sample) };
    detail = JSON.stringify(r);
    await sleep(100);
  }
  return { ok: false, detail };
}

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => { try { srv.kill(); } catch {} };
process.on("exit", cleanup);

try {
  for (let i = 0; i < 50; i++) {
    try { await fetch(`http://localhost:${PORT}/web/index.html`); break; } catch { await sleep(100); }
  }
  const browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  // Intercept the CDN: /npm/<pkg>@<version>/<sub> → treesitter/node_modules/<pkg>/<sub>.
  // Track which subpaths were served so we can prove an install fetched (and a reload did
  // NOT). Scoped packages (@tree-sitter-grammars/…) keep their leading segment.
  let cdnFetches = [];
  // Indents now come from nvim-treesitter (jsDelivr's /gh/ mirror). Block it in this test so
  // the install stays hermetic — zig ships its own queries/indents.scm, so highlight.js's
  // fallback caches that instead (the `indents.scm` OPFS-cache assertion below still holds).
  await page.route("**/cdn.jsdelivr.net/gh/**", (route) => route.fulfill({ status: 404, body: "gh mirror disabled in test" }));
  await page.route("**/cdn.jsdelivr.net/npm/**", async (route) => {
    const rest = new URL(route.request().url()).pathname.replace(/^\/npm\//, "");
    const m = rest.match(/^(@[^/]+\/[^/@]+|[^/@]+)@[^/]+\/(.+)$/);
    if (!m) return route.fulfill({ status: 404, body: "no pkg@ver match" });
    const [, pkg, sub] = m;
    try {
      const body = readFileSync(join(TS_NM, pkg, sub));
      cdnFetches.push(`${pkg}/${sub}`);
      await route.fulfill({ status: 200, contentType: sub.endsWith(".wasm") ? "application/wasm" : "text/plain", body });
    } catch {
      await route.fulfill({ status: 404, body: `not vendored: ${pkg}/${sub}` });
    }
  });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Start from a clean install cache so `:TSInstall zig` genuinely fetches (a prior run may
  // have cached it). Clear OPFS's treesitter dir, then reload so the worker re-seeds clean.
  await page.evaluate(async () => {
    try {
      const nx = await (await navigator.storage.getDirectory()).getDirectoryHandle(".nxvim");
      await nx.removeEntry("treesitter", { recursive: true });
    } catch {}
  });
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // ---- 1. A bundled grammar (rust) highlights offline ----
  await page.evaluate(() => window.__nxvim.feed(":e demo.rs<CR>"));
  await page.evaluate(() => window.__nxvim.feed("ggdGifn main() {}<Esc>"));
  const rust = await waitColored(page);
  check("bundled: rust highlights offline (no install)", rust.ok, rust.detail);
  const rustCdn = cdnFetches.length;
  check("bundled: rust used no CDN fetch", rustCdn === 0, JSON.stringify(cdnFetches));

  // ---- 2. A non-bundled grammar (zig) installs at runtime from the CDN ----
  cdnFetches = [];
  await page.evaluate(() => window.__nxvim.feed(":e demo.zig<CR>"));
  await page.evaluate(() => window.__nxvim.feed('ggdGipub fn main() !void {}<Esc>'));
  // Not installed yet → plain (no colored spans).
  const beforeInstall = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")].filter((s) => /color\s*:/.test(s.getAttribute("style"))).length);
  check("uninstalled: zig renders plain before :TSInstall", beforeInstall === 0, String(beforeInstall));

  await page.evaluate(() => window.__nxvim.feed(":TSInstall zig<CR>"));
  const zig = await waitColored(page);
  check("install: zig highlights after :TSInstall (CDN → register)", zig.ok, zig.detail);

  const fetchedWasm = cdnFetches.some((p) => p.endsWith("tree-sitter-zig.wasm"));
  const fetchedHl = cdnFetches.some((p) => p.endsWith("queries/highlights.scm"));
  check("install: fetched zig grammar + highlights from the CDN", fetchedWasm && fetchedHl, JSON.stringify(cdnFetches));

  // The status line echoes the (honest) outcome.
  let echo = "";
  for (let i = 0; i < 30; i++) {
    echo = await page.evaluate(() => window.__nxvim.message());
    if (/installed zig/i.test(echo)) break;
    await sleep(100);
  }
  check("install: status echoes 'installed zig'", /installed zig/i.test(echo), echo);

  // ---- 4. The full standard query set is cached in OPFS (indents → worker indenter) ----
  const cached = await page.evaluate(async () => {
    const out = {};
    try {
      const dir = await (await (await navigator.storage.getDirectory())
        .getDirectoryHandle(".nxvim")).getDirectoryHandle("treesitter");
      const zig = await dir.getDirectoryHandle("zig");
      for (const name of ["parser.wasm", "highlights.scm", "indents.scm", "injections.scm"]) {
        try { await zig.getFileHandle(name); out[name] = true; } catch { out[name] = false; }
      }
    } catch (e) { out.error = String(e); }
    return out;
  });
  check("cache: zig parser + full query set persisted to OPFS",
    cached["parser.wasm"] && cached["highlights.scm"] && cached["indents.scm"] && cached["injections.scm"],
    JSON.stringify(cached));

  // ---- 3. Install survives a reload — highlights from OPFS, zero refetch ----
  cdnFetches = [];
  await page.reload();
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  await page.evaluate(() => window.__nxvim.feed(":e demo.zig<CR>"));
  await page.evaluate(() => window.__nxvim.feed('ggdGipub fn main() !void {}<Esc>'));
  const zig2 = await waitColored(page);
  check("persist: zig highlights after reload (from OPFS cache)", zig2.ok, zig2.detail);
  check("persist: reload re-fetched nothing from the CDN", cdnFetches.length === 0, JSON.stringify(cdnFetches));

  // ---- 5. Re-`:TSInstall` repairs a MISSING indents.scm (regression) ----
  // The offline bundle ships grammars without indents, and older installs predate the full
  // query set, so an "available" grammar can still be missing its indents.scm. A second
  // `:TSInstall` must then FETCH the missing query — not short-circuit "already installed"
  // and no-op. Simulate the gap by deleting zig's cached indents.scm, then re-install.
  const delIndents = async () => page.evaluate(async () => {
    const dir = await (await (await navigator.storage.getDirectory())
      .getDirectoryHandle(".nxvim")).getDirectoryHandle("treesitter");
    await (await dir.getDirectoryHandle("zig")).removeEntry("indents.scm");
  });
  const hasIndents = async () => page.evaluate(async () => {
    try {
      const dir = await (await (await navigator.storage.getDirectory())
        .getDirectoryHandle(".nxvim")).getDirectoryHandle("treesitter");
      await (await dir.getDirectoryHandle("zig")).getFileHandle("indents.scm");
      return true;
    } catch { return false; }
  });
  await delIndents();
  check("repair: indents.scm absent after deletion (precondition)", !(await hasIndents()));

  cdnFetches = [];
  await page.evaluate(() => window.__nxvim.feed(":TSInstall zig<CR>"));
  let repaired = false;
  for (let i = 0; i < 40; i++) { if ((repaired = await hasIndents())) break; await sleep(100); }
  check("repair: re-:TSInstall re-cached the missing indents.scm in OPFS", repaired);
  check("repair: indents.scm was actually re-fetched (not a silent no-op)",
    cdnFetches.some((p) => /indents\.scm$/.test(p)), JSON.stringify(cdnFetches));

  // ---- 6. Another registry language installs + highlights (toml) ----
  // Guards toml's REGISTRY entry (package / version / wasm subpath) end to end: a typo
  // there fails silently at runtime, not at build. toml ships no indents.scm (the GH
  // mirror is blocked in this test), so this asserts highlighting only.
  cdnFetches = [];
  await page.evaluate(() => window.__nxvim.feed(":e demo.toml<CR>"));
  await page.evaluate(() => window.__nxvim.feed('ggdGititle = "nxvim"<CR>[package]<CR>version = "0.1.0"<Esc>'));
  const tomlBefore = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")].filter((s) => /color\s*:/.test(s.getAttribute("style"))).length);
  check("uninstalled: toml renders plain before :TSInstall", tomlBefore === 0, String(tomlBefore));

  await page.evaluate(() => window.__nxvim.feed(":TSInstall toml<CR>"));
  const toml = await waitColored(page);
  check("install: toml highlights after :TSInstall (CDN → register)", toml.ok, toml.detail);
  check("install: fetched toml grammar from the CDN",
    cdnFetches.some((p) => p.endsWith("tree-sitter-toml.wasm")), JSON.stringify(cdnFetches));

  await browser.close();
} finally {
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — runtime :TSInstall: bundled offline + CDN install + OPFS cache + reload-persistence"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
