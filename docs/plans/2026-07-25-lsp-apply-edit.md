# `workspace/applyEdit` — server-initiated workspace edits

**Status:** Done (eleven phases). Verified against real gopls, over the daemon wire, and
against a real daemon's filesystem from the browser.

## The bug

Running gopls's *"Extract declarations to new file"* code action fails:

```
btv.lsp: 'gopls.extract_to_new_file' failed: No such method workspace/applyEdit (jsonrpc error -32601)
```

The action is a bare `command`, so the editor dispatches `workspace/executeCommand`; the
server then does the actual work by asking the *client* to apply a `WorkspaceEdit` —
`workspace/applyEdit`, a **server→client request**. bemtvi answers every unmodelled
server→client request with method-not-found (native, via `async-lsp`'s router default) or
with a bare `null` (wasm `SyncLspClient`, which is worse: the server thinks it succeeded
and the edit is silently dropped). Either way the refactor never lands.

This is the whole apply half of the protocol: `executeCommand`-style refactors in gopls,
rust-analyzer, ts_ls, jdtls and pyright all deliver their edits this way, not as a code
action's `edit`.

Probed against real gopls (`scratchpad/probe.py`), `gopls.extract_to_new_file` sends:

```jsonc
{"method": "workspace/applyEdit", "params": {"edit": {"documentChanges": [
  {"textDocument": {"uri": ".../main.go", "version": 1}, "edits": [ … ]},   // cut
  {"kind": "create", "uri": ".../helper.go"},                               // resource op
  {"textDocument": {"uri": ".../helper.go", "version": 0}, "edits": [ … ]}  // paste
]}}}
```

so the fix needs both the request plumbing *and* ordered resource operations —
`normalize_workspace_edit` currently drops `DocumentChangeOperation::Op(_)` on the floor
and flattens the rest into an unordered `(Url, Vec<TextEdit>)` list.

## Phase 1 — the request, ordered changes, and `create`

1. **`WorkspaceEditData` becomes an ordered change list.** `Vec<(Url, Vec<TextEdit>)>` →
   `Vec<WorkspaceChange>` with `Edits` / `Create` / `Rename` / `Delete` variants, so
   `documentChanges` keeps the order the protocol mandates (create *then* fill).
2. **`LspEvent::ApplyEdit`** carries the normalized changes plus a per-client `id`, and the
   editor answers through a new `HostEffects::lsp_apply_edit_response(key, id, outcome)` —
   symmetric across the two clients:
   - **native** (`client.rs`): the router handler parks on a `oneshot` kept in
     `ClientState`; the manager resolves it via `socket.emit` (async-lsp's custom-event
     channel) so the response is the *real* applied flag, not an optimistic ack. The
     `MainLoop` drives request futures in a `FuturesUnordered` alongside its incoming arm,
     so parking one cannot stall the loop.
   - **wasm** (`sync_client.rs`): the pending `(key, req_id)` is stashed and the response
     framed onto the wire when the editor answers.
3. **`create`** makes an empty buffer for the path (`Editor::create_file_buffer` — no read,
   so no off-tick fetch to race the edits that follow), which the edits after it fill.
   `overwrite` / `ignoreIfExists` are honored against an existing file. Phase 3 below then
   writes it out.
4. **Capabilities**: advertise `workspace.applyEdit = true` and
   `workspaceEdit.resourceOperations = [create, rename, delete]` — a server may gate the
   refactor on them (gopls offers it regardless, jdtls does not).
5. **`rename` / `delete`** were refused loud in Phase 1 (echoed, and reported back as
   `applied: false` with a reason) since neither has an in-memory analogue; Phase 2 below
   implements them.

One wrinkle the create path turned up: bemtvi's rope always carries a trailing phantom
newline, so a "created" buffer is `"\n"` where the server's document model is `""`. Left
alone, the paste inserts *before* the phantom and the new file ends with a spurious blank
line. The fill's last edit consumes the phantom instead (`apply_workspace_edit`, guarded to
buffers this same edit created).

Tests: the scripted mock gains an `apply_edit` field that fires the request from
`workspace/executeCommand` (gopls's exact shape) and records the client's response, so a
test asserts the buffer changes, the created buffer exists with the pasted text, *and* that
we answered `{"applied": true}`. Covered natively (`lsp_features.rs`), over the daemon
(`daemon_lsp.rs`) and in the browser (`verify-lsp.mjs`) — the remote session is tier-1.
Each was mutation-checked rather than trusted for passing: dropping the native router
handler fails the native test, hard-coding `applied: true` fails the refusal test, and
renaming the sync client's `workspace/applyEdit` arm fails both browser checks (with
`_apply_edit_response: null` — precisely the silent ack this replaces).

