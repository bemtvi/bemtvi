// Playwright verifier for the PICKER INCLUDE/EXCLUDE FILTER BOXES on the pure web
// client (serverless).
//
// The boxes are VSCode's "files to include" / "files to exclude": `<C-g>` reveals two
// rows between the prompt and the separator, and collapsed they leave a badge on the
// prompt row. The web client paints them from the `menu.filters` redraw submap, and is
// the third of three renderers (TUI / GUI / web) that must agree on the ROW BUDGET —
// the server sizes the fixed box with two extra rows when they are revealed, so a web
// client that ignored them would overflow the box by two rows and push the list out.
//
// This asserts, against the real rendered DOM:
//   1. collapsed, an active filter shows its badge on the prompt row and NO extra rows;
//   2. `<C-g>` reveals labelled `include` / `exclude` rows holding the real text;
//   3. the two rows come out of the FIXED box's own height — it does not grow;
//   4. the caret follows the focused box, not the query;
//   5. the filter actually filters — the excluded path is gone from the list.
//
// Faithfulness (not a no-op): the picker is opened through the real `nx.picker` API over
// a real source, driven with real `feed()` keystrokes through the production tick, and
// every assertion reads the rendered `#grid` DOM rather than a mock.
//
// Prereqs: ../build.sh (../dist/eh.mjs + eh.wasm) and a Chromium for Playwright.
// Run:  node verify-picker-filters.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { globSync } from "node:fs";

const here = fileURLToPath(new URL(".", import.meta.url));
const PORT = 8163;

