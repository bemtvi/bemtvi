# Multiple LSP servers attached to one buffer

Date: 2026-07-25

## Problem

Enabling two servers for one filetype — the canonical `pyright` + `ruff` Python
setup, or `ts_ls` + `eslint` — **spawns both and attaches one**. The other process
runs, initializes, and then sits idle forever: it never receives `didOpen`, never
answers a request, never publishes a diagnostic.

Worse, *which* one wins is nondeterministic. Measured over three identical runs
(two mock servers, both `filetypes = { "rust" }`):

```
spawned=[alpha,beta]  attached_to_buffer=[beta]
spawned=[alpha,beta]  attached_to_buffer=[beta]
spawned=[alpha,beta]  attached_to_buffer=[alpha]
```

`nx.lsp._on_filetype` iterates `pairs(nx.lsp._enabled)` — unspecified hash order —
and queues an `LspOp::Start` per matching server. Each `Start` overwrites a single
slot:

```rust
// lsp/sync.rs, apply_lsp_start
if state.server.as_ref() != Some(&key) { state.opened = false; state.version = 0; }
state.server = Some(key);
```

`LspDocState.server` is `Option<ServerKey>` — *"Which server owns this buffer"* —
and every downstream surface keys off it (24 read sites): document sync, all
requests, `publishDiagnostics`, semantic tokens, inlay hints, folding.

### What this already invalidates

- `nxvim-workspaces`' README claims each enabled server "attaches when you open a
  matching file", and its shipped example enables `pyright` **and** `ruff` for
  Python. That example cannot work as documented.
- `vim.lsp.buf.format({ name = … })` was just made to *reject* `name` (`f516cbe3`)
  on the grounds that there is nothing to select. Once this lands, `name` becomes
  meaningful and must be **modeled** instead — the rejection is a placeholder.
- `vim.lsp.get_clients({ bufnr })` can return at most one client, so any config
  branching on "is eslint attached?" silently sees the wrong answer.

## What is already multi-server ready

Both transports drive N servers keyed by `ServerKey` and need **no change**:

- native `LspManager` — `lsp_servers: HashMap<ServerKey, ServerRuntime>`;
- wasm `SyncLspClient` — `servers: HashMap<ServerKey, ServerState>` (`sync_client.rs`).

The Lua mirror is also already a set: `nx.lsp._attached[buf][client_id] = true`,
and `nx.lsp.clients{bufnr}` iterates it with `pairs`. It reports one client today
only because the core fires one `LspAttach`.

So the work is confined to **`EditHost`'s per-buffer document/request layer**. That
is the good news and the reason this is tractable.

## Constraints

- **Per-server document state.** Most of `LspDocState` is per-server, not
  per-buffer: `opened`, `version`, `last_tick`, `shadow`, `last_save_tick`,
  `diagnostics`, `semantic`, `inlay`. Two servers negotiate *different* position
  encodings and *different* sync kinds, so `shadow` and the version counter cannot
  be shared. Only `uri` and `language_id` are genuinely per-buffer.
- **`lsp_requests` is keyed by `LspReqKind` alone.** `register_lsp_request` inserts
  by kind and settles the displaced promise as `nil`. With two hover-capable
  servers, issuing to both means the second request cancels the first — so the
  pending map must key by `(kind, server)`, and `ReqToken` must carry the server.
- **Capability gating already exists per server** (`ServerRuntime.legend`,
  `inlay_hints`, `folding_range`), which is exactly the selector a fan-out needs:
  ask only the servers that advertise the feature.
- **Diagnostics must merge, not clobber.** `publishDiagnostics` is per-server, and
  each arrives in *its own* negotiated encoding. `diagnostics_merged` already
  carries `(diagnostic, encoding)` pairs precisely because client-set diagnostics
  differ in encoding from LSP ones — the same shape extends to N servers for free.
- **The editor must never freeze.** Fan-out multiplies requests per keystroke
  (completion, signature help). Per-server generation gating must drop stale
  replies without waiting on slow servers.
