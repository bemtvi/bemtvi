-- ~~~ nxvim nx.panel playground: a transient, focus-grabbing bottom overlay ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/panel \
--       cargo run -p nxvim -- examples/panel/sample.txt
--
-- `nx.panel` is the transient, input-grabbing bottom overlay — the surface the
-- built-in listings (`:messages`, `:registers`, `:ls`, `:marks`, …) ride. Unlike
-- `nx.view` (a *persistent* dockable sidebar), a panel is *modal*: opening it shrinks
-- the main window into the rows above and **locks focus** to the panel — `<C-w>`
-- navigation is inert — until you dismiss it. Its content is an ordinary `nomodifiable`
-- buffer, so you get everything a real buffer has: motions navigate, and selection /
-- dismissal are plain buffer-local keymaps installed by a `FileType` autocmd. There is
-- no bespoke navigation / content / select API.
--
-- TRY IT interactively:
--   <leader>p   open a fruit-picker panel
--   j / k       move within the list (ordinary motions)
--   <CR>        "choose" the line under the cursor (echoes it) and close the panel
--   q / <Esc>   dismiss the panel (a default map every panel gets for free)

--------------------------------------------------------------------------------
local FRUITS = { "apple", "banana", "cherry", "date", "elderberry", "fig" }

-- Behavior is attached the unified way — a `FileType` autocmd over the panel buffer,
-- exactly like `:ls` / quickfix / the file explorer. The autocmd fires when the panel
-- is opened with `filetype = "fruitpanel"`, and `args.buf` is the panel's buffer, so the
-- `<CR>` map is buffer-local and never leaks to other buffers. `q` / `<Esc>` to close are
-- installed automatically by the built-in `nxpanel`/`nxlisting`/`nxbuffers` ftplugin — a
-- panel with its *own* filetype (like this one) opts out, so we add a close map too.
nx.autocmd.create("FileType", {
  pattern = "fruitpanel",
  callback = function(args)
    nx.keymap.set("n", "<CR>", function()
      local choice = tostring(nx.current_line())
      nx.panel.close() -- a "choose" action: dismiss, then act on the picked line
      nx.schedule(function()
        vim.notify("you chose: " .. choice)
      end)
    end, { buffer = args.buf, desc = "Choose fruit" })

    nx.keymap.set("n", "q", nx.panel.close, { buffer = args.buf, desc = "Close panel" })
    nx.keymap.set("n", "<Esc>", nx.panel.close, { buffer = args.buf, desc = "Close panel" })
  end,
})

-- `<leader>p` mounts the panel. `nx.panel.open{ name?, lines, filetype?, height? }` creates
-- (or reuses, keyed by `name`) a read-only buffer holding `lines`, tags the filetype (which
-- fires the autocmd above), and shows it in the focus-locked bottom overlay. The `name`
-- makes it unique: re-opening replaces its content, and it shows up under `:lspanels`.
nx.keymap.set("n", "<leader>p", function()
  nx.panel.open({ name = "[Fruit]", lines = FRUITS, filetype = "fruitpanel", height = 8 })
end, { desc = "Open the fruit panel" })

-- The built-in listings ride the very same mechanism — try `:messages`, `:ls`,
-- `:registers`, `:marks`, and `:lspanels` to list the panels themselves (dismiss with `q`).
-- Panel buffers are hidden from `:ls` (they're surfaces, not documents) and always open as
-- panels — `:b [Fruit]` re-opens the panel rather than showing it in the main window.
print("nx.panel example loaded — press <leader>p, or try :messages / :ls / :lspanels")
