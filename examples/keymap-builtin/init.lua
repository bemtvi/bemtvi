-- ~~~ nxvim keymap playground: instant built-ins under a colliding map ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/keymap-builtin \
--       cargo run -p nxvim -- examples/keymap-builtin/sample.txt
--
-- The point of this playground is the ABSENCE of a pause. nxvim's matcher
-- consults the editor's *own* command grammar (a shared, pure classifier,
-- `nxvim_core::command_status`). So when a user map shares a built-in's prefix
-- (here `gh`, `dh`, `fh`, `rx` all collide with built-ins), the built-in still
-- fires the INSTANT its sequence completes — no idle flush, no following key.
--
-- Each section says what to TYPE and what you should SEE. Unlike the Phase 4
-- idle-flush playground, you should NOT wait: the built-ins are immediate.

--------------------------------------------------------------------------------
-- 1. gg under a colliding `gh` map.
--    TYPE:  G            (jump to the last line, so the move is visible)
--    TYPE:  gg           and STOP. The cursor jumps to the FIRST line IMMEDIATELY
--           — no pause. The second `g` could have continued to `gh`, but `gg` is
--           a complete built-in, so the matcher releases it to the editor at once.
--    Compare: TYPE  gh   -> the MAPPING fires ("gh mapping fired") instead. Same
--           prefix, different completion — the map wins only when you actually
--           type it.
--------------------------------------------------------------------------------
vim.keymap.set("n", "gh", function()
  print("gh mapping fired (not the gg built-in)")
end)

--------------------------------------------------------------------------------
-- 2. Operators under a colliding `dh` map.
--    TYPE:  dd   -> deletes the current line instantly (no flush).
--    TYPE:  dw   -> deletes to the next word instantly.
--    Both work though `d` is a live prefix of the `dh` map below.
--------------------------------------------------------------------------------
vim.keymap.set("n", "dh", function()
  print("dh mapping fired (not the dd/dw built-in)")
end)

--------------------------------------------------------------------------------
-- 3. find-char under colliding `fh` / `ff` maps.
--    TYPE:  fx   -> jumps to the next `x` on the line instantly (the target char
--           is delivered straight through, though `f` prefixes the maps).
--    TYPE:  ;    -> repeats the find instantly.
--------------------------------------------------------------------------------
vim.keymap.set("n", "fh", function()
  print("fh mapping fired")
end)
vim.keymap.set("n", "ff", function()
  print("ff mapping fired")
end)

--------------------------------------------------------------------------------
-- 4. replace under a colliding `rx` map.
--    TYPE:  rZ   -> replaces the char under the cursor with `Z` instantly.
--    (TYPE:  rx  -> fires the MAPPING instead — that exact sequence is mapped.)
--------------------------------------------------------------------------------
vim.keymap.set("n", "rx", function()
  print("rx mapping fired (not the r{char} built-in)")
end)

--------------------------------------------------------------------------------
-- 5. The INVERSE — a genuinely-ambiguous *mapped* prefix still WAITS.
--    The disambiguation only releases a built-in when the typed run BREAKS every
--    live mapping prefix. When the run is itself a real prefix of a longer
--    MAPPING (e.g. `gg` while `ggx` is mapped, or `j` while `jk` is mapped), it
--    is correctly held and resolved by the idle flush — matching vim's
--    timeoutlen, with the user map still winning. We deliberately do NOT map such
--    a g-sequence here (it would defeat section 1's instant `gg`); see the
--    examples/phase4-config playground for that held/flush case.
--------------------------------------------------------------------------------
