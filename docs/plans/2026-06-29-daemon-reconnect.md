# Reliable remote daemon connection (reconnect + status)

Date: 2026-06-29

## Problem

A user had a GUI session connected to a remote `--daemon` over QUIC (SSH-launched).
The laptop slept; QUIC's 30 s idle timeout (`crates/nxvim-server/src/quic.rs`
`MAX_IDLE_TIMEOUT`) tore the connection down (SSH printed `Connection reset by peer`
/ `Broken pipe`). On wake, **nothing worked**: there is no reconnect, no health
detection, and no status surfaced anywhere — GUI, TUI, or web.

## Architecture recap (why this is tractable)

Confirmed model — **the editor runs locally; only host seams cross the wire** (Model
B; `crates/nxvim-gui/src/session.rs:1-9`). A remote session is a local
`nxvim_server::run` over an in-process duplex, with its fs / proc / lsp / term seams
pointed at a `DaemonClient` (`crates/nxvim-server/src/daemon.rs`). The four leg groups
ride four QUIC streams (Control/Proc/Lsp/Term), or one ssh/stdio stream split by
method.

Consequences for reconnect:

- **Local editor state (buffers, undo, cursor, windows, Lua) survives a dropped
  connection.** We must *not* tear the session down — that's what loses state.
- The seams already **fail loud** when the link is dead (e.g. `RemoteFsJobs::run`
  returns `ENOTCONN`; `RemoteHostProc` emits `exited(-1, "daemon connection closed")`)
  — so a dead link does *not* hard-freeze the editor; it makes every *remote* op
  (save, read, LSP, watch, terminal) fail permanently with no recovery. That permanent
  failure is the "nothing works" the user saw.
- Therefore the fix is **reconnect the link underneath the existing, stable seam
  handles** — not a session swap. The `DaemonClient` handles the editor holds stay
  put; the transport beneath them re-dials and rebinds.

## Decisions (from the user)

- **Recovery depth: re-establish seams, keep editing.** No daemon-side session
  survival. Running remote terminals / background jobs are lost on reconnect; LSP is
  re-opened. Smallest change that fixes the freeze. (Not: preserve the daemon session.)
- **Bounded retry, then `:reconnect`.** Auto-retry a few times with backoff; on
  exhaustion, stop and tell the user to run `:reconnect`.
- **Status must be a public API** so `~/work/nxvim-plugins/nxvim-line` can render it:
  `connected` green, `reconnecting` yellow, `disconnected` red.
- **All three clients** (GUI, TUI, web) must be reliable.

## Core mechanism: the reconnectable link supervisor

Today `connect_daemon` / `connect_quic` dial **once**, build seams bound to that one
connection's `Rpc`s, hand out the `DaemonClient`, then run the per-group demuxes until
EOF and exit the link thread. The new model keeps the link thread alive and supervises
reconnection:

1. **Dialer abstraction.** A `Dialer` that can produce a fresh connection on demand:
   - QUIC: re-run `quic_dial` (re-open the 4 streams).
   - ssh/stdio: re-spawn the child (`ssh … nxvim --daemon`) and reconnect over its
     stdio — so the dialer must own a `Command` factory, not pre-opened pipes.
   - test: an in-process duplex factory (severable + redialable).
2. **Swappable current-`Rpc` per group, behind stable seam handles.** Seams route
   requests through a shared cell (`std::sync::Mutex<Option<Rpc>>` or
   `tokio::sync::watch<Option<Rpc>>`) rather than a fixed `Rpc` clone. `request()`:
   `Some(rpc) => rpc.request(...).await`, `None => Err("daemon disconnected")`.
3. **Stable downstream push channels.** `watch_tx` (→ `host_fs.watch_rx`),
   `proc_inflight`, `lsp_inflight`, and the term-event channel are created **once** and
   kept across reconnects; the supervisor re-runs each group's demux feeding the *same*
   channels on every new connection.
4. **Supervisor loop** on the link thread:
   - Initial `dial()` → publish per-group `Rpc`s into the cells → build seams once →
     hand out `DaemonClient` → emit `Connected`.
   - Run this connection's demuxes (`tokio::join!`) until EOF (connection dead).
   - On death: clear the cells (seams now fail loud) → emit `Disconnected` → auto-retry
     up to `N` times with backoff (emit `Reconnecting{attempt}` each). On success:
     republish `Rpc`s, emit `Reconnected`, resume demuxes. On exhaustion: emit
     `Disconnected{final}` with a "run `:reconnect`" message, then **park** on a
     reconnect signal.
   - `:reconnect` pokes the signal (resets the budget, re-enters dial). `:disconnect`
     tears the link down.

Retry policy (constants, overridable later by an `nx.o` option): ~5 attempts, backoff
0.5 → 1 → 2 → 4 → 8 s (capped).

### Status surfacing

- A stable **status channel** link-thread → editor (sibling of `watch_tx`). The run
  loop selects on it, stores `editor.daemon_status: DaemonStatus`, fires a
  status-changed event, and refreshes the statusline; on final give-up it echoes the
  loud "run `:reconnect`" message.