Verified end to end against **real gopls**: "Extract declarations to new file" on a Go
buffer cuts the declaration out of `main.go` and leaves `helper.go` holding exactly
`package main\n\nfunc helper() string {\n\treturn "hi"\n}\n` — no trailing blank line.

## Phase 2 — `rename` / `delete`

Both move real bytes, which must work identically local / native-daemon / browser, so they
ride an **editor-owned off-tick fs job**: the same `FsJob` seam `btv.fs` uses (native-bare →
the event-loop actor, native-daemon + wasm → the `luafs_op` leg, serverless → OPFS), queued
through the ordinary `LoopOp::Fs` path under an id from `WORKSPACE_FS_JOB_BASE`. The two
landing sites (native `LoopEvent::FsResult`, wasm `EditHost::fs_op_result`) classify by id
and hand an editor-owned job to `on_workspace_fs_result` instead of a Lua promise — the
same trick `INTERNAL_WATCH_BASE` already uses for the per-buffer watches, so no new
transport, wire message or JS worker code was needed on any of the three legs.

The buffer half runs when the result lands: a `rename` rebinds the buffer to the new path
(content, modified state and undo history intact), a `delete` wipes it. The
`workspace/applyEdit` response is held back until the last operation settles
(`PendingApplyEdit`, counting down), so `applied` still describes what actually happened.
`ignoreIfExists` on a rename is honored by *probing* the destination first (a chained
`exists` job) rather than guessing — the seam's rename clobbers like `rename(2)`, and
nothing on the editor tick can see a daemon's filesystem.

A renamed buffer's LSP document is closed and re-opened under the new URI **without**
detaching its servers (`reopen_lsp_document`): the buffer is the same buffer on the same
servers, but a different document. Dropping the state instead — the first thing that
looked right — silently left the buffer server-less, since `FileType` doesn't fire again
when only the stem changed. The browser leg caught it; there is now a native regression
test asserting the didClose/didOpen pair and the surviving attachment.

## Phase 3 — the created file lands on disk, under the name you'd have typed

Two follow-ups from using it:

