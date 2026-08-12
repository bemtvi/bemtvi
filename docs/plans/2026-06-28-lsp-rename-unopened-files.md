# LSP rename across unopened files

**Goal.** A project-wide LSP rename (and any `WorkspaceEdit` — code actions too) must
reach files that aren't currently open in a buffer, applying each file's edits in
memory (buffer left modified, persisted by `:wa`), exactly as neovim's
`vim.lsp.util.apply_text_edits` does. This must work in **both** the native (TUI/GUI)
session and the **daemon / web (off-tick)** session, where an unopened file's bytes
live across the wire.

## Background — where edits land

`EditHost::apply_workspace_edit` (`crates/bemtvi-server/src/lsp/edit.rs`) is the single
applicator every `WorkspaceEdit` flows through (rename, code action, the Lua
`vim.lsp.util.apply_workspace_edit`). For each `(uri, edits)` it resolves the URI to a
buffer — the open buffer it names, else it loads the file — converts each edit's LSP
range to bytes against *that* buffer, and applies via `Editor::apply_edits_to(id, …)`
(one independent undo step per buffer).

The load bridge is `Editor::ensure_buffer_loaded(path)`
(`crates/bemtvi-core/src/editor/buffers.rs`).

## Phase 1 — native (DONE)

**Bug.** The default config runs `btv.explorer.enable()`, which registers a `BufReadCmd`
handler, so `should_defer_open()` is true in essentially every native session. The old
`ensure_buffer_loaded` routed through `open_buffer` → `load_new_buffer`, which therefore
**deferred**: it created an *empty* named buffer and enqueued an async disk fill. The
workspace edit applied to the empty buffer, then the deferred fill clobbered it with the
original (un-renamed) bytes. Net result: cross-file rename silently did nothing to
unopened files for every native user.

**Fix.** `ensure_buffer_loaded` now reads the file **synchronously**, bypassing the
open-deferral. A workspace edit needs the bytes *now* and is not a user `:edit`, so it
must not give a `BufReadCmd` handler first dibs (the explorer only ever claims
*directories* anyway). Off-tick still returns `None` (genuinely async — Phase 2).

Test: `crates/bemtvi/tests/lsp_features.rs::rename_reaches_an_unopened_file`.

## Phase 2 — daemon / web off-tick

Off-tick, `ensure_buffer_loaded` returns `None` (the file's bytes cross the wire), so
those files are reported "could not open" — loud, but unhandled. Close the gap with a
**deferred apply**, mirroring the existing off-tick open pipeline
(`enqueue_open` → `fs_fetch` → `apply_open` → `load_replica_bytes`/`load_replica_wasm`).

1. **Core** — add `Editor::enqueue_replica_open(path) -> BufferId`: reuse an already-open
   buffer, else create the empty named replica buffer and `enqueue_open` its fetch,
   returning the id. (The off-tick sibling of `ensure_buffer_loaded`.)

2. **Server state** — `EditHost.pending_lsp_edits: HashMap<BufferId, PendingReplicaEdit>`
   where `PendingReplicaEdit { edits: Vec<TextEdit>, encoding: PositionEncoding }`. The
   edits are kept in LSP form + the originating server's encoding, converted to bytes
   only once the buffer's real contents have landed.

3. **`apply_workspace_edit`** — for an unopened URI in an off-tick session: call
   `enqueue_replica_open`, stash the edits keyed by the new buffer id, and move on
   (don't apply now — the buffer is still empty). Count these as *deferred* so the
   "No applicable changes" message isn't shown when some edits are merely pending.

4. **Apply on landing** — `apply_pending_replica_edit(buffer)`: pop the stashed edits,
   convert their ranges to bytes against the now-filled buffer, `apply_edits_to`, and
   `sync_lsp_buffer`. Call it from both landing sites — `load_replica_bytes` (native
   daemon) and `load_replica_wasm` (wasm/OPFS) — after the bytes are loaded and the
   disk baseline stamped, before lifecycle events fire (so a `BufReadPost`-driven LSP
   attach sees the renamed text). Clear any stranded entry in `apply_open`'s error arm.

Test: a native server given an async daemon fs (so `host_fs_offtick` is on), driving
`btv._lsp_apply_workspace_edit` with edits for an open file *and* an unopened file the
daemon fs serves; assert the unopened file's buffer carries the edit once its fetch
lands. Faithful because the unopened file's bytes can only come across the wire.