- `DaemonStatus = Local | Connected | Reconnecting { attempt, max } | Disconnected`.

### Public API for plugins

- `nx.daemon.status()` → `"connected" | "reconnecting" | "disconnected"` (plus
  `attempt`/`max` detail); a non-daemon session reports `nil`/`"local"` so a component
  hides itself.
- A `DaemonStatusChanged` event (autocmd / `nx.on_*`) so a statusline component
  re-renders on change.
- `nxvim-line` component (in `~/work/nxvim-plugins/nxvim-line`) rendering
  connected=green, reconnecting=yellow, disconnected=red, redrawing on the event.

### Commands

- `:reconnect` — signal the supervisor to reset the retry budget and re-dial now.
  Server-side ex-command (not client-intercepted like `:connect`), because reconnect
  rebinds seams in place — no session swap — so it works on the TUI too.
- `:disconnect` — tear the link down (status → `Disconnected`, no auto-retry).

## Phases (commit + pause for review between each)

- **Phase 1 — Reconnectable link supervisor (core refactor).** Dialer + swappable
  per-group `Rpc` + stable push channels + supervisor with a *manual* re-dial trigger
  (no auto-retry yet). Keep a one-shot `connect_daemon(reader, writer)` shim so
  existing call sites/tests compile unchanged. Hermetic test: severable+redialable
  duplex — seam fails loud while down, resumes after a re-dial; the editor's local
  buffer survives.
- **Phase 2 — Link-layer auto-retry + status (DONE).** The supervisor auto-retries a
  dropped link with bounded backoff (`ReconnectPolicy`, default 5 attempts over 0.5 →
  8 s), then parks `Disconnected` until a manual reconnect. A `DaemonStatus`
  (`Connected | Reconnecting{attempt,max} | Disconnected`) rides a `watch` channel; the
  `ReconnectHandle` exposes `reconnect()` / `disconnect()` / `status()` / `subscribe()`.
  Tested: auto-recovery with no manual action, budget-exhaustion → park → manual
  reconnect, and `:disconnect`. *(Editor-side wiring of these — below — is Phase 3.)*
- **Phase 3 — Editor integration: status + commands + `nx` API.** Thread the
  `ReconnectHandle` into `ServerInit`; the run loop selects on its status `watch` and
  stores `DaemonStatus` in the editor, fires a status-changed event, and echoes the loud
  "run `:reconnect`" message on give-up. Add server-side `:reconnect` / `:disconnect`
  ex-commands (work on the TUI too). Expose `nx.daemon.status()` + the event to Lua.
  Tested via the harness (inject a handle into `ServerInit`).
- **Phase 4 — `nxvim-line` component.** The colored statusline component in
  `~/work/nxvim-plugins/nxvim-line` (connected green / reconnecting yellow /
  disconnected red), redrawing on the status event.
- **Phase 5 — Reconnecting ssh/stdio link in GUI + TUI (DONE).** `connect_daemon_reconnecting`
  (a dedicated link thread driving the supervisor and awaiting it, so the editor dropping
  its handle reaps the ssh child) wired into the GUI `:connect user@host` + `--connect-daemon`
  stdio paths and the TUI `run_with_daemon` stdio path; the `ReconnectHandle` threads into
  `ServerInit.daemon_link`. The re-spawn factory holds the current child on the link thread
  (`kill_on_drop` reaps the previous on the next dial). **This fixes the user's
  sleep-disconnect scenario** (ssh hop drops → EOF → auto-retry re-spawns `ssh … nxvim
  --daemon` → seams rebind, local state intact). A hermetic wrapper test proves cross-thread
  auto-recovery through the real server tick.
- **Phase 6 — State re-sync + polish (DONE).** On a genuine reconnect (a
  `Reconnecting`/`Disconnected` → `Connected` transition, tracked via
  `EditHost::prev_daemon_status`), `on_daemon_status` fires `resync_after_reconnect` off the
  tick: it **re-opens LSP** for every bound buffer (`resync_lsp_after_reconnect` shuts the
  dead/phantom servers down and re-`ensure`s them from a cached `ServerSpawn` against the new
  connection — the remote children died with the link and the manager's own respawn hit the
  dead wire), **re-arms fs watches** (clear `remote_watches` → `sync_buffer_watches`), and
  **re-stats** by threading each buffer's disk baseline through `fs_watch [path, known?]` so the
  fresh daemon (which lost its own baselines) compares + pushes a change made *during* the
  outage — the unmodified buffer autoreloads, a locally-edited one is left as a conflict.
  Lost remote terminals are surfaced (`terminal_closed` + an echo; jobs already surfaced their
  `-1` exit on the drop). Also fixed: a re-dial now publishes the per-group cells *before*
  announcing `Connected`, so the resync issues onto live cells; and a **pre-existing** Phase 3
  regression — the un-gated `daemon_link: Option<ReconnectHandle>` broke the non-native
  (wasm edit-host) build — is gated. Tests: `daemon_reconnect.rs` proves an outage-window
  external change is detected + reloaded and that a conflicting local edit is not clobbered.
  Docs: `architecture.md` (the edit-host split section); example: `examples/daemon-status/`
  (a colored `nx.daemon.status()` statusline segment, verified end-to-end).
