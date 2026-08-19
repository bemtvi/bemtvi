-- btv.picker include / exclude filters — a runnable playground.
--
--   BEMTVI_CONFIG=examples/picker-filters \
--     cargo run -p bemtvi -- examples/picker-filters/sample.txt
--
-- This directory deliberately contains the mess a real project has: a `target/`
-- build artifact, a `vendor/` lock file, and a dotfile — the things `files` and
-- `live_grep` list by default (they search unrestricted, so nothing is ever
-- unfindable) and that the filter boxes are for hiding when you don't want them.

-- Space as the leader, so the maps this file sets below read as `<Space>fs`.
--
-- The SHIPPED picker maps (`\ff` files, `\fg` grep, `\fb` buffers, …) keep
-- bemtvi's default `\`: they are registered when the prelude loads, before any
-- config runs, and `<leader>` is expanded at SET time — so a `mapleader` set here
-- cannot reach them. Type `\ff` for those and `<Space>fs` for the ones below.
vim.g.mapleader = " "

-- 1. Defaults for every filterable picker.
--
--    TYPE  \ff
--    SEE   the picker opens with `[-2]` on the prompt row — the badge for the two
--          exclude patterns below — and neither `target/junk.rs` nor
--          `vendor/lib.lock` in the list. This is the "stop showing me build
--          output" knob; set it once and every picker honors it.
--
--    Then TYPE  <C-g>
--    SEE   the include / exclude rows appear, the exclude one already holding
--          `target/, vendor/`, and the badge gone (the rows say it now).
btv.picker.setup({
  exclude = { "target/", "vendor/" },
  history = 20, -- past lines kept per box for <C-Up> / <C-Down> (0 disables)
})

-- 2. Editing a box re-runs the search.
--
--    TYPE  \ff  then  <C-g>  then  <BS> ten times (clearing the exclude box)
--    SEE   `target/junk.rs` and `vendor/lib.lock` come back as you delete — the
--          source re-runs against the new patterns, it is not a local re-rank.
--
--    TYPE  <C-g> once more to land back on the query, and type `deep`
--    SEE   the query and the filters compose: the fuzzy match narrows what the
--          filter left.

-- 3. What a pattern means.
--
--    In the exclude box, each of these behaves the way you would expect:
--
--      *.lock          any .lock file, at any depth
--      target          the `target` entry AND everything under it
--      vendor/         the same — a trailing `/` just says "the directory"
--      src/**          taken as written; a pattern with a `/` is root-anchored
--      **/{a,b}/**     ONE pattern — a comma inside {…} is alternation, not a
--                      separator
--
--    TYPE  \ff  then  <C-g>  then clear the box and type  *.rs
--    SEE   with that in the EXCLUDE box, every Rust file vanishes; move it to the
--          include box (<C-g> twice more to cycle round) and only Rust files remain.

-- 4. The line history — <C-Up> / <C-Down>.
--
--    TYPE  \ff, <C-g>, clear the box, type `*.lock`, then <Esc>
--    then  \ff, <C-g>
--    SEE   the box opens holding `*.lock` — the last line you used.
--
--    TYPE  <C-Up>
--    SEE   it walks back to the older `target/, vendor/`. <C-Down> walks forward
--          again, and one more press returns the line you were composing.
--
--    The history is per box (an include pattern never surfaces in the exclude box,
--    where it would mean the opposite) and persists across restarts: quit with
--    `:qa`, start the example again, and <C-Up> still has your lines.
--
--    `btv.picker.history("exclude")` reads the list; `btv.picker.forget_history()`
--    clears it.

-- 5. Opening a picker already scoped, from a keymap.
--
--    TYPE  <leader>fs
--    SEE   a picker showing ONLY the `src/` tree, with the rows already revealed
--          (`filters = "open"`) so the scope it was given is visible rather than
--          hidden behind a badge. The seed is a seed — clear the box and the rest
--          of the tree comes back.
btv.keymap.set("n", "<leader>fs", function()
  btv.picker.open("files", { include = "src/**", filters = "open" })
end, { desc = "Find files in src/" })

-- 6. The same for grep.
--
--    TYPE  <leader>fS  then type  needle
--    SEE   only the hits under `src/`. Compare with \fg (plain live grep),
--          which also matches the vendored copy.
btv.keymap.set("n", "<leader>fS", function()
  btv.picker.open("live_grep", { include = "src/**", filters = "open" })
end, { desc = "Live grep in src/" })

-- 7. Your own source can have the boxes.
--
--    TYPE  :lua btv.picker.open("mine")<CR>  then  <C-g>
--    NOTE  the include box may already hold a line — if you tried <leader>fs above,
--          `src/**` is the most recent include you used, and every filterable
--          picker opens from that. Clear it with <BS> first, then type  src
--    SEE   the list narrows. Declaring `filter = true` is all it took: every
--          candidate carrying a `path` is tested against the boxes at the single
--          point they all cross, so the source itself does no filtering work.
--          (A source that shells out to ripgrep should also splice `ctx.rg_globs`
--          into its argv, so the tool prunes the tree instead of streaming paths
--          that are about to be dropped.)
btv.picker.source({
  name = "mine",
  title = "A source of my own",
  filter = true,
  preview = "file",
  items = function(ctx)
    for _, p in ipairs({
      "src/deep/nested.rs",
      "src/main.rs",
      "target/debug/junk.rs",
      "vendor/lib.lock",
      "sample.txt",
    }) do
      ctx.push({ text = p, path = p })
    end
  end,
  confirm = function(item, mode, layer)
    btv.picker.edit(item, mode, layer)
  end,
})

-- 8. `<C-g>` on a picker that has no boxes.
--
--    TYPE  \fb  (the buffers picker)  then  <C-g>
--    SEE   a message saying this picker has no include/exclude filters — rather
--          than two boxes that would filter nothing. Only a source that declared
--          `filter = true` gets them.
