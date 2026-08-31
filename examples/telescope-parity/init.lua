-- ~~~ bemtvi telescope-parity config: your telescope keymaps, natively ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/telescope-parity \
--       cargo run -p bemtvi -- examples/telescope-parity/sample.txt
--
-- This is a straight port of a telescope.nvim finder config to bemtvi's OWN
-- `btv.picker` + `btv.lsp` — no plugins, no compat layer. Every telescope call maps
-- to a native equivalent; the handful telescope had that bemtvi doesn't ship
-- built-in (fd-based files, git files, `-uu`/exclude greps, current-buffer fuzzy
-- find, diagnostics, keymaps, a picker-of-pickers) are small custom
-- `btv.picker.source` drivers right here — the same shape the shipped sources use.
--
-- The two nice simplifications over the original telescope config:
--   * The awkward `yank_call_paste` dance (yank selection → schedule → feedkeys
--     `<C-r>`) collapses into the picker's built-in prompt seeding:
--     `btv.picker.open(source, { query = <selection> })`.
--   * LSP pickers are just `btv.lsp.references()` / `.document_symbol()` /
--     `.type_definition()` — they route their results into `btv.picker` for free.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 0. Shared bits
--------------------------------------------------------------------------------

-- Directories excluded from the "unrestricted + excludes" grep, mirroring the
-- `global_g_args` table from the original myutils.lua. Edit in place, or call
-- `extend_global_excludes{ "!target", "!dist" }` from later in your config.
local global_excludes = { "!node_modules", "!.idea", "!.vscode", "!.neovim", "!.venv" }

