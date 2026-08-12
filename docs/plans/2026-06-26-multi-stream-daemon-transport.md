# Multi-stream QUIC/WebTransport for the daemon transport

**Status:** ✅ complete (all 4 phases — native + browser multi-stream + docs) · **Date:** 2026-06-26

The native QUIC connector and the browser WebTransport connector both carry every
daemon leg over **one** shared bidi stream, demuxed by method namespace. The
`bemtvi-rpc` writer prioritises control over bulk, but both lanes still serialise onto
that one stream, so a `proc`/`term`/`lsp` flood (a fuzzy-finder's `rg`, an
`npm install`, an LSP `semanticTokens` dump) can head-of-line-block an `fs_write` save
queued behind it **at the QUIC layer** — app-level framing can't escape bytes already
committed to one stream. This is the unfinished follow-up the `quic.rs` module doc and
`docs/edit-host-split.md` ("QUIC gives each traffic class its own stream") both promise.

This plan splits the legs across **four** independently flow-controlled streams, grouped
by latency class, so the three flood/streaming sources are each isolated from the
latency-critical control traffic and from each other.

## The grouping (4 streams)

Each group is one bidi stream. A stream's identity is fixed by a **one-byte group tag**
the *client* writes as the very first byte of the stream (before any msgpack frame); the
daemon reads that byte off the raw `RecvStream` and dispatches the rest of the stream to
the group's legs. The tag byte is also what makes a freshly-opened bidi stream *visible*
to the peer's `accept_bi` promptly (QUIC doesn't surface an opened stream until its first
STREAM frame). Both directions of a bidi stream belong to the group — the client knows
the group because it opened the stream, so the daemon→client direction needs no tag.

| tag | group | legs | why grouped |
| --- | --- | --- | --- |
| `0` | **Control** | `fs_*`, `config_*`, `luafs_op`, `luafs_watch` | latency-critical saves/reads + one-shot config + low-volume `btv.fs` ops/pushes |
| `1` | **Proc** | `proc_*` | run-to-completion floods (`rg`, `npm install`) |
| `2` | **Lsp** | `lsp_*` | long-lived bidi pipe, bursty (`didChange`, large responses) |
| `3` | **Term** | `term_*` | continuous high-volume PTY output (browser-only sender) |

**Live legs differ by client.** Native sends only `fs` / `proc` / `lsp` / `luafs_op` /
`config`; the browser adds `term` and `luafs_watch`. The daemon is **tag-driven**, not
count-driven: it dispatches each stream as it arrives over the connection's lifetime and
never waits for a fixed number, so a client opens exactly the streams its live legs need
(native opens Control/Proc/Lsp; browser opens all four). An unrecognised tag is a **loud**
error (no silent stub), not a dropped stream.

## What stays single-stream (unchanged)

The **ssh/stdio** daemon (`bemtvi --daemon` over `BEMTVI_DAEMON_CMD`) and every in-process
`tokio::io::duplex` **test** have exactly one ordered byte pipe — there is no second
stream to open. They keep today's single-stream multiplexer verbatim
(`run_daemon_io` server-side, `serve_daemon_link` client-side). Multi-stream is a
property of the QUIC/WebTransport transports only; the leg handlers are identical either
way because they already take a generic `(Rpc, incoming)` pair.

## Architecture

The refactor concentrates in the orchestration layer; the per-leg `serve_*_daemon_on`
handlers and the client seams are reused untouched.

### Shared group model (new)

A small `LegGroup` enum (new `daemon` submodule or top of `daemon.rs`):

```rust
enum LegGroup { Control, Proc, Lsp, Term }
impl LegGroup {
    fn tag(self) -> u8;                 // 0..=3
    fn from_tag(b: u8) -> Result<Self>; // loud error on unknown tag
    fn owns(self, method: &str) -> bool;// the method→group routing table
    fn client_groups() -> &[LegGroup];  // which streams a NATIVE client opens
}
```

`owns` is the single source of truth for routing — the same table the single-stream
demux uses, partitioned by group.

### Server side (`quic.rs` + `lib.rs`)

- Factor today's per-leg spawn block out of `run_daemon_io` into
  `spawn_group_legs(group, rpc) -> (GroupSenders, Vec<JoinHandle>)` and the
  method→sender routing into a helper keyed by `LegGroup::owns`.
- `run_daemon_io(reader, writer)` (single-stream, stdio/tests) is unchanged behaviour:
  spawn **all** groups' legs over one `Rpc`, route the one `incoming` to the union of
  senders. Implemented in terms of the new helpers so it can't drift.
- New `run_daemon_group(recv, send, group)` (multi-stream): read nothing (tag already
  consumed by the caller), `connect(recv, send)` for this stream's own `Rpc`,
  `spawn_group_legs`, route this stream's `incoming` to just that group's senders.
