# Remote client over SSH

**Goal.** `nxvim-gui david@myserver.com:5022` connects to a remote host over SSH,
spawns an nxvim server *there*, and drives it from the local GUI window — the
editor (buffers, Lua, LSP, treesitter) runs on the remote; only the thin client
runs locally.

This is a small feature because the architecture already separates a headless
**server** from thin **clients** that talk msgpack-RPC over *any*
`AsyncRead`/`AsyncWrite` pair (`nxvim-rpc`). The embedded case wires that RPC
over an in-process `tokio::io::duplex`; the remote case wires the *same* RPC over
an `ssh` child process's stdin/stdout. Nothing in the editor or protocol changes.

## Decisions (confirmed with the requester)

- **Remote launch:** the **single `nxvim` binary** runs the server on the remote
  via a `--server` flag (`ssh … nxvim --server [file]`), assumed on the remote
  `PATH`. One binary plays both roles — TUI (default) and headless server
  (`--server`). Overridable via `$NXVIM_REMOTE_CMD` for non-standard installs.
  (An earlier draft shipped a separate `nxvim-server` binary; folded back into
  `nxvim` — fewer things to install, "server vs TUI" is just a flag.)
- **Scope:** SSH only. No generic TCP `--listen` mode yet (`nxvim --server` over
  stdio is the reusable core if we add one later).
- **Port:** `host:5022` is the **SSH port** (`ssh -p 5022`), matching "connect via
  ssh", not a port a server is already listening on.

## Pieces

1. **`nxvim --server` headless role** (`crates/nxvim/src/main.rs`,
   `nxvim_server::run_io`). With the `--server` flag the binary runs only the
   headless server, speaking RPC over **stdin/stdout** — exactly what `ssh` execs
   on the remote — with `ClipboardProvider::System` and `default_runtime()` (so it
   reads the *remote* host's config/runtimepath). `run_io(stdin, stdout, init)` is
   the server crate's read/write-half entry point (`run(stream, …)` is the
   single-stream convenience over it), so stdin+stdout need no `join`→`split`.

2. **GUI transport generalization** (`crates/nxvim-gui/src/lib.rs`). `run` took a
   pre-built stream; it now takes a **connector** `FnOnce() -> Future<Result<S>>`
   run *inside the IO-thread runtime*. This matters because tokio process pipes
   are bound to the runtime that polls them — the `ssh` child must be spawned on
   the same runtime that drives its stdio. A connect failure propagates back to
   the main thread (via the `Result<Rpc>` handoff) so no window opens on a failed
   SSH connection. The embedded caller passes `|| async { Ok(duplex_end) }`.

3. **SSH connector + CLI** (`crates/nxvim-gui/src/remote.rs`, `main.rs`). Parse a
   first positional matching `[user@]host[:port][/file]` (detected by a literal
   `@` that is not an existing local path) into a `RemoteSpec`; the file can be
   embedded after the host or given as a second positional. Build `ssh [-p PORT]
   -- [USER@]HOST nxvim --server [FILE]`, spawn with piped stdin/stdout (stderr
   inherited so ssh's own diagnostics reach the user), and hand back an
   `SshTransport` that owns the child (`kill_on_drop`, so closing the window tears
   down the remote) and delegates `AsyncRead`/`AsyncWrite` to `join(child.stdout,
   child.stdin)`. **Hardening:** `parse_target` rejects a `-`-leading `user`/`host`
   (no `ssh` flag smuggling like `-oProxyCommand=…`), the remote command + file
   are POSIX shell-quoted before ssh's remote shell sees them (no metacharacter
   injection from a crafted path), and `--` terminates ssh's own option parsing.

4. **Live `:connect`** (`crates/nxvim-gui/src/lib.rs`). `:connect
   [user@]host[:port][/file]` from a running window switches servers without
   recreating it: the client intercepts it on `<CR>` (the current server knows
   nothing of `:connect`), the IO thread's session loop tears the current
   connection down and brings the new one up, and the App swaps its live `Rpc`
   handle and re-attaches the UI on a `Connected` event.

5. **Interactive auth via `SSH_ASKPASS`** (`crates/nxvim-gui/src/remote.rs`,
   `main.rs`). ssh reads passwords/passphrases and host-key confirmation from the
   controlling terminal, so a desktop launch (no tty) couldn't authenticate or
   accept a new host key. `connect` points `$SSH_ASKPASS` at *this* binary (with
   `SSH_ASKPASS_REQUIRE=force`, OpenSSH 8.4+, and a marker env var); when ssh
   re-invokes it for a prompt, `run_askpass_if_invoked` pops a native dialog
   instead of starting the editor — a Yes/No for host-key acceptance, a masked
   input for secrets — and writes the answer to stdout. The dialogs shell out to
   the platform's prompt tool (macOS `osascript`; Linux `zenity`/`kdialog`;
   Windows PowerShell + WinForms), so the askpass process needs no GPU/window. A
   cancelled dialog exits non-zero, which ssh treats as abort. Key-agent auth with
   a known host needs no prompt, so nothing pops in the smooth case.

## Testing

- **`nxvim --server` stdio role** — black-box, end-to-end: spawn the *real*
  compiled binary (`CARGO_BIN_EXE_nxvim`) with `--server` and piped stdio, drive
  it with the shared harness helpers over `nxvim_rpc::connect`, assert on lines
  (`crates/nxvim/tests/stdio_server.rs`). Exercises the exact transport `ssh`
  uses, minus the network hop.
- **SSH target parsing + hardening** — `crates/nxvim-gui/tests/remote.rs` covers
  `[user@]host[:port][/file]`, the `:connect` command, the `-`-leading rejection
  that blocks ssh-flag injection, and the askpass prompt classifier.
- The **SSH hop itself** is not covered by automated tests (no remote host / ssh
  in CI); the stdio-binary test covers the mechanism, and the GUI window + askpass
  dialogs can't be asserted headlessly (see the GUI-screencapture limitation). The
  ssh-argv build is kept trivial and obviously correct. The askpass dialogs are
  built only on macOS in local dev (the Linux/Windows branches are `cfg`'d out of
  a macOS build, so they're eyeballed, not compiled here — CI compiles them).

## Known limitations (v1)

- **Clipboard is the remote's.** `"+`/`"*` use the remote host's clipboard tool;
  yank-on-remote does not reach the local OS clipboard. (A future RPC-proxied
  clipboard, à la OSC52, could fix this.)
- **No protocol negotiation.** Local client and remote server must be the same
  nxvim build; a mismatch surfaces as a dropped/garbled connection, not a clean
  version error.
- **Remote files only via the picker's absence.** A directory argument's local
  file-picker behavior does not apply to the remote; pass a file path explicitly.