local function extend_global_excludes(globs)
  for _, g in ipairs(globs) do
    global_excludes[#global_excludes + 1] = g
  end
end
_ = extend_global_excludes -- exported for your own config; silence "unused" if not called

-- Build the ripgrep argv for a live grep, mirroring `myutils.live_grep(us, g_args)`.
-- `--vimgrep` already implies --no-heading --with-filename --line-number --column,
-- so the parser downstream always gets `file:line:col:text`.
local function rg_args(query, opts)
  opts = opts or {}
  local args = { "--vimgrep", "--color=never", "--smart-case" }
  if opts.unrestricted and opts.unrestricted > 0 then
    args[#args + 1] = "-" .. string.rep("u", opts.unrestricted) -- -u, -uu, …
  end
  for _, g in ipairs(opts.globs or {}) do
    args[#args + 1] = "-g"
    args[#args + 1] = g
  end
  args[#args + 1] = "--"
  args[#args + 1] = query
  return args
end

-- Register a dynamic live-grep source that runs `rg` with a fixed set of extra
-- flags (`opts.unrestricted`, `opts.globs`). One factory covers plain grep, `-uu`,
-- and `-uu` + excludes — exactly the three telescope live_grep variants.
local function make_grep(name, title, opts)
  btv.picker.source({
    name = name,
    title = title,
    layer = "main",
    dynamic = true, -- re-run rg after each (debounced) query edit
    preview = "location", -- scroll the preview to the match and highlight it
    items = btv.async(function(ctx)
      if ctx.query == "" then
        return
      end
      local stream = btv.run_stream({ cmd = "rg", args = rg_args(ctx.query, opts), cwd = ctx.cwd })
      ctx.on_cancel(function()
        stream:kill()
      end)
      for batch in btv.await_each(stream) do
        for _, l in ipairs(batch) do
          local file, lnum, col = l:match("^(.-):(%d+):(%d+):")
          if file then
            ctx.push({ text = l, path = file, row = tonumber(lnum), col = tonumber(col) })
          end
        end
      end
    end),
    confirm = function(item, mode, layer)
      btv.picker.edit(item, mode, layer)
    end,
  })
end

-- Stream a plain listing command (one path per line) as file candidates.
local function make_files(name, title, cmd, cmd_args)
  btv.picker.source({
    name = name,
    title = title,
    layer = "main",
    preview = "file",
    items = btv.async(function(ctx)
      local stream = btv.run_stream({ cmd = cmd, args = cmd_args, cwd = ctx.cwd })
      ctx.on_cancel(function()
        stream:kill()
      end)
      for batch in btv.await_each(stream) do
        for _, l in ipairs(batch) do
          if l ~= "" then
            ctx.push({ text = l, path = l })
          end
        end
      end
    end),
    confirm = function(item, mode, layer)
      btv.picker.edit(item, mode, layer)
    end,
  })
end

--------------------------------------------------------------------------------
-- 1. File / grep sources (telescope find_files / git_files / live_grep family)
--------------------------------------------------------------------------------

-- <leader>ff  find_files with `fd -u -t file` (overrides the shipped rg-based "files")
make_files("files", "Find Files", "fd", { "-u", "-t", "file", "--color", "never" })
-- <C-p>  git_files
make_files("git_files", "Git Files", "git", { "ls-files" })

-- <leader>fg  live grep            <leader>fA  live grep -uu
-- <leader>fG  live grep -uu + excludes
make_grep("live_grep", "Live Grep", {})
make_grep("live_grep_uu", "Live Grep (-uu)", { unrestricted = 2 })
make_grep(
  "live_grep_ex",
  "Live Grep (-uu, excludes)",
  { unrestricted = 2, globs = global_excludes }
)

-- The `curbuf` (current-buffer fuzzy find), `diagnostics`, `keymaps`, and
-- `pickers` (picker-of-pickers) sources telescope has are all shipped built-in by
-- bemtvi now — `btv.picker.open("keymaps")` etc. work with no config — so this file
-- only defines the process-spawning sources telescope customized (fd files, git
-- files, and the `-uu`/exclude grep variants). The maps below wire them up.

--------------------------------------------------------------------------------
-- 2. Keymaps
--------------------------------------------------------------------------------
local map = vim.keymap.set

-- Seed a picker's prompt with the visual selection — the native replacement for
-- the old `yank_call_paste`. Yank the selection to register z, then (next tick,
-- once the register mirror has refreshed) open the picker pre-filled with it.
local function with_selection(source)
  return function()
    btv._feedkeys('"zy', false, false)
    btv.on_next_tick(function()
      local q = btv.reg.get("z"):gsub("%s+", " ")
      btv.picker.open(source, { query = q })
    end)
  end
end

local function open(source)
  return function()
    btv.picker.open(source)
  end
end

-- Files / grep
map("n", "<leader>ff", open("files"), { desc = "Find files" })
map("v", "<leader>ff", with_selection("files"), { desc = "Find files (selection)" })
map("n", "<C-p>", open("git_files"), { desc = "Git files" })
map("n", "<leader>fg", open("live_grep"), { desc = "Live grep" })
map("v", "<leader>fg", with_selection("live_grep"), { desc = "Live grep (selection)" })
map("n", "<leader>fG", open("live_grep_ex"), { desc = "Live grep -uu + excludes" })
map(
  "v",
  "<leader>fG",
  with_selection("live_grep_ex"),
  { desc = "Live grep -uu + excludes (selection)" }
)
map("n", "<leader>fA", open("live_grep_uu"), { desc = "Live grep -uu" })
map("v", "<leader>fA", with_selection("live_grep_uu"), { desc = "Live grep -uu (selection)" })
map("n", "<leader>fb", open("buffers"), { desc = "Buffers" }) -- shipped source
map("n", "<leader>fr", btv.picker.resume, { desc = "Resume last picker" }) -- shipped action
map("n", "<leader>fi", open("pickers"), { desc = "Pickers (builtin)" })
map("n", "<leader>fk", open("keymaps"), { desc = "Keymaps" })
map("n", "<leader>fm", open("marks"), { desc = "Marks" })
map("n", "<C-/>", open("curbuf"), { desc = "Fuzzy find in current buffer" })

-- Code / LSP — these route their results into btv.picker on their own.
map("n", "<leader>cx", open("diagnostics"), { desc = "Diagnostics" })
map("n", "<leader>cs", btv.lsp.document_symbol, { desc = "LSP document symbols" })
map("n", "<leader>cr", btv.lsp.references, { desc = "LSP references" })
map("n", "<leader>ct", btv.lsp.type_definition, { desc = "LSP type definitions" })

-- Not ported — no native equivalent (documented so nothing fails silently):
--   <leader>fh  help_tags — bemtvi is not neovim; there is no `:help` doc set.
-- Wire it up here once that surface exists.
