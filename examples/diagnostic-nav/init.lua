-- ~~~ bemtvi built-in diagnostic navigation: `]d`/`[d`, `]e`/`[e`, `<C-w>d` ~~~
--
-- These keymaps ship in bemtvi's CORE, exactly as in upstream neovim — you do NOT
-- bind them, they are always on:
--
--   ]d / [d   jump to the next / previous diagnostic (any severity)
--   ]e / [e   jump to the next / previous ERROR (severity = ERROR only)
--   <C-w>d    show the diagnostics on the cursor's LINE in full, in a read-only
--             listing (<C-w><C-d> is the same). Line-scoped, so you need not be
--             sitting on the flagged span itself.
--
-- `]d`/`[d` and `]e`/`[e` are prelude default keymaps over `btv.diagnostic.goto_*`;
-- `<C-w>d` rides the native `<C-w>` window grammar. Being defaults, any of them can
-- be overridden (`btv.keymap.set("n", "]d", ...)`) or disabled (map it to an empty
-- function) in your own config.
--
-- This example needs no language server: it seeds a fixed set of diagnostics with
-- `btv.diagnostic.set` (the client-set surface — the same one a linter plugin uses)
-- so you can try the motions immediately.
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/diagnostic-nav \
--       cargo run -p bemtvi -- examples/diagnostic-nav/sample.txt
--
-- Then press `]d` a few times to walk the diagnostics (it wraps at the end), `]e`
-- to stop only on the errors, and `<C-w>d` while sitting anywhere on a flagged line
-- to read the full message.

--------------------------------------------------------------------------------
-- Show the signs + inline messages so the seeded diagnostics are visible.
--------------------------------------------------------------------------------
btv.diagnostic.config({
  signs = true, -- the gutter letters (E / W / I / H)
  underline = true, -- squiggles under the flagged span
  virtual_text = true, -- the end-of-line message
})

-- One namespace owns the demo diagnostics (a real plugin would use its own).
local ns = btv.ns.create("diagnostic-nav-demo")
local S = btv.diagnostic.severity

-- The fixed "lint" result, keyed to `sample.txt`. `lnum`/`col` are 0-based; this
-- mirrors the shape an LSP server or a linter plugin hands `btv.diagnostic.set`.
-- stylua: ignore
local DIAGS = {
  { lnum = 2, col = 2, message = "undefined function `prnit` (did you mean `print`?)", severity = S.ERROR },
  { lnum = 3, col = 9, message = "undefined variable `naem` (did you mean `name`?)", severity = S.ERROR },
  { lnum = 7, col = 6, message = "`unusedLocal` is never read", severity = S.WARN },
  { lnum = 9, col = 0, message = "loop may never terminate", severity = S.WARN },
  { lnum = 11, col = 21, message = "undefined function `brek` (did you mean `break`?)", severity = S.ERROR },
  { lnum = 15, col = 10, message = "undefined variable `undefinedThing`", severity = S.ERROR },
  { lnum = 16, col = 0, message = "prefer `io.write` over `print` here", severity = S.HINT },
}

-- Re-seed the diagnostics whenever the sample file is entered, so they survive a
-- reload / buffer switch.
btv.autocmd.create({ "BufReadPost", "BufEnter" }, {
  pattern = "*sample.txt",
  callback = function(args)
    btv.diagnostic.set(ns, args.buf, DIAGS)
  end,
})
