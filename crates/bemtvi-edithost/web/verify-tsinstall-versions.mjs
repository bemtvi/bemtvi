// Playwright verifier for the browser `:TSInstall` paths that pick where a grammar comes
// from:
//   1. `:TSInstall markdown` fetches bemtvi's own committed parsers (markdown and the
//      markdown_inline it injects) from jsDelivr's GitHub mirror at the commit build.sh
//      stamped, caches them in OPFS, and highlights a markdown buffer;
//   2. `:TSInstall zig@latest` fetches the npm package at that dist-tag and remembers it;
//   3. `:TSUpdate zig` fetches the remembered version again;
//   4. a plain `:TSInstall zig` goes back to the pinned version and forgets it;
//   5. a git sha as a version, and a version on markdown, fail loud.
//
// Hermetic: jsDelivr is intercepted. /npm/<pkg>@<ver>/<sub> is served from the pinned
// `treesitter/node_modules/` whatever <ver> asks (the test checks the version in the URL,
// not the bytes); /gh/bemtvi/bemtvi@<sha>/… from this checkout's prebuilt/ files, only at
// the stamped sha; /gh/nvim-treesitter/… from the offline bundle's queries.
//
//   node verify-tsinstall-versions.mjs   (after ./build.sh)
import { chromium } from "playwright";
import { spawn, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, readFileSync } from "node:fs";
import { join } from "node:path";

const here = fileURLToPath(new URL(".", import.meta.url));
const EH = join(here, "..");
const TS_NM = join(EH, "treesitter", "node_modules");
const PORT = 8103;

const STAMPED = (readFileSync(join(here, "vendor", "build-info.js"), "utf8").match(/"([0-9a-f]{40})"/) || [])[1];
const EXPECTED = execFileSync("git", ["log", "-1", "--format=%H", "--",
  "treesitter/prebuilt/markdown.wasm", "treesitter/prebuilt/markdown_inline.wasm"], { cwd: EH, encoding: "utf8" }).trim();

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

// Feed a command and wait for its `:TSInstall` outcome on the message line.
async function install(page, cmd) {
  await page.evaluate((c) => window.__bemtvi.feed(c), cmd);
  let last = "";
  for (let i = 0; i < 100; i++) {
    last = await page.evaluate(() => window.__bemtvi.message());
    if (/^TSInstall: (installed |.* failed)/.test(last)) return last;
    await sleep(100);
  }
  return `(timed out; last message: ${last})`;
}

