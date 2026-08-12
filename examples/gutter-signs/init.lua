-- ~~~ bemtvi gutter signs + line fill: the `sign_text` / `line_fill` extmark decorations ~~~
--
-- Two render-only extmark decorations (both placed with `btv.buf.set_extmark`):
--
--   * `sign_text` / `sign_hl_group` — a 1–2 cell glyph drawn in the GUTTER (the sign
--     column) on the mark's line. This is the surface a git-status gutter, diagnostics,
--     or a DAP breakpoint marker builds on. Extmark signs share the column with the LSP
--     diagnostic signs; when two signs land on one line the higher `priority` wins.
--
--   * `line_fill = { text, hl_group }` — an btv-native whole-line FILL: `text` repeated
--     across the line's width (e.g. a `─` rule on a blank separator row, or a diff
--     viewer's alignment-gap fill).
--
-- `'signcolumn'` (default `auto`) reveals the column the moment a sign exists; this
-- config sets it to `yes` so the gutter width is stable as you edit.
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/gutter-signs \
--       cargo run -p bemtvi -- examples/gutter-signs/sample.txt
--
-- You'll see a gitsigns-style gutter (a `┃` bar on added lines, `~` on a changed line,
-- a `_` under a deletion) and a `─` rule filling the blank separator line. The signs
-- anchor to their lines, so they track edits — press `o` to open a line and watch the
-- ones below slide down. Try `:GutterSigns` / `:GutterClear`, and `:SignClash` to see
-- priority pick the winner when two signs share a line.

--------------------------------------------------------------------------------
-- The gutter palette (a ported colorscheme would theme these; we define defaults).
--------------------------------------------------------------------------------
btv.hl.define(0, "GutterAdd", { fg = "#a6e3a1" }) -- green  ┃  (added)
btv.hl.define(0, "GutterChange", { fg = "#f9e2af" }) -- yellow ~  (changed)
btv.hl.define(0, "GutterDelete", { fg = "#f38ba8" }) -- red    _  (deleted)
btv.hl.define(0, "FillRule", { fg = "#45475a" }) -- dim    ─  (the fill rule)

-- One namespace owns every mark, so :GutterClear wipes them in a single call.
local ns = btv.ns.create("gutter-signs")

-- A small "diff" plan, keyed by 0-based line: which gutter sign each line carries.
local SIGNS = {
  [0] = { text = "┃", hl = "GutterAdd" }, -- a hunk of added lines (gitsigns' bar)
  [1] = { text = "┃", hl = "GutterAdd" },
  [4] = { text = "~", hl = "GutterChange" }, -- a changed line
  [7] = { text = "_", hl = "GutterDelete" }, -- a deletion marker under line 7
}

-- (Re)place the demo signs + the blank-line fill on the current buffer.
local function place()
  btv.buf.clear_namespace(0, ns, 0, -1)
  local n = btv.buf.line_count(0)
  for line0, s in pairs(SIGNS) do
    if line0 < n then
      btv.buf.set_extmark(0, ns, line0, 0, { sign_text = s.text, sign_hl_group = s.hl })
    end
  end
  -- A whole-line rule across the FIRST blank line (the section separator).
  for i = 0, n - 1 do
    if btv.buf.lines(0, i, i + 1)[1] == "" then
      btv.buf.set_extmark(0, ns, i, 0, { line_fill = { text = "─", hl_group = "FillRule" } })
      break
    end
  end
end

vim.o.number = true
vim.o.signcolumn = "yes" -- reserve the gutter so the layout doesn't jump as signs come/go

btv.command("GutterSigns", place, { desc = "(re)place the demo gutter signs + fill rule" })
btv.command("GutterClear", function()
  btv.buf.clear_namespace(0, ns, 0, -1)
end, { desc = "clear the demo signs + fill" })

-- Two signs on line 0: priority decides which the single column shows. The default
-- extmark priority is 4096; a higher one wins. Run :SignClash to swap a ★ in over the ┃.
btv.command("SignClash", function()
  btv.buf.set_extmark(
    0,
    ns,
    0,
    0,
    { sign_text = "★", sign_hl_group = "GutterChange", priority = 5000 }
  )
  print("placed a priority-5000 ★ on line 1 — it wins the column over the ┃ (priority 4096)")
end, { desc = "demo: a higher-priority sign wins the shared column" })

-- Paint on startup so the gutter shows the moment the file opens.
place()

print(
  "gutter-signs: ┃/~/_ in the gutter, a ─ rule on the blank line. :GutterSigns / :GutterClear / :SignClash"
)
