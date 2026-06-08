-- ~~~ nxvim × telescope.nvim: the real fuzzy finder, running on nxvim ~~~
--
-- This loads your EXISTING telescope.nvim + plenary.nvim install (the ones under
-- ~/.local/share/nvim/lazy) and drives them on nxvim's vim.* surface — no fork, no
-- shim in the plugin, the upstream code as-is. Run it from the repo root:
--
--     NXVIM_CONFIG=examples/telescope \
--     NXVIM_RUNTIMEPATH="$HOME/.local/share/nvim/lazy/telescope.nvim:$HOME/.local/share/nvim/lazy/plenary.nvim" \
--       cargo run -p nxvim -- examples/telescope/sample.txt
--
-- (Adjust the two paths if your plugins live elsewhere. If `lazy` installed them
--  under a different root, point NXVIM_RUNTIMEPATH at the telescope.nvim and
--  plenary.nvim directories — each must contain a `lua/` folder.)
--
-- WHAT IT EXERCISES, end to end, in the running editor — the surface nxvim grew to
-- host telescope:
--   * floating windows (nvim_open_win) for the prompt / results / preview
--   * scratch buffers + nvim_buf_set_option/buftype=prompt emulation
--   * nvim_buf_attach → on_lines: the prompt-change channel that re-runs the finder
--     as you type (wired through the server's buffer-change detection)
--   * plenary.async + uv.spawn: the job pipeline behind live_grep (ripgrep)
--   * extmarks incl. accepted-but-unrendered virtual text (the result counter)
--   * vim.o.columns / vim.o.lines: the screen extent telescope centers floats in
--
-- TRY IT (the prompt opens in insert mode — just type to filter):
--   <leader>fb   fuzzy-find over a STATIC list (works with zero external tools)
--   <leader>ff   find_files in the cwd        (needs `fd`, `rg`, or `find` on PATH)
--   <leader>fg   live_grep across the cwd     (needs `rg` on PATH)
--   <CR> selects · <C-n>/<C-p> move · <Esc> closes
--
-- The leader is Space.

vim.g.mapleader = " "

local telescope = require("telescope")
telescope.setup({
  defaults = {
    -- A simple, dependency-free layout that fits nxvim's screen reporting.
    sorting_strategy = "ascending",
    layout_strategy = "flex",
  },
})

local builtin = require("telescope.builtin")

-- A static-list picker, assembled the way telescope's own pickers are, so the demo
-- works even on a machine with no fd/rg/find: type to fuzzy-filter, <CR> prints the
-- pick on the message line.
local function fruit_picker()
  local pickers = require("telescope.pickers")
  local finders = require("telescope.finders")
  local conf = require("telescope.config").values
  local actions = require("telescope.actions")
  local action_state = require("telescope.actions.state")

  pickers
    .new({}, {
      prompt_title = "Fruits (fuzzy-filter me)",
      finder = finders.new_table({
        results = {
          "apple", "apricot", "avocado", "banana", "blackberry", "blueberry",
          "cherry", "clementine", "cranberry", "date", "elderberry", "fig",
          "grape", "grapefruit", "guava", "kiwi", "lemon", "lime", "mango",
          "nectarine", "orange", "papaya", "peach", "pear", "pineapple",
          "plum", "pomegranate", "raspberry", "strawberry", "tangerine",
        },
      }),
      sorter = conf.generic_sorter({}),
      attach_mappings = function(prompt_bufnr)
        actions.select_default:replace(function()
          local entry = action_state.get_selected_entry()
          actions.close(prompt_bufnr)
          print("you picked: " .. (entry and entry[1] or "<nothing>"))
        end)
        return true
      end,
    })
    :find()
end

local map = vim.keymap.set
map("n", "<leader>fb", fruit_picker, { desc = "Telescope: fuzzy-find a fruit (no deps)" })
map("n", "<leader>ff", function() builtin.find_files() end, { desc = "Telescope: find files" })
map("n", "<leader>fg", function() builtin.live_grep() end, { desc = "Telescope: live grep" })

print("telescope ready — press <Space>fb (static), <Space>ff (files), or <Space>fg (grep)")