async function opfsText(page, parts) {
  return page.evaluate(async (parts) => {
    try {
      let dir = await navigator.storage.getDirectory();
      for (const p of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(p);
      const fh = await dir.getFileHandle(parts[parts.length - 1]);
      return await (await fh.getFile()).text();
    } catch { return null; }
  }, parts);
}

async function colored(page) {
  for (let i = 0; i < 60; i++) {
    const n = await page.evaluate(() =>
      [...document.querySelectorAll("#grid .win .row span[style]")].filter((s) => /color\s*:/.test(s.getAttribute("style"))).length);
    if (n > 0) return n;
    await sleep(100);
  }
  return 0;
}

check("build.sh stamped the last commit that changed the prebuilt parsers", STAMPED === EXPECTED,
  `stamped ${STAMPED}, git says ${EXPECTED}`);

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

  let npm = []; // `<pkg>@<ver>/<sub>` requested
  let npmDown = false; // simulate the CDN failing
  let gh = []; // `/gh/…` paths requested
  await page.route("**/cdn.jsdelivr.net/npm/**", async (route) => {
    const rest = new URL(route.request().url()).pathname.replace(/^\/npm\//, "");
    const m = rest.match(/^(@[^/]+\/[^/@]+|[^/@]+)@([^/]+)\/(.+)$/);
    if (!m) return route.fulfill({ status: 404, body: "no pkg@ver match" });
    const [, pkg, ver, sub] = m;
    npm.push(`${pkg}@${ver}/${sub}`);
    if (npmDown) return route.fulfill({ status: 503, body: "CDN down (test)" });
    try {
      const body = readFileSync(join(TS_NM, pkg, sub));
      await route.fulfill({ status: 200, contentType: sub.endsWith(".wasm") ? "application/wasm" : "text/plain", body });
    } catch {
      await route.fulfill({ status: 404, body: `not vendored: ${pkg}/${sub}` });
    }
  });
  await page.route("**/cdn.jsdelivr.net/gh/**", async (route) => {
    const path = new URL(route.request().url()).pathname.replace(/^\/gh\//, "");
    gh.push(path);
    const own = path.match(/^bemtvi\/bemtvi@([0-9a-f]{40})\/crates\/bemtvi-edithost\/(treesitter\/prebuilt\/[^/]+\.wasm)$/);
    if (own) {
      if (own[1] !== EXPECTED) return route.fulfill({ status: 404, body: "not at that commit" });
      return route.fulfill({ status: 200, contentType: "application/wasm", body: readFileSync(join(EH, own[2])) });
    }
    const q = path.match(/^nvim-treesitter\/nvim-treesitter@[^/]+\/runtime\/queries\/([^/]+)\/(highlights|injections)\.scm$/);
    if (q) {
      const file = q[2] === "highlights" ? join(here, "vendor", "queries", `${q[1]}.scm`) : join(here, "vendor", "injections", `${q[1]}.scm`);
      try { return route.fulfill({ status: 200, contentType: "text/plain", body: readFileSync(file) }); } catch {}
    }
    return route.fulfill({ status: 404, body: "not served in this test" });
  });

  await page.goto(`http://localhost:${PORT}/web/index.html`);
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await page.evaluate(async () => {
    try {
      const btv = await (await navigator.storage.getDirectory()).getDirectoryHandle(".bemtvi");
      await btv.removeEntry("treesitter", { recursive: true });
    } catch {}
  });
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);

  // ---- 1. markdown from bemtvi's own repository ----
  let msg = await install(page, ":TSInstall markdown<CR>");
  check("markdown: installed", msg.includes("installed markdown") && msg.includes(`bemtvi@${EXPECTED.slice(0, 9)}`), msg);
  const prebuiltGets = gh.filter((p) => p.startsWith(`bemtvi/bemtvi@${EXPECTED}/`)).map((p) => p.split("/").pop());
  check("markdown: both parsers fetched at the stamped commit",
    prebuiltGets.includes("markdown.wasm") && prebuiltGets.includes("markdown_inline.wasm"), JSON.stringify(gh));
  const manifest = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "manifest.json"])) || "[]");
  check("markdown: both cached in OPFS", manifest.includes("markdown") && manifest.includes("markdown_inline"), JSON.stringify(manifest));
  await page.evaluate(() => window.__bemtvi.feed(":e notes.md<CR>"));
  await page.evaluate(() => window.__bemtvi.feed("ggdGi# Title<CR><CR>some *emph* and `code`<Esc>"));
  const mdColored = await colored(page);
  check("markdown: a markdown buffer highlights after the install (the OPFS copy loads first)", mdColored > 0,
    JSON.stringify(await page.evaluate(() => ({
      msg: window.__bemtvi.message(),
      rows: [...document.querySelectorAll("#grid .win .row")].slice(0, 4).map((r) => r.textContent),
    }))));

  // ---- 2. zig@latest ----
  npm = [];
  msg = await install(page, ":TSInstall zig@latest<CR>");
  check("zig@latest: installed, following latest", msg.includes("installed zig") && msg.includes("following latest"), msg);
  check("zig@latest: the wasm came from the latest dist-tag",
    npm.some((p) => /tree-sitter-zig@latest\/.*\.wasm$/.test(p)), JSON.stringify(npm));
  let follows = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "follows.json"])) || "{}");
  check("zig@latest: remembered", follows.zig === "latest", JSON.stringify(follows));

  // :TSInstallInfo lists what it follows, now and after a reload (seeded from OPFS).
  const infoFollows = async () => {
    await page.evaluate(() => window.__bemtvi.feed(":TSInstallInfo<CR>"));
    for (let i = 0; i < 50; i++) {
      const ls = await page.evaluate(() => window.__bemtvi.lines());
      const text = Array.isArray(ls) ? ls.join("\n") : String(ls);
      if (/^zig\s+following latest/m.test(text)) return true;
      await sleep(100);
    }
    return JSON.stringify(await page.evaluate(() => window.__bemtvi.lines()));
  };
  let info = await infoFollows();
  check(":TSInstallInfo: zig following latest", info === true, info);
  await page.evaluate(() => window.__bemtvi.feed(":q<CR>"));
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  info = await infoFollows();
  check(":TSInstallInfo after a reload: still following latest", info === true, info);
  await page.evaluate(() => window.__bemtvi.feed(":q<CR>"));

  // ---- 3. :TSUpdate follows it ----
  npm = [];
  msg = await install(page, ":TSUpdate zig<CR>");
  check(":TSUpdate zig: installed at latest again", msg.includes("installed zig") && msg.includes("following latest"), msg);
  check(":TSUpdate zig: fetched the latest dist-tag", npm.some((p) => /tree-sitter-zig@latest\/.*\.wasm$/.test(p)), JSON.stringify(npm));

  // ---- 3b. a plain :TSInstall that fails keeps the followed version on record ----
  npmDown = true;
  msg = await install(page, ":TSInstall zig<CR>");
  npmDown = false;
  check("failed pinned install: reported", msg.includes("failed"), msg);
  follows = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "follows.json"])) || "{}");
  check("failed pinned install: still following latest (what OPFS holds)", follows.zig === "latest", JSON.stringify(follows));

  // ---- 4. plain :TSInstall goes back to the pinned version ----
  npm = [];
  msg = await install(page, ":TSInstall zig<CR>");
  check(":TSInstall zig: installed without following", msg.includes("installed zig") && !msg.includes("following"), msg);
  check(":TSInstall zig: fetched the pinned version",
    npm.some((p) => /tree-sitter-zig@\d[^/]*\/.*\.wasm$/.test(p)), JSON.stringify(npm));
  follows = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "follows.json"])) || "{}");
  check(":TSInstall zig: forgot the followed version", !("zig" in follows), JSON.stringify(follows));

  // ---- 5. refused refs ----
  msg = await install(page, ":TSInstall zig@deadbeef1<CR>");
  check("zig@<sha>: fails loud", msg.includes("failed") && msg.includes("git commit"), msg);
  msg = await install(page, ":TSInstall markdown@latest<CR>");
  check("markdown@latest: fails loud", msg.includes("failed") && msg.includes("no versions"), msg);

  // ---- 6. a damaged follows.json loses only the versions, not the listing ----
  await page.evaluate(async () => {
    const dir = await (await (await navigator.storage.getDirectory()).getDirectoryHandle(".bemtvi")).getDirectoryHandle("treesitter");
    const w = await (await dir.getFileHandle("follows.json", { create: true })).createWritable();
    await w.write("{ not json");
    await w.close();
  });
  await page.reload();
  await page.waitForFunction(() => window.__bemtvi !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__bemtvi.ready);
  await page.evaluate(() => window.__bemtvi.feed(":TSInstallInfo<CR>"));
  let listing = "";
  for (let i = 0; i < 50; i++) {
    const ls = await page.evaluate(() => window.__bemtvi.lines());
    listing = Array.isArray(ls) ? ls.join("\n") : String(ls);
    if (/^rust\b/m.test(listing)) break;
    await sleep(100);
  }
  check("damaged follows.json: :TSInstallInfo still lists the grammars", /^rust\b/m.test(listing) && /^zig\b/m.test(listing), listing);

  // ---- 7. two installs in one command both stay on record ----
  await page.evaluate(async () => {
    const dir = await (await (await navigator.storage.getDirectory()).getDirectoryHandle(".bemtvi")).getDirectoryHandle("treesitter");
    await dir.removeEntry("follows.json").catch(() => {});
  });
  await page.evaluate(() => window.__bemtvi.feed(":TSInstall zig@latest toml@latest<CR>"));
  let both = {};
  for (let i = 0; i < 150; i++) {
    both = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "follows.json"])) || "{}");
    if (both.zig && both.toml) break;
    await sleep(100);
  }
  await sleep(500); // let a late writer land, if there is one
  both = JSON.parse((await opfsText(page, [".bemtvi", "treesitter", "follows.json"])) || "{}");
  check("two installs at once: both remembered", both.zig === "latest" && both.toml === "latest", JSON.stringify(both));

  await browser.close();
} catch (e) {
  console.log("FAIL  (exception)", e);
  failures++;
}
cleanup();
console.log(failures === 0 ? "\nALL PASS — browser :TSInstall sources: bemtvi's markdown, npm versions, :TSUpdate" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