- `serve_one` (QUIC): after auth, loop `accept_bi()`; for each accepted stream read the
  one tag byte, `LegGroup::from_tag`, and spawn `run_daemon_group` on its own task. The
  connection's lifetime bounds the session; when it drops, every stream EOFs and each
  group winds down + reaps its children. A bad tag logs loud and drops that stream only.

### Client side (`quic.rs` + `daemon.rs`)

- `connect_quic` / `quic_dial`: open one bidi stream per `LegGroup::client_groups()`,
  write the tag byte, `connect()` each → a per-group `(Rpc, incoming)`. Signal dial-ok
  only once all are initialised.
- Refactor `serve_daemon_link` to take per-group channels (a small struct) and build each
  seam off **its group's** `Rpc`:
  - `host_fs` / `fs_jobs` / `config` → Control `Rpc`; `run_fs_jobs` over Control.
  - `host_proc` → Proc `Rpc`.
  - `lsp_transport` → Lsp `Rpc`.
  - `fs_watch` (the streaming `btv.fs.watch` seam, `RemoteFsWatch`) → Control `Rpc`; its
    `luafs_change`/`luafs_watch_err` pushes land in the Control demux. (Browser-only when
    this was written; the native client took the leg later, so a daemon session watches the
    daemon's disk rather than its own.)
  - (`term` seam: browser-only — N/A for the native client.)
- Split `run_client_demux` into a per-group demux (each group's `incoming` carries only
  its own notifications): Control→`fs_changed`→`watch_tx`; Proc→`proc_*`→inflight;
  Lsp→`lsp_*`→inflight. The single-stream path keeps one front demux that splits the
  shared `incoming` into the per-group demuxes by `LegGroup::owns`, so both transports run
  the *same* per-group demux code.

### Browser side (`crates/bemtvi-edithost/web/`)

- `rpc.mjs`: today's `RpcClient` opens one bidi (`createBidirectionalStream()`, L136) and
  decodes with `decodeMultiStream`. Generalise to open the four group streams, write each
  group's tag byte first, keep one reader/writer loop per stream, and expose a
  `notify(method, params)` that picks the stream by a JS mirror of the `owns` table.
- `worker.mjs`: `applyDaemonNotifications` (L1139) already routes inbound by method, so it
  needs no change beyond reading from whichever stream delivered the notification (the
  group is implied; method routing is unchanged). Outbound `daemon.notify(...)` /
  `daemon.request(...)` calls pick their stream via the same table.
- Keep the tag table identical to the Rust `LegGroup` (a single documented constant on
  each side; they must not drift).

## Phases (commit + pause between each)

1. **Group model + behaviour-preserving server refactor.** Add `LegGroup`; factor the
   per-leg spawn + method routing into a `DaemonLegs` helper; re-express today's
   `run_daemon_io` (single-stream, stdio/tests) through it with **no behaviour change**;
   add `run_daemon_group` (the per-stream multi path) as new, not-yet-wired code. The tag
   handshake is **not** flipped here — `serve_one` still runs single-stream — so existing
   clients and every leg test stay green. `cargo test --workspace` green.
2. **Native multi-stream (server + client together).** Flip the handshake atomically:
   `serve_one` accepts tagged streams → `run_daemon_group`; `connect_quic` opens the
   tagged client streams; per-group `serve_daemon_link`; per-group client demux. New
   `daemon_quic` tests: every native leg (fs read/write, proc spawn, lsp round-trip,
   luafs_op, config) works over the split, and an HOL-isolation test (a proc flood does
   not delay an `fs_write` ack).
3. **Browser parity.** Multi-stream `rpc.mjs`/`worker.mjs`; rebuild wasm (`~/emsdk`),
   run the `verify-*.mjs` harness (Playwright) to confirm fs/proc/lsp/term all work over
   the split from the browser edit-host.
4. **Docs + loud-handshake polish.** Update the `quic.rs` module doc (drop "still one
   stream"), `docs/edit-host-split.md`, `docs/architecture.md`; verify the unknown-tag and
   short-read handshake paths fail loud per the no-silent-stubs rule.

## Risks / notes

- **Protocol is in-repo, both sides update together** — no cross-version negotiation
  needed (the peer is the same build); the tag handshake fails loud on mismatch.
- **Idle streams are cheap** but the native client simply doesn't open Term, so there's
  no idle PTY stream to pay for.
- **Auth is per-connection** (bearer token at session accept), so all four streams inherit
  it; no per-stream auth.
- **`bemtvi-rpc` priority lanes still apply per stream** — control-vs-bulk prioritisation
  now matters mainly *within* a group (e.g. Lsp stdin vs a stdout flood).
