# LSP work-done progress — `$/progress` from the wire to the statusline

Status: **done** (all four phases shipped, plus the post-ship review below).
Author-date: 2026-08-04.

**Post-ship review.** A read-back of the shipped chain found three more gaps, each now
test-first:

- **`nx.lsp.progress()` had no stable order.** The mirror is a client-id-keyed table and
  the read walked it with `pairs`, which is unordered — and *observably* reversed once
  the ids sit in the table's hash part (any session where client 1 has stopped): with
  ids 3 and 2 pushed highest-first, `pairs` yields 3 then 2. The docstring promised
  "newest task last" and nxvim-line renders `tasks[1]` plus `(+N)`, so the bar could
  pick a different server's task from one update to the next. The read now walks the ids
  sorted, and the guarantee (ascending client id; within a client, begin order) is
  documented rather than accidental.
- **The reconnect teardown didn't clear the store** (defence in depth, *not* an observed
  failure — see the correction below). `resync_lsp_after_reconnect` is the one path that
  drops a server record *without* `retire_lsp_server`, and the store is keyed by
  `ServerKey`, which the re-`ensure` reuses. It now clears the tasks with the record.
- **Numeric tokens were untested, and the mock could not spell one.** `progress_token`'s
  `NumberOrString::Number` branch — the "exactly one key type crosses the edge"
  guarantee — had no coverage, and the mock read every scripted token with `as_str`, so
  two numeric tokens collapsed to one `""` create. The mock now keys tokens the way the
  editor does while sending the create *verbatim*, and a test drives two numeric tokens
  through begin/end.

Also covered, as guards rather than fixes: a `report` with no `begin` (documented to be
accepted with an empty title, previously asserted nowhere).

**Correction — the reconnect "leak" was not real.** The review first read
`resync_lsp_after_reconnect`'s bare `lsp_servers.remove` as stranding both the progress
store *and* the Lua client handle, since it bypasses `retire_lsp_server`. Driving it end
to end says otherwise: **both** legs synthesize a server exit when the link drops — the
native demux clears its inflight map, dropping the `exit_tx` so `RemoteLspProcess::wait`
resolves `(None, None)`, and the Worker pushes a synthetic `lsp_exited` per live server
("a dropped link is a server exit to the SyncLspClient"). The retirement therefore always
runs *before* the resync, and the resync's own `remove` only ever meets an already-empty
slot. Measured, not assumed: the native `daemon_lsp.rs` reconnect test is 15/15 clean, and
the browser leg is clean too. So the `lsp_progress.remove` above is defence in depth for a
branch that doesn't currently arise, and there was never a client-handle leak to fix.

What the exercise *did* produce is the coverage that was missing: a reconnect had never
been driven with a language server attached on either leg. `crates/nxvim/tests/daemon_lsp.rs`
now covers the native one and `web/verify-lsp-reconnect.mjs` the browser one, both
asserting that a dropped link leaves exactly one live client and no stranded task.
Mutation-tested — deleting the Worker's synthetic `lsp_exited` reproduces precisely the
failure originally hypothesized (`nx.lsp.clients()` listing `1,2`, and `Indexing` spinning
under the dead client id), so the guard is real even though the bug was not.

**What the plan missed, and the build found.** Decoding `$/progress` was necessary but
not sufficient: a conforming server sends **nothing** unless the client both
*advertises* `window.workDoneProgress` at `initialize` **and** *acks*
`window/workDoneProgress/create`. nxvim did neither on the native leg — async-lsp's
method-not-found default answered the `create` with an error, and gopls concluded the
client cannot do progress and stayed silent. The wasm `SyncLspClient` already acked
every unmodelled request with `null`, so the two legs had **silently drifted**: the
browser leg would have worked and the native one never would. Both are fixed in
`client_capabilities()` / the router, and both are now guarded — the mock asks
permission per token and drops the updates for any token whose `create` the client
refused (removing the ack fails all seven behavior tests), and a separate test asserts
the capability on the recorded `initialize` wire. This is the whole reason the
"verify the example against a real server" rule exists: every mock-driven test passed
while real gopls sent nothing.

## Goal

A language server reports long-running work (indexing, loading a workspace, building a
crate graph) over `$/progress` with a `WorkDoneProgress` payload. nxvim currently
**drops it on the floor at every layer**:

- `crates/nxvim-lsp/src/client.rs:274` — the native client's
  `router.unhandled_notification` swallows it (the comment even names "progress").
- `crates/nxvim-lsp/src/sync_client.rs:660` — the wasm client's
  `on_server_notification` has no `"$/progress"` arm.
