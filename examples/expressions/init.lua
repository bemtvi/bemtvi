-- The expression surfaces, other than `:s/…/\=…/` (see examples/subst-expr).
--
-- Run this example with the config isolated from your real one:
--
--   BEMTVI_CONFIG=examples/expressions cargo run -p bemtvi -- examples/expressions/sample.txt
--
-- Each section is *type this / see that*. `<leader>N` (leader is <Space> here)
-- runs the demo; `:Cheat` reprints the list.
--
-- Every one of these takes a **string of Lua source**, not a function, because
-- the expression runs in a second, pure VM and a closure cannot cross between
-- VMs. Nothing it needs is read from the editor — it is all passed in.

btv.o.number = true
btv.o.expandtab = true
btv.g.mapleader = " "

-- ===========================================================================
-- 1. Fold levels — `'foldexpr'`, over `line` and `lnum`
--
--    type:  <leader>1   (then zR to open them again)
--    see:   the two `fn … { … }` blocks collapse
--
--    Returning `>1` / `<1` is what expresses nesting: the engine carries the
--    running level from line to line, so the expression only ever looks at the
--    one line it was handed.

local FOLDEXPR = [[ line:find("{%s*$") and ">1" or line:find("^%s*}") and "<1" or "=" ]]

btv.o.foldmethod = "expr"
btv.o.foldexpr = FOLDEXPR

-- ===========================================================================
-- 2. Fold text — `btv.fold.text`, over `first`, `lines`, `lnum`
--
--    type:  <leader>2   then <leader>1 to fold
--    see:   the collapsed row reads `fn alpha() { … 5 lines` instead of the
--           built-in `+--   5 lines: fn alpha() {`
--
--    `:FoldText off` puts the built-in back, so you can compare.

local FOLDTEXT = [[ first:gsub("%s+$", "") .. "  … " .. lines .. " lines" ]]

-- Note the explicit `if`. `cond and nil or X` cannot yield nil in Lua —
-- `true and nil` is nil, and `nil or X` is X — so the idiomatic-looking
-- one-liner would silently re-install the expression instead of clearing it.
btv.command("FoldText", function(a)
  if a.fargs[1] == "off" then
    btv.fold.text(nil)
  else
    btv.fold.text(FOLDTEXT)
  end
end, { nargs = "?", desc = "Toggle the custom foldtext" })

-- ===========================================================================
-- 3. Indentation — `btv.indent.expr`
--
--    type:  <leader>3   (which is `16G=3j` — `=` is a normal-mode operator)
--    see:   lines 17-18 (`six`, `seven`) indent under `fn gamma() {`
--
--    In scope: `prev` (the previous non-blank line), `line`, `lnum`, `sw`
--    (the effective 'shiftwidth') and `previndent`. Returning `nil` declines
--    and lets smartindent/autoindent answer, so you only describe what you
--    care about.

btv.indent.expr([[
  line:match("^%s*}") and previndent - sw
    or prev:match("{%s*$") and previndent + sw
    or previndent
]])

-- ===========================================================================
-- 4. Filetype from content — `btv.filetype.detect`
--
--    type:  <leader>4, then <leader>4 again (it alternates between the two
--           headers sitting next to this file)
--    see:   `:set ft?` says `cpp` for widget.h and `c` for plain.h — the same
--           `.h` extension, decided by what is inside the file
--
--    bemtvi's built-in tables resolve `.h` to `c` and stop there rather than
--    guessing. Returning `nil` declines and leaves that answer alone.

btv.filetype.detect([[
  ext == "h" and (head:find("template", 1, true) or head:find("::", 1, true))
    and "cpp" or nil
]])

-- ===========================================================================
-- 5. Picker ranking — `btv.picker.scorer`, over `label`, `query`, `score`
--
--    type:  <leader>5   then type `mod`
--    see:   `docs/model.md` jumps to the top, ahead of the two source files
--           the matcher scored higher
--
--    `score` is the fuzzy score the row already earned, so a scorer *nudges*
--    the native order rather than reinventing matching. `:Scorer off` turns it
--    off so you can watch the order change back — which is the point of the
--    demo, so the nudge here is one the matcher would not have made on its own.

local SCORER = [[ score + (label:find("^docs/") and 500 or 0) ]]

btv.picker.source({
  name = "demo",
  -- Report the pick rather than opening it: these are illustrative paths, and
  -- seeing which row `<CR>` took is the whole point of the demo.
  confirm = function(item)
    btv.notify("picked " .. item.text)
  end,
  items = function(ctx)
    for _, t in ipairs({
      "src/test/mod.rs",
      "src/model.rs",
      "src/module.rs",
      "docs/model.md",
    }) do
      ctx.push({ text = t })
    end
  end,
})

btv.picker.scorer(SCORER)

btv.command("Scorer", function(a)
  if a.fargs[1] == "off" then
    btv.picker.scorer(nil)
  else
    btv.picker.scorer(SCORER)
  end
end, { nargs = "?", desc = "Toggle the picker scorer" })

