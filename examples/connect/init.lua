-- ~~~ nx.connect: a connector — teach `:connect` a new scheme ~~~
--
-- A *connector* registers an async resolver for a URL scheme. `:connect <url>` then
-- routes through the local VM: a matching resolver runs (it may provision a
-- remote/container and stream progress), returns a transport spec, and the client
-- swaps the window onto it — nxvim's "reload window onto a new backend". This is the
-- foundation the real `nxvim-remotes` connector (container:// + ssh://) builds on; here a
-- toy `demo://` scheme dials a LOCAL `nxvim --daemon` so you can see the whole loop
-- with no remote host.
--
-- The public API:
--
--   * `nx.connect.register(scheme_or_matcher, resolver)` — `scheme_or_matcher` is a
--     scheme string ("demo", "ssh", "container") or a `fn(url) -> boolean` matcher;
--     `resolver` is `fn(url) -> spec`, where `spec` is the `nx.session.reconnect`
--     table. The resolver MAY return a promise (provision asynchronously) and MAY
--     call `nx.notify` to stream progress.
--   * `:connect <url>` — routes through the registry; with no matching provider it
--     falls back to the built-in direct dial (`nxvim://…` QUIC, or `[user@]host` ssh).
--   * `nx.session.reconnect(spec)` — the imperative form a connector calls directly
--     once it has a transport (skipping URL routing).
--
-- Try it:
--
--     # NXVIM_DAEMON points the demo resolver at the binary to spawn as the "remote"
--     # daemon (a debug build isn't on PATH); drop it if `nxvim` is installed.
--     NXVIM_DAEMON=target/debug/nxvim NXVIM_CONFIG=examples/connect \
--       cargo run -p nxvim -- examples/connect/sample.txt
--     # then, in the editor:
--     :connect demo://here
--     # the window reloads onto a local `nxvim --daemon` (a daemon session). Prove it:
--     :lua print(nx.daemon.status())   -- "connected" (was nil/"local" before)
--     # a scheme with no connector falls back to the built-in dialer:
--     :connect nxvim://127.0.0.1:8765/tok?cert=abc   -- (needs a real QUIC daemon)

-- Register a resolver for the `demo://` scheme. A real connector would ssh/docker out,
-- detect the remote arch, install + launch the daemon, and keep a control-master alive;
-- this one just spawns the sibling `nxvim --daemon` locally to demonstrate the swap.
nx.connect.register("demo", function(url)
  -- Progress is just `nx.notify` — the user sees the bootstrap on the message line.
  nx.notify("connect: provisioning " .. url .. "…")

  -- The daemon command. A structured `argv` runs WITHOUT a shell (nothing can be
  -- smuggled through shell metacharacters); a connector spawning ssh/docker would build
  -- the argv the same way, e.g. { "ssh", host, "nxvim", "--daemon" }. `NXVIM_DAEMON`
  -- names the binary (a debug build isn't on PATH), defaulting to an installed `nxvim`.
  local exe = os.getenv("NXVIM_DAEMON") or "nxvim"
  local spec = {
    transport = { kind = "spawn", argv = { exe, "--daemon" } },
    -- Use the daemon's own config after the swap ("remote"); "local" would keep this
    -- config and let the daemon back only the fs/proc/lsp seams.
    config_source = "remote",
  }

  -- Return the spec directly (synchronous). To provision asynchronously instead, return
  -- a promise that fulfils with the spec — nx.connect awaits it before swapping:
  --
  --   return nx.promise.new(function(resolve)
  --     nx.run({ "some", "provisioning", "step" }):next(function() resolve(spec) end)
  --   end)
  return spec
end)

-- Surface the daemon link state so you can SEE the swap took effect (green ● daemon once
-- connected). See examples/daemon-status for the full walk-through of this segment.
nx.statusline.segment {
  name = "daemon",
  render = function()
    local phase = nx.daemon.status()
    if phase == nil or phase == "local" then
      return nil
    end
    return { { text = " ● " .. phase, hl = "Comment" } }
  end,
}
nx.autocmd.create("User", {
  pattern = "DaemonStatusChanged",
  callback = function()
    nx.statusline.invalidate("daemon")
  end,
})
nx.statusline.setup {
  left = { "mode", "filename" },
  right = { "daemon", "location" },
}
