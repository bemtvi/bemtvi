-- ~~~ bemtvi on-demand completion: the manual trigger is a SESSION ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/complete-on-demand \
--       cargo run -p bemtvi -- examples/complete-on-demand/sample.txt
--
-- The companion to `examples/ui-complete` (which shows the full engine with
-- `auto = true`). Here the popup NEVER opens on its own — `auto = false` — so the
-- only way in is the trigger key. The point of this example is what happens
-- AFTER that key: the popup it opens keeps following the prefix as you type,
-- exactly like vim's ins-completion narrowing its menu, instead of dying on the
-- next keystroke.
--
-- A manual session:
--   * bypasses `min_chars` for its whole life — it stays open even when you
--     backspace down to a single character, because you asked for it explicitly;
--   * preselects the top row, so <C-y> / <CR> accept without navigating first;
--   * ends when the popup does — <C-e>, <Esc>, an accept, or a prefix that
--     nothing matches. After that, typing is plain insert again until you press
--     the trigger key anew.

--------------------------------------------------------------------------------
-- 1. The engine, on demand only.
--
-- `auto = false` is the whole switch: no popup as you type. `keys.trigger` names
-- the key(s) that open one — unset it defaults to `<C-Space>` / `<C-x><C-o>`, and
-- naming it here is just being explicit. `min_chars = 4` is set deliberately high
-- to make the bypass visible: auto-completion would need four characters, but the
-- manual session happily completes a one-character prefix.
--------------------------------------------------------------------------------
btv.complete.setup {
  sources = { { "buffer" } },
  auto = false,
  min_chars = 4,
  keys = {
    trigger = { "<C-Space>", "<C-x><C-o>" },
    next = { "<C-n>", "<Down>" },
    prev = { "<C-p>", "<Up>" },
    confirm = { "<C-y>", "<CR>" },
    abort = "<C-e>",
  },
}

--------------------------------------------------------------------------------
-- 2. The same trigger as a Lua API, on a second key.
--
-- `btv.complete.trigger()` is the API half of the trigger key — map it wherever
-- you like. It starts an identical session.
--------------------------------------------------------------------------------
btv.keymap.set("i", "<C-j>", btv.complete.trigger, { desc = "completion: open on demand" })

--------------------------------------------------------------------------------
-- TYPE THIS / SEE THAT
--
-- Open the sample and put the cursor on the empty last line, then:
--
--   1. `o` then type `co`      → NOTHING opens. `auto = false`, and `co` is under
--                                `min_chars` anyway.
--   2. <C-Space>               → the popup opens on the 2-char prefix (min_chars
--                                bypassed) listing every `co…` word in the buffer,
--                                with the TOP ROW ALREADY HIGHLIGHTED.
--   3. type `nn`               → the popup STAYS UP and narrows to the `conn…`
--                                words as you type. This is the session — before
--                                it existed, that first keystroke closed it for
--                                good. Your keys still land in the document.
--   4. <BS><BS><BS>            → back to `c`: the popup WIDENS again rather than
--                                closing, still below `min_chars`.
--   5. <C-y>                   → accepts the highlighted row (no navigation step
--                                needed — a manual session preselects).
--
-- Then check the end conditions:
--
--   6. `o`, type `co`, <C-j>   → the Lua-API key opens the same session.
--   7. <C-e> then type `nn`    → aborted: the popup does NOT come back, because
--                                the session ended with the popup. Press
--                                <C-Space> again to start a new one.
--   8. `o`, <C-Space>, `zzz`   → nothing in the buffer matches `zzz`, so the popup
--                                closes on its own and stays closed.
--------------------------------------------------------------------------------
