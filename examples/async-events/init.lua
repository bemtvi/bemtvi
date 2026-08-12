--------------------------------------------------------------------------------
-- Async event model — what a handler may return, and what the editor guarantees.
--
-- Run:
--   BEMTVI_CONFIG=examples/async-events cargo run -p bemtvi -- examples/async-events/sample.txt
--
-- Background: docs/autocmd-events.md, "Hot-path events are synchronous" and
-- "What happens when a handler is async".
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. Hot-path handlers must be SYNCHRONOUS.
--
-- CursorMoved fires on nearly every keypress, so the editor never waits for it.
-- Returning a promise from one is a contract violation and raises, naming the
-- event and the line you registered it at.
--
-- Type-this:  j k h l   (move the cursor)
-- See-that:   the message line shows the move count — no error.
--
-- The async work below still runs; it is simply not RETURNED. Uncomment the
-- `return` to see the raise:
--     E5108: ... CursorMoved handlers must be synchronous (registered at ...):
--     ... Start the async work with btv.schedule / btv.on_next_tick ...
--------------------------------------------------------------------------------
local moves = 0
btv.autocmd.create("CursorMoved", {
  callback = function()
    moves = moves + 1
    -- return                      -- <- uncomment this word to see the hard error
    btv.promise.delay(1):next(function()
      print("cursor moves: " .. moves .. "  (async work, not returned)")
    end)
  end,
})

--------------------------------------------------------------------------------
-- 2. The read sequence is ORDERED, even across async handlers.
--
-- BufReadPost -> FileType -> BufEnter advances one stage at a time, each waiting
-- for the previous stage's async handlers. This BufReadPost handler takes 50ms
-- and sets the filetype; FileType still sees the value it set.
--
-- Type-this:  :messages<CR>
-- See-that:   "2. BufReadPost done (async)" appears BEFORE "2. FileType = demo".
--             Without gating you would instead see FileType fire first, with the
--             extension-derived filetype, and a second FileType afterwards.
--------------------------------------------------------------------------------
btv.autocmd.create("BufReadPost", {
  pattern = "*/sample.txt",
  callback = function(a)
    print("2. BufReadPost start (async, 50ms)")
    return btv.promise.delay(50):next(function()
      btv.bo[a.buf].filetype = "demo"
      print("2. BufReadPost done (async)")
    end)
  end,
})

btv.autocmd.create("FileType", {
  pattern = "demo",
  callback = function(a)
    print("2. FileType = " .. a.match .. " (after the async read handler settled)")
  end,
})

--------------------------------------------------------------------------------
-- 3. LATE SUBSCRIBERS still get the event.
--
-- A handler that registers ANOTHER handler for the same event, asynchronously,
-- while the first is still running: the newcomer receives that same event. This
-- is exactly how an `ft`-lazy plugin with an async `config` works — without it,
-- the plugin would load and its own handler would miss the buffer that woke it.
--
-- Type-this:  :messages<CR>
-- See-that:   "3. late subscriber ran for demo" is there, even though the
--             handler that registered it did not exist when FileType fired.
--
-- Note it fires ONCE. Delivery is filtered by registration order, so handlers
-- that already ran are never re-run.
--------------------------------------------------------------------------------
btv.autocmd.create("FileType", {
  pattern = "demo",
  callback = function()
    return btv.promise.delay(10):next(function()
      btv.autocmd.create("FileType", {
        pattern = "demo",
        callback = function(a)
          print("3. late subscriber ran for " .. a.match)
        end,
      })
    end)
  end,
})

--------------------------------------------------------------------------------
-- 4. The settle BUDGET (500ms default) bounds the WAIT, not the delivery.
--
-- This handler asks for a 40ms budget and then takes ~400ms. The budget expires
-- first, so the sequence advances anyway — one slow handler cannot leave a
-- buffer half-initialised. Two warnings land in :messages: one when the budget
-- blows, naming this file and line, and one when the handler eventually settles,
-- with the elapsed time.
--
-- Type-this:  :messages<CR>
-- See-that:   "... handler exceeded its 40ms budget (.../init.lua:NNN) ..."
--             "... settled NNNms after starting, past its 40ms budget"
--
-- Raise `timeout` for a handler you know is legitimately slow (a first LSP
-- spawn, say) so it does not warn on every open.
--------------------------------------------------------------------------------
btv.autocmd.create("BufWinEnter", {
  pattern = "*/sample.txt",
  timeout = 40,
  callback = function()
    return btv.promise.delay(400):next(function()
      print("4. the slow handler finally finished")
    end)
  end,
})

--------------------------------------------------------------------------------
-- 5. A handler that NEVER settles stays visible.
--
-- It never reports completion, so it would otherwise be invisible. It is listed
-- in btv.autocmd.pending() with its event, site, elapsed time and budget.
--
-- Type-this:  :lua print(vim.inspect(btv.autocmd.pending()))<CR>
-- See-that:   one entry, event = "User", site = this file, elapsed_ms growing.
--------------------------------------------------------------------------------
btv.autocmd.create("User", {
  pattern = "NeverSettles",
  timeout = 50,
  callback = function()
    return btv.promise.new(function() end) -- nobody ever resolves this
  end,
})
btv.autocmd.create("VimEnter", {
  callback = function()
    btv.autocmd.exec("User", { pattern = "NeverSettles" })
    print("5. fired User NeverSettles — see :lua print(vim.inspect(btv.autocmd.pending()))")
  end,
})