- **Phase 7 — QUIC 4-stream reconnect + web/wasm + keep-alive tuning.**
  - *QUIC 4-stream reconnect (DONE).* The supervisor (`maintain_link` / `reconnect_cycle`)
    is now generic over a **`DialedConnection` dialer** rather than a single-stream
    reader/writer factory: `split_single_stream` is the ssh/duplex adapter, and the QUIC
    dialer ([`connect_quic_reconnecting`]) opens the four leg-group streams per dial and
    holds the endpoint+connection in a slot (each re-dial replaces them, tearing the old
    link down). `connect_daemon_reconnecting{,_on}` funnel through the shared
    `connect_reconnecting_{thread,on}` cores. Both QUIC callers (TUI `run_with_daemon_quic`,
    GUI `:connect nxvim://…`) now thread the `ReconnectHandle` into `ServerInit.daemon_link`.
    Tested end-to-end over **real loopback QUIC** (`daemon_quic.rs`): a `:disconnect` →
    `:reconnect` re-dials a fresh connection (four new streams) to the same `serve_quic`
    listener and an outage-window external change autoreloads over the new Control stream.
  - *Keep-alive/idle (DONE).* Clarified that the reconnect supervisor — not the idle timeout
    — is the sleep/wake fix (a suspend far exceeds any sane idle timeout, so the link drops
    and re-dials on wake); `KEEP_ALIVE` (3 s) still prevents spurious drops of an
    actively-used-but-quiet link, and a modest `MAX_IDLE_TIMEOUT` (30 s) now means a dead
    link is *noticed and re-dialed* promptly rather than hanging.
  - *Web/wasm supervisor mirror (DONE).* The reconnect supervisor is mirrored in the
    browser Worker (`web/worker.mjs`): `startDaemonSupervisor` dials, then `serveDaemonLink`
    awaits the live link's death (a new `RpcClient.dead` promise resolved by `_fail`) and
    auto-retries with the same bounded backoff, parking `disconnected` on exhaustion. A
    `daemonReconnecting` flag forces the SAB run loop's non-blocking `Atomics.waitAsync` park
    so the backoff + WebTransport dial actually run while the link is down (a dropped link
    clears the live-state sets, so the loop would otherwise *block* the event loop and the
    reconnect would hang). Status rides `nx.daemon.status()` exactly like native, via a new
    `eh_daemon_status(phase, reconnected)` FFI that calls the now-**shared**
    `EditHost::apply_daemon_phase` → `resync_after_reconnect` (`resync_lsp_after_reconnect`
    and the resync orchestration were un-gated / moved to `lifecycle.rs` so both the native
    run loop and the wasm host drive them). The web **re-stat** is at full native parity: the
    daemon's `fs_read` reply stat is threaded through the web read path (`eh_fs_read_complete`
    → `complete_fs_read` → `load_replica_wasm` → `mark_replica_read_from_disk`) so a wasm
    buffer carries a real disk baseline, and the wasm `sync_buffer_watches` re-arm forwards it
    (`fs_watch [path, [secs,nanos,size]]`, via a `{path,stat}` arm shape in
    `eh_take_watch_requests` / `drainWatchRequests`). Tested over a **real** loopback `nxvim
    --daemon` + WebTransport in `web/verify-reconnect.mjs` (Playwright): a forced drop
    auto-recovers, the local buffer survives, a change made **during** the outage (before the
    re-dial re-arms) autoreloads — the fresh daemon compares the threaded baseline and pushes
    it — and a `:w` lands on the daemon's disk over the new wire. *(This also fixes a
    pre-existing web gap: `load_replica_wasm` stamped no disk baseline at all, so external-change
    precision improves beyond just reconnect.)*
  - *Vendored plugin copy.* The web demo's plugin bundle (`web/vendor/plugins`) and the
    `demo-site` build copies are **gitignored / regenerated** (`build-plugins.sh` +
    `package-site`), so they need no in-tree sync — a demo rebuild picks up the reconnect
    changes from `web/` automatically.
  - *(QUIC stayed one-shot through Phase 6 — the ssh path covered the reported case.)*

## Testing

Hermetic only (no real network/ssh; PTY e2e stays `#[ignore]`). Reuse the
`daemon_*.rs` pattern: spawn the daemon side over an in-process duplex and inject the
seam. Add a **redialable** test transport whose current daemon end can be dropped
(severing the link) and which the supervisor re-dials to a fresh daemon end. Assert:
editor stays alive, the local buffer survives, seam ops fail loud while down, and
remote ops succeed again after re-dial; status transitions fire in order; the retry
budget is bounded and give-up surfaces the `:reconnect` hint.

## Out of scope

Daemon-side session survival (terminals/jobs persisting across a drop). Re-attach by
session id. These were explicitly declined for now and would be a separate effort on
top of this.
