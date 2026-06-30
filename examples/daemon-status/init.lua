-- ~~~ nxvim daemon connection status: a reconnecting remote link, surfaced ~~~
--
-- The edit-host / daemon split runs the editor LOCALLY and only fs/process/watch/LSP
-- on a remote `nxvim --daemon`. An ssh link can drop (a laptop sleeping past QUIC's
-- idle timeout, a flaky hop) — nxvim does NOT tear the session down: it re-dials the
-- connection underneath the seam handles the editor holds, so your buffers/undo
-- survive and editing keeps working. This config surfaces that link's state.
--
-- The public API (so a statusline plugin can render it):
--
--   * `nx.daemon.status()` -> "connected" | "reconnecting" | "disconnected", or
--     `nil` / "local" for a non-daemon session (so a component hides itself).
--   * a `User DaemonStatusChanged` autocmd that fires on every transition.
--   * `:reconnect` re-dials now (resets the retry budget); `:disconnect` drops the
--     link on demand. Both work on the TUI too (server-side ex-commands).
--
-- Try it (needs a daemon — see examples/docker-daemon, or any reachable host):
--
--     # 1. launch the TUI with this config:
--     NXVIM_CONFIG=examples/daemon-status \
--       cargo run -p nxvim -- examples/daemon-status/sample.txt
--     # 2. connect to a daemon over ssh (the GUI/TUI :connect spawns ssh … --daemon):
--     :connect user@host/some/file.txt
--     # 3. now exercise the link:
--     #      :disconnect  -> the segment turns red,
--     #      :reconnect   -> yellow (reconnecting) -> green (connected).
--     #    or pull the network / sleep the laptop: the link auto-retries a few
--     #    times (yellow) and recovers on its own — no command needed.
--
-- A LOCAL session (no daemon) reports nil, so the segment renders nothing.

-- Colors for the three phases. Tweak to taste (or let your colorscheme own them).
vim.api.nvim_set_hl(0, "DaemonOk", { fg = "#a6e3a1", bold = true }) -- green
vim.api.nvim_set_hl(0, "DaemonWait", { fg = "#f9e2af", bold = true }) -- yellow
vim.api.nvim_set_hl(0, "DaemonDown", { fg = "#f38ba8", bold = true }) -- red

-- Map the phase to (icon, highlight). `connected` is intentionally quiet — the dot
-- is enough; `reconnecting` and `disconnected` shout.
local function phase_chunk()
  local phase = nx.daemon.status()
  -- A local (non-daemon) session: render nothing so the segment vanishes.
  if phase == nil or phase == "local" then
    return nil
  end
  if phase == "connected" then
    return { { text = " ● daemon", hl = "DaemonOk" } }
  elseif phase == "reconnecting" then
    return { { text = " ◌ reconnecting…", hl = "DaemonWait" } }
  else -- "disconnected"
    return { { text = " ✕ disconnected (:reconnect)", hl = "DaemonDown" } }
  end
end

-- A custom statusline segment. Its render() is cheap (reads the pushed mirror), and
-- only re-runs when invalidated — we invalidate on the status event below, so the
-- segment repaints exactly when the link changes, not every frame.
nx.statusline.segment {
  name = "daemon",
  render = phase_chunk,
}

-- Re-render the segment whenever the link's phase changes. `DaemonStatusChanged` is a
-- `User` autocmd the run loop fires off the reconnect supervisor's status feed.
nx.autocmd.create("User", {
  pattern = "DaemonStatusChanged",
  callback = function()
    nx.statusline.invalidate("daemon")
  end,
})

-- Put the segment on the right of the bar, alongside the usual built-ins. (If you
-- already have a statusline config, just add the "daemon" segment + the autocmd above.)
nx.statusline.setup {
  left = { "mode", "filename" },
  right = { "daemon", "filetype", "location" },
}