-- ===========================================================================
-- 6b. Completion ranking — `btv.complete.scorer`, over `label`, `query`,
--     `score` and `kind`
--
--    type:  o then `for`   (insert mode, in this buffer)
--    see:   the snippet row `for_loop` sits *below* the two real words, even
--           though its source outranks theirs
--
--    `:CompleteScorer off` puts the native order back — the snippet leads again,
--    which is the complaint this nudge answers. `score` is the *blended* native
--    key (fuzzy score + the source's own bias), so subtracting from it composes
--    with source order instead of replacing it.

btv.complete.source {
  name = "demo_snippets",
  priority = 50,
  debounce = 0,
  complete = function(ctx)
    ctx.push { text = "for_loop", kind = "Snippet" }
  end,
}

btv.complete.source {
  name = "demo_words",
  priority = 1,
  debounce = 0,
  complete = function(ctx)
    ctx.push("format_all")
    ctx.push("forward_ref")
  end,
}

btv.complete.setup { sources = { { "demo_snippets" }, { "demo_words" } }, min_chars = 3 }

local CSCORER = [[ score - (kind == "Snippet" and 100 or 0) ]]

btv.complete.scorer(CSCORER)

btv.command("CompleteScorer", function(a)
  if a.fargs[1] == "off" then
    btv.complete.scorer(nil)
  else
    btv.complete.scorer(CSCORER)
  end
end, { nargs = "?", desc = "Toggle the completion scorer" })

-- ===========================================================================
-- 6. The expression register — `"=` and `<C-r>=`
--
--    Nothing to configure: this one is built in. It is the register whose
--    contents are *computed*, and it prompts with `=` for the expression.
--
--    type:  o<C-r>=lnum * 10<CR><Esc>
--    see:   the new line reads `20` — `lnum` is the line you are on
--
--    type:  "=("-"):rep(20)<CR>p
--    see:   a rule pasted in; `"=` stores the result and the `p` after it pastes
--
--    type:  :s/^/<C-r>=lnum<CR>. /<CR>
--    see:   the line numbered — `<C-r>=` inside a command line splices its
--           result into the line you are typing (`<Esc>` there abandons only the
--           expression, not the command)
--
--    In scope: `line` (the text of the line you are on), `lnum` and `col`, both
--    1-based. The result is evaluated once, when you submit the prompt, and
--    stored — so `:registers` shows it and `getreg("=")` reads it back.

-- ===========================================================================
-- 7. Frame-time paint — `btv.decor.expr`, over `line` and `lnum`
--
--    type:  <leader>7   then scroll around
--    see:   every `TODO` and `FIXME` in the file lit up, and the paint keeps up
--           with typing and scrolling *in the same frame* — there is no
--           round trip to wait for
--
--    `:Paint off` removes it. This is the one surface that takes a **block**
--    rather than a single expression: a per-line paint loops over the matches on
--    the line, so the source ends in its own `return`. Spans are
--    `{ first, last, group }` with the 1-based inclusive columns `string.find`
--    already hands back.
--
--    Its sibling `btv.decor.provider` is the general surface — async, full Lua,
--    virtual text and signs. Reach for the block when the paint is a pure
--    function of the line, and for the provider when it is not.

local PAINT = [[
  local out, i = {}, 1
  while true do
    local s, e = line:find("TODO", i, true)
    if not s then
      s, e = line:find("FIXME", i, true)
    end
    if not s then break end
    out[#out + 1] = { s, e, "Todo" }
    i = e + 1
  end
  return out
]]

btv.command("Paint", function(a)
  if a.fargs[1] == "off" then
    btv.decor.expr(nil)
  else
    btv.decor.expr(PAINT)
  end
end, { nargs = "?", desc = "Toggle the frame-time paint" })

-- ===========================================================================
-- The shortcuts. 1 and 3 put a command on the command line (an `<expr>`
-- mapping returns keys to feed, which is the public way to produce keystrokes);
-- the rest act directly.

btv.keymap.set("n", "<leader>1", "zM", { desc = "Close every fold" })
btv.keymap.set("n", "<leader>2", function()
  btv.fold.text(FOLDTEXT)
end, { desc = "Install the custom foldtext" })
btv.keymap.set("n", "<leader>3", "16G=3j", { desc = "Reindent the flat block" })
local function sibling(name)
  local dir = (btv.buf.name(0) or ""):match("^(.*)/") or "."
  return dir .. "/" .. name
end

btv.keymap.set("n", "<leader>4", function()
  local here = btv.buf.name(0) or ""
  btv.cmd("e " .. sibling(here:find("widget%.h$") and "plain.h" or "widget.h"))
end, { desc = "Alternate the two .h headers" })
btv.keymap.set("n", "<leader>5", function()
  btv.picker.open("demo")
end, { desc = "Open the demo picker" })
btv.keymap.set("n", "<leader>7", function()
  btv.decor.expr(PAINT)
end, { desc = "Install the frame-time paint" })

local CHEATS = {
  "<leader>1   zM — fold (foldexpr);  zR reopens",
  "<leader>2   install the custom foldtext;  :FoldText off restores",
  "<leader>3   :16,19=  — reindent the flat block (indentexpr)",
  "<leader>4   alternate widget.h (cpp by content) and plain.h (stays c)",
  "<leader>5   the demo picker — type `mod`; docs/ leads.  :Scorer off compares",
  "6           no mapping — type o<C-r>=lnum * 10<CR><Esc> yourself",
  "6b          type `ofor` — the snippet row sorts last.  :CompleteScorer off",
  "<leader>7   paint every TODO/FIXME as you scroll;  :Paint off removes it",
}

btv.command("Cheat", function()
  btv.ui.float(CHEATS, { title = " expression demos ", relative = "editor" })
end, { desc = "Show the expression demo list" })