1. **`create` puts the file on disk.** Leaving the file itself uncreated means the
   on-disk project is half-refactored, so the `create` resource operation writes it out.
   **Superseded by Phase 11**, which settled *what* is written: an empty placeholder, with
   the extracted content left unsaved in the buffer (neovim's model). Phase 3 wrote the
   content too, as a deliberate deviation; Phase 11 reverts that, so the deviation is gone
   and `Editor::queue_buffer_write` with it. Phase 5 puts a recursive `mkdir` of the file's
   directory in front of the write either way.
2. **Buffer names are cwd-relative again.** A workspace edit only ever has absolute URIs,
   so every file it created or opened was *named* absolutely while its neighbours were
   short. `EditHost::buffer_path_for` stores the path relative to the session's effective
   directory when it lives under it — the name `:e <file>` would have given it — while the
   filesystem side of the same operation keeps the absolute path (it may run on a daemon,
   where a relative path resolves against the daemon's launch dir).

That second change surfaced a related bug and fixed it at the seam: a buffer's path was
turned into its LSP URI with `absolutize`, i.e. against *this process's* cwd. Correct
locally (the process cwd tracks the effective dir) but wrong in a daemon session, where a
relatively-named buffer — `:e src/main.rs` has always stored the relative form — addressed
a local path the server had never heard of. `EditHost::buffer_uri` / `abs_buffer_path` now
resolve against `DirState`, and `path_to_uri` is gone.

## Phase 4 — ordering, and the failure-handling strategy we can actually promise

Asking "what about `transactional`?" turned up a real bug in Phase 2: the file operations
were queued as independent `LoopOp::Fs` jobs, each `tokio::spawn`ed, so they ran
**concurrently**. `documentChanges` is a sequence — `rename a→b` followed by `rename b→c`
is nonsense the other way round — and the race is decided, not theoretical, the moment one
operation costs an extra round trip (an `ignoreIfExists` probe, or any daemon-session
latency). They now run **one at a time, in order**: `queue_workspace_fs_job` appends to a
FIFO with at most one in flight, and each landing starts the next.

That is also what makes a failure strategy implementable, so bemtvi now declares one:

- **`failureHandling: abort`.** When a change fails, the ones before it stay applied and
  the ones after it are dropped (`drop_workspace_group`), and the response carries
  `failedChange` — an index that *means* something to the server precisely because a
  strategy is declared. The user gets the same story echoed, including how many later
  changes were skipped.
- **Text edits resolve before any of them apply.** Every document is staged against its
  buffer first; a URI that resolves to nothing aborts the edit with *nothing* applied (and
  drops the queued file operations, none of which have started, plus any buffer a `create`
  had made). For the common failure this is stronger than `abort` promises.
- **Not `transactional`.** A `delete` cannot be rolled back without a backup copy, and a
  half-applied chain of moves cannot be guaranteed reversible, so promising all-or-nothing
  across resource operations would be a lie a server may act on. `textOnlyTransactional`
  is nearly true — the staging above gets us there locally — but an off-tick session's
  deferred replica fetch can still fail *after* the synchronous edits landed, so the
  weaker, exactly-true claim is the honest one.

## Phase 5 — the two ordering holes a review of Phases 1–4 turned up

Both are cases where a change is applied against the filesystem *as it was*, not as the
edit's earlier changes leave it.

1. **Edits addressed to a file an earlier `rename` moves.** `documentChanges` may say
   `rename a → b` and then edit `b` (rust-analyzer's move-module shape; the protocol
   allows it anywhere). The rename only *runs* off-tick, after every text edit is staged,
   so `b` didn't exist yet when the edits resolved: they opened a **fresh empty buffer**
   for a file that wasn't there, applied into it, and the rename then bound a *second*
   buffer to the same name — two `moved.rs` buffers and the edit silently lost. Each
   document's URI is now rewound through the renames this same edit has queued
   (`rewind_pending_renames`, transitively for `a → b → c`, bounded so a cyclic chain
   terminates), so the edits reach the buffer that holds the file *now* and the rename
   rebinds that same buffer when it lands. The buffer a `rename`/`delete` acts on is
   likewise resolved **when the operation lands**, not when it was queued — this edit's
   own text-edit half may have opened it in between.
2. **`create` into a directory that doesn't exist yet.** `:w` refuses to create one
   (vim's `E212`, rightly), so the created buffer's write failed *after* the edit had
   already answered `applied: true` — the file silently never appeared. The write is now
   queued behind a recursive `mkdir` of the file's directory on the same ordered fs seam
   (`WorkspaceFsOp::CreateDir`), which is also why it works identically local / daemon /
   browser. `recursive` ⇒ an existing directory is a success, so the common
   same-directory create needs no special case; the response waits for the directory
   (not for the write, which still reports itself loud like any `:w`).

## Phase 6 — nobody waits forever, and the edit is reachable from Lua

1. **A watchdog over the file operations.** `workspace/applyEdit` is a *request*: the
   server is blocked until bemtvi answers, and Phase 2 made that answer wait for the
   `rename`/`delete`/`mkdir` to land. An fs leg that stops answering rather than
   erroring (a daemon link that goes quiet) therefore blocked the server *forever*.
   A one-shot timer — re-armed per dispatched operation, disarmed when the queue
   drains — fails the stalled operation through the ordinary result path, so it takes
   the same `abort` route a real error does. The reason is hedged (`ETIMEDOUT … it may
   still complete`) because giving up is not proof the operation failed. Default 30s,
   `$BEMTVI_WORKSPACE_FS_TIMEOUT_MS` for the test that drives the give-up path (a real
   stall: an `btv.fs` job leg pointed at a duplex nobody serves).
   Two smaller holes closed with it: a late result for an abandoned job is now
   swallowed by id (`on_workspace_fs_result` classifies on `WORKSPACE_FS_JOB_BASE`
   alone, as its doc always claimed) rather than handed to a Lua callback that never
   existed, and `WORKSPACE_FS_JOB_BASE` no longer shares its number with
   `PARSE_RESUME_TIMER_ID` — harmless while the two id spaces never met, which is
   exactly what a watchdog timer over these jobs would have ended. A server that exits
   mid-apply also drops its held-back response now, instead of leaving a record that
   can only be settled into the void.
2. **The Lua entry existed but had no caller.** `btv._lsp_apply_workspace_edit` /
   `btv._lsp_show_document` were wired end to end from Phase 7 of the LSP plan, yet
   `vim.lsp.util` was never defined — so the example in `btv.lsp.commands`'s own
   documentation (`vim.lsp.util.show_document(loc)`) errored on a nil table. Both are
   now public as `btv.lsp.apply_workspace_edit` / `btv.lsp.show_document`, with the
   `vim.lsp.util.*` spellings aliased onto them. Resource operations work through this
   path exactly as through a server reply, which is what the new test asserts.

## Phase 7 — the off-tick holes: a `create` that clobbered, and edits that raced a fetch

Both are the same mistake in two places: treating "the editor tick cannot see the
filesystem" as "there is nothing there".

1. **`create` with `ignoreIfExists`, off-tick.** `ensure_buffer_loaded` returns `None`
   in a daemon / browser session (it cannot read synchronously), and the create fell
   through to "make it empty" — then Phase 3 wrote that empty version out, over the
   very file the server asked to spare. The probe it needed already exists: the replica
   fetch. Off-tick the create now enqueues one (`enqueue_replica_open`) and the
   *landing* answers the question — `existed` ⇒ leave it alone, its real content is in
   the buffer with the edits on top; absent ⇒ this was a create after all, so the file
   appears exactly as the synchronous path makes one (`settle_workspace_create`).
   A fetch that *fails* writes nothing: not knowing is not a licence to overwrite.
   The same knowledge fixed the local path's own inconsistency — an `ignoreIfExists`
   create over a file that turns out not to exist is a create, and lands on disk.
2. **Edits racing an in-flight fetch.** Only the change that *started* a replica fetch
   deferred its edits; a later change naming the same document found the buffer by path
   and applied inline — into a still-empty buffer the landing then overwrote, losing
   the edit. Deferral is now decided by the buffer, not by which branch created it
   (`Editor::has_pending_open`), so it covers a second change for one document, the
   `create` probe above, and an open that was already in flight when the edit arrived.

## Phase 8 — change annotations, and a `create` that names a directory

1. **`changeAnnotations`.** A server can split one edit into named groups and mark some
   of them `needsConfirmation` — the point being that the safe half of a refactor
   applies while the half you should look at asks first. bemtvi now carries them
   (`WorkspaceEditData` gained the annotation map; every `WorkspaceChange` carries the
   id it was tagged with, and a document's edits split into runs when they are tagged
   differently, so a declined group takes only its own edits), advertises
   `changeAnnotationSupport.groupsOnLabel`, and asks: one `btv.ui.confirm` per distinct
   **label**, driven from Lua (`btv.lsp._confirm_edit`) because the confirm UI, its keys
   and its rendering already live there and work identically in every build. Nothing of
   the edit applies while the question is open, and a server-initiated
   `workspace/applyEdit` waits with it — the server asked whether its edit was applied,
   and until the user says, it hasn't been. Declining everything answers `applied:
   false` with the reason; declining part of it applies the rest and says so.
   Lua that can't be reached declines rather than parking the edit forever.

   One thing had to be fixed underneath: **`lsp-types` drops a text edit's
   `annotationId`.** Its `OneOf<TextEdit, AnnotatedTextEdit>` is `#[serde(untagged)]`
   and `TextEdit` accepts unknown fields, so an annotated edit deserializes as a plain
   one *before* any bemtvi code sees it — the confirm gate would have been dead on
   arrival, silently applying exactly the changes a server wanted a human to see. So
   normalization now runs on the **raw JSON** (`normalize_workspace_edit_value`), and
   every path that carries a `WorkspaceEdit` was rewired to keep it: the inbound
   `workspace/applyEdit` (a raw-params request type on the native router; the wasm
   client already had the value), the `rename` / `codeAction` / `codeAction/resolve`
   replies (raw-result request types), and the Lua entry. Resource-op annotations were
   never affected — those are typed fields.
2. **A `create` whose URI ends in `/` makes a directory**, with its parents, on the
   same ordered fs seam as the rest — so a scaffolding refactor can lay out a package
   and then fill it. (A file `create` already brings its own recursive `mkdir`, which
   is why the test for this uses a directory nothing else in the edit touches: anything
   else would pass without the feature.)

## Phase 9 — what a review of Phases 1–8 turned up

Five of them. The first three are one mistake in three sets of clothes — telling a
**blocked server** something we hadn't established: `applied: true` for an edit that
was declined, for one we couldn't even read, and a `failedChange` index in a numbering
the server doesn't use.

1. **A confirm that can't be asked answered `applied: true`.** When
   `run_lsp_confirm_edit` fails, `ask_before_applying` declines the edit through
   `on_workspace_edit_decision` — but that runs *inside* the apply, before
   `on_apply_edit` has recorded the response to fold an outcome into, so both the
   decline and the file operations the surviving changes queued were dropped on the
   floor and the held-back response settled to the unconditional "applied". The
   decision now *returns* its `AppliedEdit` and the caller carries it out. (The
   normal path is unaffected: there the record exists by the time the answer lands.)
2. **A malformed edit was `applied: true` natively and `applied: false` on wasm.** The
   native router registered `workspace/applyEdit` with a raw params type and no
   validation, and `normalize_workspace_edit_value` degrades an unparseable edit to an
   *empty* one — indistinguishable from "the server sent no changes", which reports
   success. Normalization now has an error channel
   (`try_normalize_workspace_edit_value`); both clients refuse the same way, the Lua
   entry uses it, and the `rename` / `codeAction/resolve` reply paths log the WARN
   they lost when they moved to the raw shape.
3. **`failedChange` was indexed against the wrong list.** `apply_workspace_changes`
   numbered the changes it was handed, so a confirmation that declined a change in
   front of the failure shifted the index — and that index is exactly what a server
   acts on, `abort` being declared. Changes now carry their position in the
   `documentChanges` the *server* sent, assigned before any filtering.
4. **An aborted edit still wrote the file its `create` probe was checking.** An
   off-tick `ignoreIfExists` probe is a replica fetch, not one of the queued file
   operations, so `drop_workspace_group` didn't close that door: the probe landed into
   an abandoned apply and `settle_workspace_create` wrote the file out. The one abort
   path that cleaned up (the resolve-time one) did it inline; the cleanup moved into
   `drop_workspace_group`, where both paths get it.
5. **`btv.lsp.apply_workspace_edit` read columns at an encoding it didn't document.** It
   used the *current* buffer's first server's encoding (utf-8 when there is none) while
   the docstring promised the protocol's utf-16, so a utf-16 edit on a line with any
   multi-byte character landed wrong — and `vim.lsp.util.apply_workspace_edit(edit,
   encoding)` accepted and discarded the encoding neovim honors. It now takes
   `opts.encoding`, defaulting to utf-16 like its sibling `btv.lsp.show_document`, and
   the `vim.lsp.util` alias passes its positional argument through.

Plus the remaining "waits forever" hole on the other side of Phase 6's watchdog: the
confirm chain only answered on the happy path, so a rejected `btv.ui.confirm` parked the
edit — and the server blocked on it — indefinitely. `btv.lsp._confirm_edit` now answers
once, whatever happens to the chain.

And one piece of dead weight: Phase 8 moved every edit-carrying path to the raw shape,
which left the typed `normalize_workspace_edit` with no real caller — `code_action`
still ran it, but `code_actions_value` overwrote the result from the raw value on the
next line. The typed normalizer is gone and `code_action` leaves `edit: None` for its
caller to fill (it only ever needed to know *whether* there was one, to decide
`resolve`).

Each fix is covered by a test that was mutation-checked against the unfixed code, and
the behaviors Phases 1–8 claimed but never exercised got tests too: declining *every*
change (`applied: false` with the reason), an `ignoreIfNotExists` delete of an absent
file not aborting the changes after it, and the encoding option both ways.

Deliberately *not* in scope: `failureHandling = transactional` / `undo` (see above).

## Phase 10 — a second review of Phases 1–9

Two behaviors that only went wrong on the paths the earlier phases reached last: the
*local* half of an off-tick fix, and the *off-tick* half of a local one.

1. **A local `create` with `ignoreIfExists` over an absent file gained a blank line.**
   Phase 7 taught that path that such a create is a create after all, and queued the
   write — but its buffer comes from `ensure_buffer_loaded` (an empty rope for a file
   that isn't there), not `create_file_buffer`, so it never joined `created_bufs` and
   missed the phantom-newline handling Phase 1 built for exactly this. The fill
   inserted *before* the rope's trailing newline and the created file ended one line
   longer than the edit asked for. Off-tick was unaffected (it keys the same handling
   off `pending_create_writes`), which is why the browser and daemon checks were green.
2. **A goto into an unopened file lost its column off-tick.** Phase 6's
   `jump_to_lsp_location` fix refined the column "only when the line text is really
   here", tested with `self.editor.cursor.line == line` — which a *deferred* open
   satisfies whenever the target is on **line 0**, the empty buffer clamping there. It
   then read the column off an empty line and overwrote the landing target with `0`, so
   a definition on line 0 still landed on the top-left, the very bug that fix was for.
   And on the lines it did leave alone the recorded column was the raw protocol
   `character` used as a byte offset — right for ASCII, wrong on any line with a
   multi-byte character under the protocol's utf-16 default. A deferred open is now not
   refined at the jump at all: the position is stashed (`PendingGoto`) and converted at
   the fetch landing, where the target line finally exists. Locally the jump was always
   exact — this is the tier-1 rule, so the remote one is too now.

And three smaller things the same read turned up:

- **A `changes`-map edit applied in hash order.** The map has no order of its own, but
  Phase 4 made this list one that is applied *in sequence* and reported on *by index*,
  so leaving it `HashMap`-ordered meant the messages — and a `failedChange` — differed
  between two runs of the same rename. Sorted by URI: which document goes first is
  arbitrary either way, being the same arbitrary each time is the point.
- **Two file-operation echoes still blared absolute paths** (`Deleted /tmp/…/x.rs`, and
  the skipped-rename note), which is what Phase 3 changed every other buffer-facing name
  away from.
- **Housekeeping:** Phase 8's move to the raw reply shape left `sync_client` importing a
  `WorkspaceEdit` it no longer names. Invisible to `cargo clippy --all-targets` (the
  module is `#[cfg(not(feature = "native"))]`), a warning in the build that actually
  compiles it — the wasm-eligible one.
- **`vim.lsp.util.show_document`'s third argument was discarded silently.** neovim's
  `{ reuse_win, focus }`: bemtvi's jump is always a focused `'switchbuf'`-aware one, so
  `reuse_win` is the behavior anyway, but `focus = false` asks for something this path
  cannot do — and now says so rather than focusing regardless.

Both fixes are covered by tests mutation-checked against the unfixed code: the local
`ignoreIfExists` create's two halves (spare a file that is there, create one that isn't,
with exactly the edit's bytes) in `lsp_features.rs`, and the off-tick goto's two
(a line-0 column surviving the deferred open, and a utf-16 column converting against the
line that landed) in `lsp_offtick.rs`.

## Phase 11 — a `create` creates the file, and stops there

Phase 3's deliberate deviation is reverted, at the user's call: bemtvi now does what
neovim does. A `create` resource operation puts the file on disk **empty**, and the
content the edits after it put in its buffer stays there — modified, unsaved, yours to
`:w` — exactly the in-memory contract every *other* change in a workspace edit gets. The
argument for writing the content (a `:q!` loses what the refactor extracted) applies just
as much to the edits a rename makes in ten other files, and those have never been written
behind your back; a `create` is not special enough to be the one exception.

So the disk half of a `create` is now a two-step chain on the same ordered fs seam —
recursive `mkdir` of the directory, then the empty file
(`WorkspaceFsOp::CreateDir` → `CreatePlaceholder`) — and the response waits for the file
to exist rather than merely for its directory. `Editor::queue_buffer_write`, added in
Phase 3 for the content write and now with no callers, is gone.

Writing the placeholder ourselves means telling **both** change detectors that the write
was ours, or each reports it straight back as an external change to a modified buffer — a
W12 conflict over bemtvi's own file:

- **Locally**, re-snapshot the buffer's disk baseline (`Editor::restamp_disk_baseline`).
  It had none, never having been read, and that is load-bearing twice over: without it a
  later `:w` *refuses* (the mutation test drops the content on the floor), and the
  buffer's file watch never arms at all, since `sync_buffer_watches` skips a buffer with
  no snapshot.
- **Over a daemon**, re-arm the watch with no `known` stat. The arm re-baselines to the
  live file and, per that leg's contract, "an absent/equal `known` pushes nothing" — so
  the daemon silently adopts the file we just made. Its `fs_write` leg self-suppresses
  this way for `:w`; the `luafs_op` leg the placeholder rides has no such hook, and this
  is the reason it needed one.

Tests updated to the new contract rather than deleted: the file must *appear* and be
empty, the buffer must hold the extracted text and be **modified**, and a plain `:w` must
then write it with no W11/W12/E211 (`:checktime` asserted clean). Two off-tick tests moved
their assertion from the daemon fake to the real disk, because the placeholder rides the
`FsJob` seam and that harness wires only the buffer legs to the fake — including the abort
test, whose "the file was not written" assertion would otherwise have become vacuous.
