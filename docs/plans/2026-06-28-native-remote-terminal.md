# Native remote `:terminal` (the edit-host's Term leg)

## The bug

In a daemon (edit-host split) session — TUI over ssh-stdio, or GUI over `:connect`
(QUIC) — `:e /` browses the **remote** filesystem, but `:terminal` opened a PTY on the
**local** machine. So a remote session's shell ran in the wrong place entirely.

## Root cause

nxvim's remote mode is the edit-host split: the local TUI/GUI runs the full editor +
server and routes fs / processes / LSP / watch to a `--daemon` child (`docs/plans/
2026-06-09-edit-host-and-browser-lua.md`). Every facility had a native client seam
forwarding it to the daemon — except terminals:

- The **daemon** half was complete: `run_daemon_io` / `serve_quic` already serve the Term
  leg (`serve_term_daemon_on`, a real PTY whose output streams back as `term_data` /
  `term_exit`). The wasm/browser client drove it via `HostEffects::term_*`.
- The **native client** half never existed. `daemon::split_incoming` dropped the Term
  group (*"Term has no native client consumer"*), `quic::connect_quic` opened only three
  streams (Control/Proc/Lsp), and the native `dispatch_terminal_ops` always spawned a
  **local** PTY via `TerminalManager`, ignoring any daemon connection.

## The fix

Build the native client's Term leg, mirroring the existing `RemoteHostProc`:

1. **`daemon.rs`** — `RemoteHostTerm`: ships `term_open`/`term_write`/`term_resize`/
   `term_kill` on the Term-group `Rpc`, and a demux (`run_term_demux`) decodes the
   daemon's `term_data`/`term_exit` pushes into the *same* `TermEvent` channel the local
   `TerminalManager` feeds — so the run loop's `on_term_events` arm consumes a remote
   terminal identically to a local one. Added to `DaemonClient`; the Term group is now
   routed in `split_incoming` and built in `serve_daemon_link_inner` (both transports).
2. **`quic.rs`** — `quic_dial` opens a fourth Term stream; `connect_quic` wires it. The
   daemon's `accept_bi` loop already serves whatever groups the client opens (keyed by the
   leading tag byte), so **no daemon-side QUIC change** was needed.
3. **`lib.rs`** — `ServerInit::host_term` (native-only); `run_io` takes the remote seam's
   `TermEvent` receiver as the run loop's terminal stream when present (the local actor's is
   left idle) and moves the command seam into `NativeEffects`.
4. **`edithost.rs`** — `NativeEffects::terminal_command` routes to the remote seam when a
   daemon is connected, else the local actor; `has_remote_term()` gates the cwd resolution.
5. **`terminal.rs`** — the open's default cwd resolves against the daemon's `DirState`
   (which honors `:cd`/`:lcd`) in a remote session instead of the local process cwd.
6. **`main.rs` / `nxvim-gui/session.rs`** — wire `client.host_term` into `ServerInit` for
   the stdio (TUI) and QUIC (`:connect`, GUI) paths.

## Tests

- `crates/nxvim-server/tests/daemon_terminal_client.rs` — a real editor whose terminal seam
  is a `RemoteHostTerm` talking to a real-PTY `serve_term_daemon_on` over a duplex.
  `terminal_opens_on_the_daemon_in_its_cwd` distinguishes remote from local by seeding the
  daemon's cwd to a unique temp dir and asserting an interactive `pwd` prints it (a local
  PTY would print the test's cwd). `terminal_input_round_trips_to_the_daemon_child` proves
  the `term_write` leg.
- `crates/nxvim/tests/daemon_quic.rs` — the multi-stream round-trip now also drives a
  `:terminal` PTY over the **Term** QUIC stream end to end.

## Known limitation (pre-existing, shared with the browser)

The daemon sends `term_data` over the backpressured stream channel (`notify_stream`) and
`term_exit` over the control channel (`notify`), and the RPC writer drains control first —
so an **instantly-exiting** child (`:terminal ls`) can have its `term_exit` overtake the
final `term_data`, and the client tears the emulator down before the tail output lands. An
**interactive** terminal (the normal `:terminal` — a persistent shell you type into) has no
such race and is unaffected; this is the same latent ordering characteristic the wasm
terminal path already has, not introduced here. A future fix would order the final data
ahead of the exit without regressing the flood-exit promptness the control channel buys.