- There is no `LspEvent` variant for it, no store on `EditHost`, no Lua mirror, and no
  `LspProgress` autocmd event.

So a plugin cannot tell "lua_ls is indexing, 43%" from "lua_ls is idle", and
nxvim-line's `lsp` component can only ever render attached client *names*. The ask is
lualine's `lsp_status`: the attached servers **plus** what they are busy doing.

Close the gap at every layer, ending with the bundled statusline rendering it.

Non-goal for this plan: **cancellation**. `window/workDoneProgress/cancel` (the client
telling a server to stop a cancellable task) is a separate verb with its own UI
question ("what does the user press?"); the `cancellable` flag is carried through so a
later phase can act on it, but nothing sends the cancel.

## The shape of the data

`$/progress` carries `{ token, value }` where `token` is a `NumberOrString` minted
either by the server (via a `window/workDoneProgress/create` request — which the client
must ack; see Phase 0) or by the client on a request, and `value` is one of three tagged
variants (`lsp-types` `WorkDoneProgress`):

| kind     | fields                                              |
| -------- | --------------------------------------------------- |
| `begin`  | `title` (mandatory), `message?`, `percentage?`, `cancellable?` |
| `report` | `message?`, `percentage?`, `cancellable?`            |
| `end`    | `message?`                                          |

The protocol's own rules that the store has to honor:

- `title` arrives **only** on `begin`; a `report` that changes nothing about the title
  must not erase it. So the store is stateful per token, not a pass-through.
- An **absent** `message` / `percentage` on a `report` means "the previous value is
  still valid" — *not* "clear it". Only overwrite what the report actually carried.
- A server may (and rust-analyzer does) run **several concurrent tokens**, so the store
  is per-(client, token) and ordered, not a single slot.
- A `report` for a token we never saw `begin` for is legal-ish in the wild; accept it
  (with an empty title) rather than dropping it, so a non-conforming server still shows
  activity.

## Layers and phases

### Phase 0 (unplanned, found in Phase 4) — ask for it in the first place

`window.workDoneProgress: true` in `client_capabilities()`, and a
`window/workDoneProgress/create` handler on the native router that replies `Ok(())`.
Neither is optional: without the capability a conforming server never reports, and
without the ack gopls stops reporting after the first token it fails to create. See
the note at the top.

### Phase 1 — capture it in both clients, carry it as an `LspEvent`

`crates/nxvim-lsp/src/protocol.rs`:

- `ProgressKind { Begin, Report, End }` and

  ```rust
  pub struct ProgressUpdate {
      pub kind: ProgressKind,
      pub title: Option<String>,      // Begin only
      pub message: Option<String>,    // absent = "keep the previous"
      pub percentage: Option<u32>,    // absent = "keep the previous"
      pub cancellable: Option<bool>,
  }
  ```

- `LspEvent::Progress { key: ServerKey, token: String, update: ProgressUpdate }`.

  The wire token is a `NumberOrString`; it is normalized to a `String` **at the edge**
  (a number token becomes its decimal spelling) so exactly one token type crosses into
  the editor, the Lua mirror, and the autocmd payload. Both clients normalize the same
  way, so a number token from a native server and the same token over the wasm leg
  produce the same Lua key.

`client.rs` (native, `async-lsp`): `router.notification::<Progress>` decoding
`ProgressParamsValue::WorkDone`. This is the leg the **daemon** uses too — the daemon
ships raw `lsp_stdout` bytes and the `LspManager` runs edit-host-side — so remote
sessions get progress from this one arm (tier-1 rule).

`sync_client.rs` (wasm): a `"$/progress"` arm in `on_server_notification`, deserializing
the same `ProgressParams`.

Shared normalization (`NumberOrString` → `String`, `WorkDoneProgress` →
`ProgressUpdate`) lives in one function in `protocol.rs` that both clients call, so the
two legs cannot drift.

**Test:** the mock server gains a `progress` script field — a list of `$/progress`
payloads it emits after `didOpen` — so a test can drive a real begin/report/end
sequence. Phase 1's own assertion is at the Phase 2 boundary (there is nothing
user-visible yet), so the mock lands here and is asserted next phase.

### Phase 2 — the store, the Lua mirror, and the `LspProgress` event

`EditHost` (`crates/nxvim-server/src/lsp/`): a per-server ordered store

```rust
lsp_progress: HashMap<ServerKey, Vec<ProgressEntry>>   // ordered by first sighting
```

