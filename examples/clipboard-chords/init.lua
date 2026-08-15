--------------------------------------------------------------------------------
-- Ctrl+C / Ctrl+V — the built-in system-clipboard chords.
--
-- Run:
--   BEMTVI_CONFIG=examples/clipboard-chords \
--     cargo run -p bemtvi -- examples/clipboard-chords/sample.txt
--
-- bemtvi ships these bound out of the box, on the SYSTEM clipboard (the `"+`
-- register) rather than the unnamed one:
--
--     <C-c> / <C-S-c>   visual   copy the selection      ("+y)
--     <C-v> / <C-S-v>   normal   paste at the cursor     ("+P)
--     <C-v> / <C-S-v>   insert   insert at the caret     (<C-r>+)
--     <C-v> / <C-S-v>   cmdline  insert into the line    (<C-r>+)
--
-- This file is about what you can do WITH them: they are overridable defaults, so
-- everything below is a config taking one back. Sections 1-2 need no config at
-- all — they are the shipped behaviour; 3-5 are the overrides, commented out so
-- you meet the defaults first.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. Copy and paste, no config required.
--
-- Type-this:  vee<C-c>        (select two words, copy)
-- Type-this:  j0<C-v>         (down a line, to column 0, paste)
-- See-that:   the words land AT the cursor, pushing the rest of the line right.
--             Paste is `P`, not `p` — where a non-modal editor puts it.
--
-- Then paste into another application: it is on the real system clipboard, not
-- just an editor register.
--
-- Type-this:  Vj<C-c>         (select two whole lines, copy)
-- Type-this:  <C-v>           (paste)
-- See-that:   a linewise copy pastes as whole lines, above the current one.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. Pasting while typing.
--
-- Type-this:  A << <C-v> >><Esc>
-- See-that:   the clipboard goes in at the caret and insert mode continues, so
--             the `>>` you type afterwards lands after it.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. Take a chord back.
--
-- The chords are registered as `default = true` maps, so any map of your own on
-- the same key wins — no need to unmap first.
--
-- Uncomment, restart, then press <C-v> in normal mode:
--     See-that: the message line says so and nothing is pasted.
--------------------------------------------------------------------------------
-- btv.keymap.set("n", "<C-v>", function()
--   btv.notify("Ctrl+V is mine now", "info")
-- end, { desc = "Hijacked paste" })

--------------------------------------------------------------------------------
-- 4. Turn one off entirely.
--
-- Map it to an empty function: the key becomes a no-op rather than falling back
-- to the built-in.
--
-- Uncomment, restart, then press <C-c> in visual mode:
--     See-that: nothing is copied (the clipboard keeps whatever it had).
--------------------------------------------------------------------------------
-- btv.keymap.set("v", "<C-c>", function() end, { desc = "Disable the copy chord" })

--------------------------------------------------------------------------------
-- 5. The command line takes the chord too.
--
-- No config needed — `<C-v>` is bound in cmdline mode as well, so a path copied
-- out of a terminal goes straight into `:e` instead of being retyped.
--
-- Type-this:  0v$<C-c>        (on the sample's left-margin path line)
-- Type-this:  :e <C-v>
-- See-that:   the path appears in the command line, ready to open. <Esc> to bail.
--
-- Adding a chord bemtvi does NOT ship — a cut to match the copy:
--------------------------------------------------------------------------------
-- btv.keymap.set("v", "<C-x>", '"+d', { desc = "Cut the selection to the clipboard" })

--------------------------------------------------------------------------------
-- 6. A copy needs somewhere to copy TO.
--
-- `"+` resolves to a real clipboard: a host tool (wl-copy / pbcopy / xclip), a
-- terminal that speaks OSC 52 (the ssh case — the copy rides an escape out to the
-- terminal you are actually sitting at), or the browser's navigator.clipboard in
-- the web client. With none of them a copy fails LOUDLY rather than going quietly
-- nowhere.
--
-- Type-this:  :registers<CR>
-- See-that:   the `"+` row shows what the chords put there.
--------------------------------------------------------------------------------

btv.autocmd.create("UIEnter", {
  once = true,
  callback = function()
    print("clipboard chords: <C-c> copies (visual), <C-v> pastes (normal/insert)")
  end,
})
