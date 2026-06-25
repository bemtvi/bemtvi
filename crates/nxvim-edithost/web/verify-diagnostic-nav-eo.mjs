// Repro for "diagnostics don't enable right away on the web when I do :eo" (the
// examples/diagnostic-nav demo: `:luao` the config, then `:eo` to open sample.txt).
// The config seeds diagnostics from a BufReadPost/BufEnter autocmd via nx.diagnostic.set.
// Opening the file through the picker is a multi-tick async dance (the bound-path realfs
// read round-trips through the UI), so the question is whether the frame posted AFTER the
// file lands actually carries the diagnostics — or whether they only show after the next
// keystroke nudges a fresh redraw.
//
// This drives the REAL `:eo` keystroke path (stubbed OS chooser only) and asserts the
// gutter sign + squiggle reach the frame WITHOUT feeding any extra key after the open.
//
//   node verify-diagnostic-nav-eo.mjs   (needs a built dist/eh.mjs — run build.sh first)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync, readFileSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8098;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = [
    ...globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome`),
    ...globSync(`${home}/Library/Caches/ms-playwright/chromium-*/chrome-mac*/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`),
  ].sort();
  return found.length ? found[found.length - 1] : undefined;
}

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) { if (detail !== undefined) console.log(`        ${detail}`); failures++; }
}

const CONFIG = readFileSync(`${here}../../../examples/diagnostic-nav/init.lua`, "utf8");
const SAMPLE = readFileSync(`${here}../../../examples/diagnostic-nav/sample.txt`, "utf8");

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

  // Stub the OS chooser to return sample.txt (the autocmd pattern is `*sample.txt`).
  await page.addInitScript(({ name, body }) => {
    const handle = {
      name, kind: "file",
      async getFile() { return new File([body], name, { type: "text/plain" }); },
      async requestPermission() { return "granted"; },
      async queryPermission() { return "granted"; },
    };
    self.showOpenFilePicker = async () => [handle];
  }, { name: "sample.txt", body: SAMPLE });

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);

  // Load the demo config (the `:luao` step — registers the BufReadPost/BufEnter autocmd
  // and enables signs/underline/virtual_text). Also tap BufReadPost/BufNewFile so the test
  // asserts the ROOT cause: an existing file opened via :eo must fire BufReadPost (the
  // event the config keys diagnostics off), not BufNewFile.
  await page.evaluate((cfg) => window.__nxvim.execLua(cfg + `
    _G.__events = {}
    nx.autocmd.create({ "BufReadPost" }, { pattern = "*sample.txt",
      callback = function() _G.__events[#_G.__events + 1] = "BufReadPost" end })
    nx.autocmd.create({ "BufNewFile" }, { pattern = "*sample.txt",
      callback = function() _G.__events[#_G.__events + 1] = "BufNewFile" end })
  `), CONFIG);

  // Drive the REAL `:eo` keystrokes so the genuine intercept + bound-path realfs flow runs.
  await page.evaluate(() => document.getElementById("kbd").focus());
  await page.keyboard.type(":eo");
  for (let i = 0; i < 100; i++) {
    if ((await page.evaluate(() => window.__nxvim.cmdline())) === ":eo") break;
    await sleep(30);
  }
  await page.keyboard.press("Enter");

  // Wait for the file to land in the buffer — polling reads frames only, it feeds NO keys.
  const lines = await (async () => {
    for (let i = 0; i < 200; i++) {
      const v = await page.evaluate(() => window.__nxvim.lines());
      if (/prnit/.test(String(v))) return v;
      await sleep(40);
    }
    return await page.evaluate(() => window.__nxvim.lines());
  })();
  check(":eo opened sample.txt into the buffer", /prnit/.test(String(lines)), `lines=${JSON.stringify(lines)?.slice(0, 200)}`);

  // Give the loop a moment to settle any post-load frame, still WITHOUT feeding a key.
  await sleep(400);

  // ROOT CAUSE: the existing file must fire BufReadPost (not BufNewFile) so the config's
  // diagnostic autocmd runs.
  const events = await page.evaluate(() =>
    window.__nxvim.execLua("return table.concat(_G.__events, ',')"));
  const evStr = String(events?.result ?? events).replace(/^ok:String\(Utf8String \{ s: Ok\(|\)\s*\}\)$/g, "").replace(/^"|"$/g, "");
  check("ROOT: :eo on an existing file fires BufReadPost", /BufReadPost/.test(evStr), `events=${JSON.stringify(evStr)}`);
  check("ROOT: it does NOT mis-fire BufNewFile", !/BufNewFile/.test(evStr), `events=${JSON.stringify(evStr)}`);

  // THE BUG: with no extra keystroke, do the diagnostics show? Check the frame the renderer
  // is painting (the focused window) for the seeded Error sign + squiggle.
  const frame = await page.evaluate(() => {
    const w = (window.__nxvim.frame()?.windows || []).find((x) => x.focused) || {};
    return {
      sign_width: w.sign_width,
      signs: (w.diagnostics_signs || []).filter(Boolean),
      diags: (w.diagnostics || []).flat().filter(Boolean),
    };
  });
  check("NO-NUDGE: a sign column is reserved (sign_width > 0) after :eo",
    frame.sign_width > 0, `sign_width=${JSON.stringify(frame.sign_width)}`);
  check("NO-NUDGE: an Error sign reached the gutter payload after :eo",
    frame.signs.some((c) => Array.isArray(c) && c[1] === 1), `signs=${JSON.stringify(frame.signs)}`);

  // DOM: the gutter glyph actually painted, again with no extra keystroke.
  const gutterPainted = await page.evaluate(() =>
    [...document.querySelectorAll("#grid .win .row span[style]")]
      .some((s) => s.textContent.includes("E") && s.getAttribute("style").toLowerCase().includes("#e06c75")));
  check("NO-NUDGE: the Error gutter glyph painted in the DOM after :eo", gutterPainted);

  // Control: a harmless cursor nudge must NOT be what makes them appear. If the no-nudge
  // checks failed but this passes, that pinpoints the off-tick redraw race.
  if (failures > 0) {
    await page.evaluate(() => document.getElementById("kbd").focus());
    await page.keyboard.press("l");
    await page.keyboard.press("h");
    await sleep(200);
    const afterNudge = await page.evaluate(() =>
      [...document.querySelectorAll("#grid .win .row span[style]")]
        .some((s) => s.textContent.includes("E") && s.getAttribute("style").toLowerCase().includes("#e06c75")));
    console.log(`  [diagnostic] gutter painted AFTER a keystroke nudge: ${afterNudge}` +
      (afterNudge ? "  → confirms an off-tick redraw race (shows only after input)" : ""));
  }

  await browser.close();
} catch (e) {
  check("harness ran without throwing", false, String((e && e.stack) || e));
} finally {
  try { if (browser) await browser.close(); } catch {}
  cleanup();
}

console.log(failures === 0
  ? "\nALL PASS — :eo seeds + paints the diagnostics with no extra keystroke"
  : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