- **Tier-1 remote.** Every phase must work identically native and over a daemon,
  and be verified in both builds (`--test daemon_*`, plus `--no-default-features`).
- **No silent stubs.** A server that can't answer a kind is skipped explicitly; a
  request with no capable server settles its promise rather than hanging.

## Design

`LspDocState` splits in two:

```rust
struct LspDocState {              // per BUFFER
    uri: Option<Url>,
    language_id: String,
    servers: BTreeMap<ServerKey, LspServerDoc>,   // ordered => deterministic
    inlay_enabled: bool,          // buffer-level user toggles stay here
    semantic_enabled: Option<bool>,
}

struct LspServerDoc {             // per (BUFFER, SERVER)
    opened: bool,
    version: i32,
    last_tick: u64,
    last_save_tick: u64,
    shadow: String,
    diagnostics: Vec<Diagnostic>,
    semantic: SemanticTokensCache,
    inlay: InlayHintsCache,
}
```

`BTreeMap` (not `HashMap`) so iteration order is the server name — deterministic
output, which is the direct fix for the nondeterminism above.

**Request routing** gets an explicit policy per kind, because "ask everyone" is
wrong for some:

| kind | policy | why |
| --- | --- | --- |
| hover, signatureHelp | first capable, in name order | one popup; merging prose is noise |
| definition, declaration, typeDefinition, implementation | first capable | a jump has one destination |
| references, documentSymbol, codeAction | **fan out + merge** | genuinely additive across servers |
| completion | fan out, **streamed** (no barrier) | the whole point of a second server; the popup is already open, so shares append as they land |
| formatting | single, **selected** | `format({name=})` picks; else first capable |
| rename | first capable | a rename is one workspace edit |
| diagnostics (push) | per-server, merged at projection | already the shape |

The policy table is the reviewable artifact — it is where the judgment lives, and
it is deliberately conservative (fan out only where merging is well-defined).

## Phases

Each phase is committed separately and pauses for review.

### Phase 1 — split the state, keep exactly one server bound

Introduce `LspServerDoc` and the `servers` map. **Behavior does not change**: the
dispatcher still binds one server per buffer, so all 24 read sites move to
`state.primary()` (the single entry). Pure refactor with the full suite green.

*Why first:* it isolates the mechanical churn (24 sites) from any behavior change,
so the phase that follows has a reviewable diff.

Done when: suite green, no observable difference, `state.server` gone.

### Phase 2 — attach N, sync N

`apply_lsp_start` inserts into `servers` instead of overwriting. `sync_lsp` loops
the map, sending `didOpen`/`didChange`/`didSave`/`didClose` per server at its own
encoding and sync kind. `LspAttach` fires once per server; `LspDetach` likewise.

Done when: two mock servers both receive `didOpen` for one buffer, and
`vim.lsp.get_clients({ bufnr = 0 })` reports both, deterministically ordered.

### Phase 3 — per-server requests

Split in two once the work started, because the halves need different machinery
and only the second needs new state.

**3a — capability-aware selection (done).** Every request picks the first attached
server, in key order, that advertises the matching provider (`LspReqKind::provider`
→ `ProviderCaps`), instead of the buffer's first server. No new machinery:
`lsp_requests` stays keyed by kind because exactly one server is asked per request.
`PendingLspReq` records which server was asked, so a reply is decoded against the
encoding/legend of the server that *produced* it — re-deriving it would decode one
server's semantic tokens with another's legend, which paints plausible nonsense
rather than failing visibly. The semantic/inlay projections read across servers for
the same reason: the cache now lives under whichever server advertised the feature.

**3b — fan-out and merge (done, except completion).** References, document symbols
and code actions issue to every capable server; their replies fold into an
`LspFanout` round and present once. The round lives in its own map rather than
re-keying `lsp_requests` by `(kind, ServerKey)`: fan-out kinds have N replies in
flight for one user action, which the single-slot map cannot express, and keeping
them separate leaves the single-target path untouched.