function chromiumPath() {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const home = process.env.HOME || "";
  const found = globSync(`${home}/.cache/ms-playwright/chromium-*/chrome-linux*/chrome`).sort();
  if (found.length) return found[found.length - 1];
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

// Every row of the picker box, plus where the caret is — read straight from the DOM.
const menuState = (page) =>
  page.evaluate(() => {
    const box = document.querySelector("#grid .pmenu");
    if (!box) return { open: false };
    const rows = [...box.children].map((el) => el.textContent.replace(/\s+$/, ""));
    const cur = document.getElementById("nx-cursor");
    const caretRow = cur ? [...box.children].findIndex((el) => el.contains(cur)) : -1;
    // Cells before the caret on its row — stop AT the cursor node; the padding that
    // follows it is not "before".
    let caretCol = -1;
    if (cur && caretRow >= 0) {
      caretCol = 0;
      for (const node of box.children[caretRow].childNodes) {
        if (node === cur || (node.contains && node.contains(cur))) break;
        caretCol += (node.textContent || "").length;
      }
    }
    return { open: true, rows, caretRow, caretCol };
  });

// The rows that are list results (not prompt / filter / separator chrome).
const listRows = (st) =>
  st.rows.filter(
    (r) => r && !r.startsWith(">") && !/^(include|exclude)(\s|$)/.test(r) && !/^─+$/.test(r),
  );

const srv = spawn(process.execPath, [`${here}serve.mjs`, String(PORT)], { stdio: "inherit" });
const cleanup = () => {
  try {
    srv.kill();
  } catch {}
};
process.on("exit", cleanup);

let browser;
try {
  for (let i = 0; i < 50; i++) {
    try {
      await fetch(`http://localhost:${PORT}/web/`);
      break;
    } catch {
      await sleep(100);
    }
  }

  browser = await chromium.launch({ executablePath: chromiumPath() });
  const page = await browser.newPage();
  page.on("console", (m) => {
    if (m.type() === "error") console.log("  [page error]", m.text());
  });
  page.on("pageerror", (e) => console.log("  [pageerror]", e.message));

  await page.goto(`http://localhost:${PORT}/web/`);
  await page.waitForFunction(() => window.__nxvim !== undefined, null, { timeout: 15000 });
  await page.evaluate(() => window.__nxvim.ready);
  check("Worker booted (serverless)", true);

  // A filterable source over fixed paths — no process spawn, so this works on the
  // pure web client where there is no shell at all.
  await page.evaluate(() =>
    window.__nxvim.execLua(`
      nx.picker.source {
        name = "webpaths",
        filter = true,
        items = function(ctx)
          for _, p in ipairs({ "src/main.rs", "src/net.rs", "target/junk.rs", "README.md" }) do
            ctx.push { text = p, path = p }
          end
        end,
        confirm = function(item) end,
      }
    `),
  );

  // ── 1. Collapsed: a badge on the prompt row, and no extra rows.
  await page.evaluate(() =>
    window.__nxvim.execLua(`nx.picker.open('webpaths', { exclude = 'target' })`),
  );
  await sleep(400);
  const collapsed = await menuState(page);
  check("picker opened with a filter", collapsed.open, JSON.stringify(collapsed));
  check(
    "collapsed: the badge rides the prompt row",
    /\[-1\]\s*$/.test(String(collapsed.rows[0])),
    JSON.stringify(collapsed.rows[0]),
  );
  check(
    "collapsed: no include/exclude rows are drawn",
    !collapsed.rows.some((r) => /^(include|exclude)\s/.test(r)),
    JSON.stringify(collapsed.rows),
  );
  check(
    "the filter is applied — the excluded path is gone",
    listRows(collapsed).some((r) => r.includes("src/main.rs")) &&
      !listRows(collapsed).some((r) => r.includes("target/")),
    JSON.stringify(listRows(collapsed)),
  );
  const collapsedListCount = listRows(collapsed).length;

  // ── 2/3. Revealed: two labelled rows, paid for out of the list.
  await page.evaluate(() => window.__nxvim.feed("<C-g>"));
  await sleep(300);
  const revealed = await menuState(page);
  check(
    "revealed: an `include` row is drawn",
    revealed.rows.some((r) => /^include(\s|$)/.test(r)),
    JSON.stringify(revealed.rows),
  );
  check(
    "revealed: an `exclude` row holds the real text",
    revealed.rows.some((r) => /^exclude\s+target/.test(r)),
    JSON.stringify(revealed.rows),
  );
  check(
    "revealed: the badge is gone (the rows say it now)",
    !/\[-1\]/.test(String(revealed.rows[0])),
    JSON.stringify(revealed.rows[0]),
  );
  // The box is a FIXED size, so the two revealed rows come out of its own height —
  // it must not grow. (That they specifically displace LIST rows is pinned exactly by
  // the Rust geometry test, which uses more results than the box can show; here the
  // result set is short, so the rows come out of the box's blank padding.)
  check(
    "revealed: the box did not grow to fit the two rows",
    revealed.rows.length === collapsed.rows.length,
    `collapsed box=${collapsed.rows.length} list=${collapsedListCount}; ` +
      `revealed box=${revealed.rows.length} list=${listRows(revealed).length}`,
  );

  // ── 4. The caret follows the focused box (<C-g> moved focus to `include`).
  const includeRow = revealed.rows.findIndex((r) => /^include(\s|$)/.test(r));
  check(
    "the caret sits on the focused include row, past its label",
    revealed.caretRow === includeRow && revealed.caretCol === 8,
    JSON.stringify({ caretRow: revealed.caretRow, caretCol: revealed.caretCol, includeRow }),
  );

  // ── 5. Typing into the box re-runs the source against the new pattern.
  await page.evaluate(() => window.__nxvim.feed("src"));
  await sleep(500);
  const typed = await menuState(page);
  check(
    "typing into the include box filters the list",
    typed.rows.some((r) => /^include\s+src/.test(r)) &&
      listRows(typed).length > 0 &&
      listRows(typed).every((r) => r.includes("src/")),
    JSON.stringify({ rows: typed.rows, list: listRows(typed) }),
  );

  await page.evaluate(() => window.__nxvim.feed("<Esc>"));
} catch (e) {
  check("harness ran without throwing", false, String((e && e.stack) || e));
} finally {
  if (browser) await browser.close();
  cleanup();
}

console.log(
  failures === 0
    ? "\nALL PASS — the web client paints the filter boxes and honors the shared row budget"
    : `\n${failures} FAILED`,
);
process.exit(failures === 0 ? 0 : 1);
