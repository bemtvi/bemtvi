-- ~~~ nxvim :TSInstall — fetch & compile a treesitter grammar on demand ~~~
--
-- Auto-indent (press <CR> after `if x {` and the new line indents), structural
-- highlighting, and the `=`/`==`/`gg=G` operators are all driven by a treesitter
-- grammar + its `indents.scm`. `:TSInstall <lang>` puts those on disk for you:
-- it downloads the grammar source + queries from nvim-treesitter (pinned to one
-- commit, so it's reproducible), compiles `parser/<lang>.so` in-process, and
-- drops `queries/<lang>/` next to it under nxvim's data dir.
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/ts-install \
--       cargo run -p nxvim -- examples/ts-install/sample.rs
--
-- Then, inside the editor:
--
--     :TSInstall rust          " fetch + compile the Rust grammar (first run only)
--     :TSInstallInfo           " list installed parsers + their queries / root
--
-- Watch the message line: "TSInstall: installing rust…" then
-- "TSInstall: installed rust @ 77a37472 [<compiler>] (queries: …)". The buffer
-- re-highlights and gains auto-indent immediately — no `:e` needed.
--
-- THE COMPILER. `:TSInstall` needs a C compiler. It tries, in order:
--   1. $NXVIM_CC (e.g. `NXVIM_CC="zig cc"`), then
--   2. a system `cc` / `clang` / `gcc` / `zig` on $PATH, then
--   3. a pinned Zig it downloads + checksum-verifies on demand — so even with no
--      toolchain at all, the install just works (the download is one-time).
--
-- ALREADY USE NEOVIM? If you've run nvim-treesitter, nxvim also searches your
-- existing `~/.local/share/nvim/site/` read-only, so those grammars light up
-- with no `:TSInstall` here at all.
--
-- WHERE THINGS LAND. nxvim's data dir — `$NXVIM_DATA_DIR`, else
-- `$XDG_DATA_HOME/nxvim`, else `~/.local/share/nxvim` — under `parser/` and
-- `queries/`. Delete `parser/rust.so` to force a re-install.

-- Spaces, not tabs, so the indentation below is visible as columns.
vim.o.expandtab = true
vim.o.shiftwidth = 4
vim.o.tabstop = 4

print("ts-install demo: run  :TSInstall rust  then edit sample.rs (o / <CR> auto-indent)")