`on_lsp_event`'s new `LspEvent::Progress` arm folds the update in per the rules above
(begin inserts/replaces, report patches only the fields it carried, end removes the
token), then:

1. **Mirrors** the server's whole active list into `nx.lsp._progress[client_id]` (a new
   `LuaRuntime::set_lsp_progress`), the same push-a-mirror shape `set_diagnostics` /
   `set_lsp_client` use — Lua never reads live Rust state.
2. **Fires `LspProgress`** with `pattern = kind` (`"begin"` / `"report"` / `"end"`) and
   `data = { client_id, token, kind, title, message, percentage, cancellable }`.
   Neovim's `LspProgress` uses the kind as the pattern exactly this way, so
   `nx.autocmd.create("LspProgress", { pattern = "end", … })` works as a config author
   expects, and `nx.statusline`'s two-word `"LspProgress end"` event spelling narrows a
   segment to it.

   `fire_autocmd_data` only carries a `client_id`, so this needs a richer sibling
   (`fire_lsp_progress`) that builds the whole data table.

A server that exits drops its store entry along with its runtime and Lua client (the
existing `ServerExited` arm) — otherwise a crashed server leaves a spinner running
forever.

Public Lua API in `prelude/lsp.lua`:

```lua
nx.lsp.progress()                        -- every client's active work, newest client last
nx.lsp.progress({ client_id = id })      -- just that client's
nx.lsp.progress({ bufnr = 0 })           -- just the clients attached to that buffer
```

(Shipped as a **filter table**, matching `nx.lsp.clients(filter)`, rather than the bare
`client_id` sketched here.) "Newest client last" is a real guarantee, not an artifact of
iteration order — see the post-ship review at the top.

each item `{ client_id, client_name, token, title, message, percentage, cancellable }`.
Docstring written to the book's markdown rules (backticked tokens, fenced example).

**Tests** (`crates/nxvim/tests/`, black-box through the harness against the mock):

- a scripted `begin → report → end` leaves `nx.lsp.progress()` non-empty mid-sequence
  with the right title/percentage, and **empty** after the `end`;
- a `report` carrying only a `percentage` **keeps** the `begin`'s title and message
  (the "absent means unchanged" rule — the assertion that fails if the store is a
  naive overwrite);
- two concurrent tokens are both listed, and ending one leaves the other;
- `LspProgress` fires with `pattern = kind` and a `data.client_id` resolving through
  `nx.lsp.clients()`;
- a server exit clears its progress.

### Phase 3 — nxvim-line renders it

The `lsp` component (already in the default `lualine_x` as of `f6646e8`) grows the
progress half, which is what lualine's `lsp_status` actually shows: the client names,
and for each client with active work a spinner frame plus `title`/`message`/`percentage`.

- `opts.progress = false` opts back out to names-only.
- The spinner is a frame index advanced by an `nx.timer` that **only runs while some
  progress is active** — armed on `LspProgress begin`, disarmed when the last token
  ends. An always-on animation timer would violate the never-freeze/never-busy spirit
  of the per-event rule for a bar that is idle 99% of the time.
- Component `events = { "LspProgress", "LspAttach", "LspDetach", "BufEnter" }`.

Tests in the plugin's `test/components_spec.lua` drive `nx.lsp._progress` and assert the
rendered bar (the same style as the existing `lsp` / `diagnostics` component tests).

### Phase 4 — example + docs

`examples/lsp-progress/` in the core repo: an `init.lua` with numbered
*type-this / see-that* sections (an `LspProgress` autocmd echoing each phase, a
`nx.lsp.progress()` readout on a key, the statusline showing it live) plus a sample
file. Verified end-to-end, throwaway harness test deleted before commit per the
examples rule.

Book pages regenerate from the `nx.lsp.progress` docstring; the plugin's vimdoc
regenerates via `scripts/gen-vimdoc.sh`.

## Verification matrix (tier-1 remote)

| leg | how |
| --- | --- |
| native | `cargo test --workspace --test lsp_progress` against the mock |
| daemon | the native `LspManager` runs edit-host-side over raw `lsp_*` bytes — covered by the same arm; asserted via the existing daemon LSP test harness |
| wasm | `--no-default-features` build + the `$/progress` arm in `SyncLspClient` (which already acked `workDoneProgress/create` via its catch-all) |

Verified against **real gopls** through the example config, not only the mock — the
step that surfaced Phase 0. The check was a throwaway test (deleted before commit, per
the examples rule); its transcript was `begin Setting up workspace | end -`, with
`nx.lsp.progress()` non-empty mid-run and empty after, and the `end` carrying no title
— rule 1 observed on the wire.