Merged code actions carry the server that produced each one. That is not
bookkeeping: a lazy action is finished with `codeAction/resolve`, whose `data` blob
only its own server understands, so resolving ruff's action against pyright is a
wrong request rather than a degraded one.

A round completes when every asked server replies **or exits** (`drop_fanout_server`
retires a dead server's slot). A server that neither replies nor exits holds its
round open until the next request supersedes it — the same exposure a single hung
server has always had, not a new one.

**3c — completion (done).** Deferred out of 3b because it re-requests per keystroke
and its popup path needed its own design rather than the `LspFanout` round. It got
one: completion **streams** instead of merging at a barrier.

A round asks every `completionProvider` at once and each server's share appends to
the open menu the moment it lands, so a slow server delays only its own candidates
rather than holding the fast one's behind a barrier — the opposite of `LspFanout`,
which is right there (one merged list presents once) and wrong here (the popup is
already open and the user is still typing).

The requests ride the per-server pending map Phase 4 introduced, which is the same
shape completion needs — N in flight for one buffer, each reply decoded against its
own server — so the map was widened rather than a third one added
(`lsp_buf_requests` → `lsp_multi_requests`, `LspReqKind::per_server_pending`).

What the streaming model forced:

- **The round resets on ISSUE, not on first reply.** The merged cache is emptied when
  the requests go out, so every reply is a plain append. That is what keeps a row's
  `key` stable while the round is still filling — a lazy `completionItem/resolve`
  issued against row 3 of the first server's share still addresses row 3 after the
  second server's share arrives. Resetting on first reply would renumber under it.
- **The whole buffer's in-flight completion requests are retired at round start**, not
  just the per-server supersede. Otherwise a server this round does *not* ask (it
  stopped advertising completion, or detached) could still land its previous share in
  this round's cache.
- **Each item records its origin server.** The accept converts its `textEdit` at that
  server's negotiated encoding, and its `completionItem/resolve` goes back to it — the
  `data` blob is that server's own handle on the item, so resolving ruff's candidate
  against pyright is a wrong request. Both were reading the buffer's *first* server.
- **Priority steps down by the server's position in key order.** The engine's blended
  sort is stable, so equal-scoring rows keep *streamed* order — which is reply-arrival
  order, i.e. nondeterministic. One point per server makes cross-server ties break in
  key order instead, and keeps every LSP row far above the buffer-word tier (the `lsp`
  bias is 8 against 0).
- **An identical offer from a second server is dropped** (same label, kind, snippet
  flag and effective insert text): the two rows are indistinguishable in the popup and
  accept to the same text. Anything that differs is kept — accepting it does something
  different.
- **`isIncomplete` is OR-ed** across the servers that replied: if any narrowed its list
  to the old prefix, a prefix edit must re-request rather than re-serve the cache.
- **A trigger with no capable server is silent.** It fires per keystroke, so the
  single-target path's "No language server attached" echo would shout once per typed
  character (the same reason the signature-help auto-trigger drops quietly).

Done when: two servers with overlapping capabilities both contribute references,
and a slow server cannot stall the other's reply.

3c is covered by `completion_merges_candidates_from_every_capable_server`,
`accepting_a_candidate_applies_its_own_server_encoding`,
`a_lazy_docs_resolve_goes_back_to_the_items_own_server` and
`a_completion_burst_does_not_accumulate_candidates` in
`crates/nxvim/tests/lsp_complete.rs`. The last is the amplification guard the risk
section asks for: a 12-keystroke burst inside one word must leave exactly one row per
server, so a cache that accumulated per keystroke fails rather than merely looking
busy. The encoding and resolve-routing tests were both mutation-checked — restoring
the "buffer's first server" derivation fails them.

### Phase 4 — diagnostics, semantic tokens, inlay hints per server (done)

`publishDiagnostics` stores under its own server; `diagnostics_merged` concatenates
across servers, each with its own encoding (the pair shape already supports it).
That half landed with Phase 3, because `publishDiagnostics` is a *push*: two attached
servers publish independently, so a shared slot has each one's set erase the other's.

Semantic tokens and inlay hints now request from **every** advertising server, cache
per server, and merge at projection. Three things that were forced by the work:

- **Their own pending map.** `lsp_requests` is one slot per kind, which cannot hold
  two semantic-token requests in flight for one buffer — the second evicts the first,
  and the reply is then decoded against whichever server was recorded last. Wrong
  legend ⇒ plausible nonsense; wrong encoding ⇒ columns inside a multi-byte glyph.
  `lsp_buf_requests` keys by the token's unique generation and records `(kind, buffer,
  server)`, so one request per capable server is outstanding and each reply lands
  under the server that produced it. Folding ranges moved there too, but stay
  **single-target**: a buffer has one fold structure, and merging two containment
  trees is not defined.
- **The mirrors are per buffer, so they rebuild across servers.**
  `nx._semantic_tokens[buf]` / `nx._inlay_hints[buf]` are one flat list each; pushing
  the answering server's half would erase the other's. Both are rebuilt from every
  attached server's cache, each entry tagged with its producing `client_id`, sorted
  line-then-column. The projections likewise re-sort the merged spans — inlay hints
  are inserted left to right, so an out-of-order anchor lands at a shifted column.
- **A reply's positions belong to the server that sent it, on the apply paths too.**
  `apply_formatting_edits` / `apply_workspace_edit` derived their encoding from the
  buffer's *first* server. `format{ name = … }` (Phase 5) makes that reachable by
  design: name the utf-16 server on a buffer whose first is utf-8 and every edit on a
  line with a multi-byte character shifts. Both now take the producing server's
  encoding — carried on `PendingLspReq.server` for a reply, on the merged action's
  origin for a code action, `codeAction/resolve` included.

Done when: two servers publishing diagnostics for one buffer both render, with
correct columns under differing negotiated encodings.

Covered by `semantic_tokens_merge_from_every_capable_server`,
`inlay_hints_merge_from_every_capable_server` and
`a_named_formatter_applies_edits_at_its_own_encoding` in
`crates/nxvim/tests/lsp_config.rs` — each mock pair negotiates utf-8 against utf-16
over a line (`let föö = 1`) whose byte and code-unit columns disagree, so a shared
encoding fails the assertion rather than passing by luck.

### Phase 5 — the Lua/compat surface catches up (done)

- `nx.lsp.format{ name = … }` / `vim.lsp.buf.format{ name = … }` **modeled** —
  replacing the `f516cbe3` rejection. `LspOp::Format` carries the name, and
  `:LspFormat [server]` is the ex twin. A name not attached to the buffer reports
  so and resolves `nil`; it never silently falls back to a different server, which
  is the failure the option exists to prevent. `bufnr`/`range`/`filter` stay
  rejected (nxvim formats the current buffer whole).
- `nx.lsp.clients{bufnr}` documented as returning N, with the warning not to index
  `[1]` expecting "the" server.
- `nxvim-workspaces` README/example corrected — multi-server is now described as
  the normal case, and the Python example names `ruff` as its formatter because
  pyright also advertises formatting.
- `docs/architecture.md` needed no change: it never claimed one server per buffer.

### Phase 6 — remote + wasm verification (done)

The multi-server layer is *plausible* remotely by construction — it all lives in
`EditHost`, and both transports are already keyed by `ServerKey` — but "the design
says it should" is not a verification, so both legs are now driven with two servers.

**Native daemon** (`crates/nxvim/tests/daemon_lsp.rs`). A new harness helper,
`spawn_with_daemon_lsp`, injects a `RemoteLspTransport` talking to a
`serve_lsp_daemon` over an in-process duplex, so each mock server is a real child on
the daemon side with its stdio tunneled. Two tests: both servers attach and *both*
publish (`publishDiagnostics` is the sharpest probe available — a server→client push
only happens if that server actually received `didOpen` down its own tunnel, so two
messages prove two documents, not two spawned processes); and a hover routes by
capability across the tunnel. Mutation-checked by stopping the daemon side from
serving — both tests then fail, so the wire is load-bearing rather than a local
fallback.

**Browser / wasm** (`crates/nxvim-edithost/web/verify-lsp.mjs`). Extended from one
mock server to two for the same filetype. Each of the three added checks is a merge
or a routing decision a one-server session cannot satisfy: both servers' diagnostics
merge, the hover reaches the one advertising `hoverProvider` (`mock2` withholds it),
and completion fans out and merges both servers' candidates (Phase 3c over the wire).
Mutation-checked by enabling only `mock` — all three then fail.

Two pre-existing breaks surfaced and were fixed to get there, neither caused by this
work (both reproduce with the multi-server changes stashed):

- `nxvim-edithost` did not compile to wasm at all: `GitJob::Fetch` had no arm in the
  browser git-job encoder (added by the gix work, whose decoder side already handled
  `"fetch"`). The wasm build is a tier-1 target, so this was a silent hole — nothing
  in `cargo test --workspace` builds that crate for wasm.
- `verify-lsp.mjs`'s hover check had gone stale: it read the content-float surface
  (`frame().float`), but hover became a real float **window** (`windows[]` with
  `floating == true`) — the same place the native `lsp_config.rs` helpers read. It was
  failing against a *single* server before this phase touched it.

`--no-default-features` compiles (`cargo check -p nxvim-server --no-default-features`),
which is the other half of the wasm-eligible build.

### Phase 7 — the surfaces that still resolved "the" server by position (done)

A review of the finished feature found the same failure mode surviving in six more
places: a path answering "which server?" with the buffer's **first** attached one
instead of the one actually involved. Phases 1–6 fixed it for sync, request routing
and the decorations; these are the request *context*, the merged results, and the
apply/dispatch follow-ups. Each is covered by a test confirmed failing first, in
`crates/nxvim/tests/lsp_config.rs`.

- **`context.diagnostics` is per server.** The code-action fan-out asked every
  server but handed them all one list harvested from the first. A linter gates its
  quick-fixes on *its own* diagnostic being quoted back, so the second server was
  asked about problems it never published and its fixes were silently never offered
  — the exact hole the fan-out exists to close. Each server is now sent the
  diagnostics it published over the range, in its own encoding. (Client-set
  `vim.diagnostic.set` entries stay out, as in neovim: they carry no server's
  `data`, so no server can act on them.)
  `a_code_action_request_carries_each_servers_own_diagnostics`.
- **Merged locations and symbols decode at their reporting server's encoding**, and
  duplicates actually collapse. `apply_lsp_locations` / `apply_lsp_symbols` derived
  one encoding from the buffer's first server, and the dedup was `Vec::dedup_by` —
  which only collapses *adjacent* duplicates, so a merged list of alpha's block then
  beta's never collapsed anything. `LspFanout` now pairs every location/symbol with
  its producer's encoding, and the duplicate check runs on the **converted**
  `(path, row, byte)`: two servers at different encodings spell one position
  differently, so comparing raw LSP ranges fails for precisely the case the merge
  creates. `references_merge_deduplicate_and_decode_at_each_servers_encoding`.
- **A rename applies at the answering server's encoding.** Phase 4 fixed this for
  `apply_formatting_edits` and missed `apply_workspace_edit`, which re-derived the
  encoding *per target buffer* from that buffer's first server — overriding the
  origin it had just been handed. `a_rename_applies_its_edits_at_the_answering_servers_encoding`.
- **A code action's `command` runs on the server that offered it.** The merged
  chooser tracked each action's origin for `codeAction/resolve` and then dropped it
  for the command. Found while testing it: `nx.lsp._dispatch_command` **did not
  exist** — the neovim-compat removal took `vim.lsp.commands` with it, so every
  command-carrying action had been failing with an `E5108` since. Implemented for
  real (a `nx.lsp.commands[name]` client-side registry, else
  `workspace/executeCommand` on the origin client), with `vim.lsp.commands` aliased
  to the same table. `a_code_actions_command_runs_on_the_server_that_offered_it`.
- **The signature-help auto-trigger gates by capability.** Core's trigger set is a
  union across servers, so it raised the request correctly; the per-buffer gate then
  resolved to the buffer's *first* server and dropped it. On `eslint` + `ts_ls`
  every typed `(` was swallowed. `the_signature_autotrigger_fires_for_the_server_that_advertises_it`.
- **`documentSymbol` / `workspaceSymbol` are modelled in `ProviderCaps`.** Both were
  unmodelled, so the routing predicate failed *open* and the documentSymbol fan-out
  asked every attached server including ones that never advertised it. Both flags
  are now probed at `initialize` and mirrored to `server_capabilities`.
  `document_symbols_only_ask_the_servers_that_advertise_them`.
- **`workspace/symbol` fans out.** It was the last list-shaped kind still answered by
  one server, though merging it is as well defined as `documentSymbol`'s — two
  indexers each know symbols the other does not.
  `workspace_symbols_merge_from_every_capable_server`.
- **`:LspInfo` reports every attached server**, not just the first — the
  introspection surface for exactly the thing that became plural. Its "Running
  servers" section listed both all along, which made the header's silence misleading
  rather than merely incomplete. `lsp_info_reports_every_server_on_the_buffer`.
- **Merged diagnostics carry the `client_id` that published them.**
  `vim.diagnostic.get` returns one flat list per buffer, merged across servers, so
  without a tag there is no way to tell a type-checker's errors from a linter's —
  the first question anyone asks of a two-server buffer. The semantic-token and
  inlay-hint mirrors were tagged in Phase 4 for exactly this reason; the diagnostics
  mirror, built by the same merge, was not. (`source` is close but is *server* text —
  a linter name, `"compiler"`, or absent — not a handle that resolves back to a
  client.) `None` for a client-set `vim.diagnostic.set` entry, which has no server
  behind it. `merged_diagnostics_carry_the_client_that_published_each`.

Fallout the above made possible: `LspDocState::primary` is gone and `primary_key`
survives only as `reply_encoding`'s last-resort fallback for an edit with no
producing server (one built in Lua). `request_lsp` now rejects a non-cursor kind by
name instead of falling through to a request of a *different* kind — a raw
`nx._lsp_buf(10)` used to issue `documentSymbol` for a code-action ask — and the
single-slot `PendingLspReq.code_action` field went with the dead single-target
code-action reply arm (code actions always fan out).

The encoding rule this phase generalizes, for anything added later: **a reply's
positions belong to the server that sent it.** Carry the producing server from the
request to the reply and convert with *its* encoding; never re-derive one from the
buffer. The buffer's first server is the right answer only when there is no
producing server at all.

## Testing

Per repo convention every phase is black-box through the running server. The mock
(`nxvim --__lsp-mock`) already answers from real document state; multi-server tests
need it spawnable **twice with different scripts**, which today is blocked by
`$NXVIM_LSP_CMD` overriding the argv globally. Phase 2 therefore also introduces a
per-server mock override (`$NXVIM_LSP_CMD_<name>`), or the tests cannot distinguish
which server answered — a prerequisite, not an afterthought.

## Risks

- **Request amplification.** Completion fan-out doubles per-keystroke traffic.
  Mitigated by capability gating and per-server generation drops; guarded by a
  flood test in the spirit of `terminal.rs`. *(3c: traffic is linear in attached
  servers, and the per-round reset keeps the merged cache from growing with the
  keystroke count — `a_completion_burst_does_not_accumulate_candidates` holds that
  line. The remaining exposure is the wire itself: N servers answering per keystroke,
  which is what enabling N servers asks for.)*
- **Encoding bugs.** Two servers on one buffer at utf-8 and utf-16 is exactly where
  column math breaks. Phase 4 tests must use *differing* encodings deliberately.
- **Phase 1 churn.** 24 mechanical sites; the risk is a silent behavior change
  smuggled in. Mitigated by requiring the suite green with zero test edits.
