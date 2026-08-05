-- ~~~ nx.diagnostic.config({ update_in_insert = … }) — diagnostics that hold
-- ~~~ still while you type, then catch up ~~~
--
-- A language server re-diagnoses after every `didChange`, and nxvim syncs a
-- `didChange` per keystroke — so applied as they land, the squiggles, gutter signs
-- and inline messages move under the cursor on every key, mostly complaining about
-- a line you haven't finished writing.
--
-- `update_in_insert` sets the timing. nxvim takes a NUMBER OF MILLISECONDS here as
-- well as neovim's two booleans:
--
--   update_in_insert = 3000   (default)  hold an update that lands while you type,
--                                        and apply it once typing has been quiet
--                                        for 3s — a debounce, not a freeze
--   update_in_insert = true              apply every update the moment it lands
--   update_in_insert = false             hold everything until `InsertLeave`
--
-- Nothing is ever lost: while an update is held the newest one is kept, and leaving
-- insert mode applies it immediately whatever the interval is.
--
-- This example needs no language server — it wires a demo "linter" that re-runs on
-- every text change, which is exactly the churn the setting tames.
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/diagnostic-insert \
--       cargo run -p nxvim -- examples/diagnostic-insert/sample.txt
--
--------------------------------------------------------------------------------
-- 1. Make the diagnostics visible (all three surfaces), with a SHORT debounce so
--    the demo doesn't keep you waiting — the shipped default is 3000.
--------------------------------------------------------------------------------
nx.diagnostic.config({
  signs = true, -- the gutter letters (E / W / I / H)
  underline = true, -- squiggles under the flagged span
  virtual_text = true, -- the end-of-line message
  update_in_insert = 1000, -- catch up 1s after you stop typing
})

--------------------------------------------------------------------------------
-- 2. A demo "linter": flags every line containing `bug` (error) or `todo`
--    (warning), re-run on EVERY text change — including each keystroke in insert
--    mode, which is what a real language server does.
--
--    type-this / see-that: put the cursor at the end of line 11 (`local answer =
--    42`), press `A`, and type ` -- bug` without stopping. Nothing moves while the
--    keys are coming; a second after you stop, the error appears — you are still
--    in insert mode. Press `<Esc>` instead of waiting and it appears at once.
--------------------------------------------------------------------------------
local ns = nx.ns.create("diagnostic-insert-demo")
local S = nx.diagnostic.severity

local function lint(bufnr)
  local out = {}
  for i, line in ipairs(nx.buf.lines(bufnr, 0, -1, false)) do
    local at = line:lower():find("bug", 1, true)
    if at then
      out[#out + 1] = {
        lnum = i - 1,
        col = at - 1,
        end_lnum = i - 1,
        end_col = at + 2,
        severity = S.ERROR,
        message = "the word `bug` is not allowed here",
      }
    end
    at = line:lower():find("todo", 1, true)
    if at then
      out[#out + 1] = {
        lnum = i - 1,
        col = at - 1,
        end_lnum = i - 1,
        end_col = at + 3,
        severity = S.WARN,
        message = "unresolved `todo`",
      }
    end
  end
  nx.diagnostic.set(ns, bufnr, out)
end

-- `TextChangedI` fires per keystroke in insert mode; `TextChanged` covers normal
-- mode edits, and `BufEnter` seeds the buffer on open.
nx.autocmd.create({ "BufEnter", "TextChanged", "TextChangedI" }, {
  pattern = "*sample.txt",
  callback = function(args)
    lint(args.buf)
  end,
})

--------------------------------------------------------------------------------
-- 3. Three commands to feel the difference back to back.
--
--    type-this / see-that: run `:DiagLive`, then repeat the `A -- bug` edit from
--    section 2 — now the error (and its squiggle, sign and inline message) appears
--    and re-flows on every single key. `:DiagHold` goes the other way: nothing
--    happens until you press `<Esc>`, however long you pause. `:DiagDebounce` puts
--    the demo back.
--------------------------------------------------------------------------------
nx.command("DiagLive", function()
  nx.diagnostic.config({ update_in_insert = true })
  nx.notify("diagnostics: updating on every keystroke (update_in_insert = true)")
end, { desc = "Update diagnostics on every keystroke" })

nx.command("DiagHold", function()
  nx.diagnostic.config({ update_in_insert = false })
  nx.notify("diagnostics: held until InsertLeave (update_in_insert = false)")
end, { desc = "Hold diagnostic updates until leaving insert mode" })

nx.command("DiagDebounce", function()
  nx.diagnostic.config({ update_in_insert = 1000 })
  nx.notify("diagnostics: debounced 1s after you stop typing")
end, { desc = "Debounce diagnostic updates while typing" })
