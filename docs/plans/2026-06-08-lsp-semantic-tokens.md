# LSP semantic tokens — completion plan

> **Status: COMPLETE — Phases 1–3 ✅.**
> Closes the one remaining open wire from
> [ADR 0001](../decisions/0001-native-engines-vendored-lua-apis.md)'s bridge
> table: **bridge #2 — LSP semantic tokens**. Phase 1 builds the full bridge —
> client capability + legend capture, the `semanticTokens/full` request fired on
> open/change, the packed-token decode, and the per-line projection merged **above
> the treesitter floor** through the existing `highlights_for` merge. A server that
> advertises `semanticTokensProvider` (verified against real `lua_ls`) now refines
> the syntactic colors; an undefined `@lsp.*` group is dropped so the floor shows
> through. Phase 2 adds the **`full/delta` refresh** — once a buffer has a
> `resultId`, a refresh sends the diff request, the edits splice into the cached
> token array, and the repaint matches the equivalent full set; a server without
> delta support, or one that can't honor the `resultId`, falls back to a full set
> transparently. Phase 3 adds the **`vim.lsp.semantic_tokens` Lua surface** —
> `start`/`stop`/`force_refresh`/`get_at_pos`, the editor-wide `enable` gate, and
> `client.server_capabilities.semanticTokensProvider` — over a per-buffer enable
> flag and a Rust→Lua token mirror; `highlight_token` stays a loud `btv._notimpl`
> gap. The remaining gaps are the per-phase *approximations* below (one group per
> cell, theme-gated, no `range`, `highlight_token`).

## Why this document exists

ADR 0001 names three highlight bridges of one shape — a vendored/async enrichment
projected into bemtvi's own highlight layer at the right priority, always *over* the
synchronous treesitter floor so a slow or absent server degrades to "syntactic but
correct," never to blank. Two are built (`vim.treesitter.start`, the query
bridge). The third — **semantic tokens** — is described there as the open wire:

> **LSP semantic tokens** — async, server-side. Cached on arrival and projected as
> extmarks *above* the treesitter floor. Request plumbing exists in `bemtvi-lsp`;
> the response→extmark projection is the open wire.

Semantic tokens are the server's *authoritative* classification of every token in
the buffer — which `foo` is a function vs. a variable vs. a parameter, which name
is `readonly` or `deprecated` — richer than treesitter's purely-syntactic guess.
A real nvim setup paints them at priority `125`, just over the treesitter floor
(`100`) and under user extmarks. We paint neither.

The fail-loud, no-silent-stub rule from the
[LSP completion plan](2026-06-05-lsp-completion.md) applies: a part of the API we
don't honor yet stays a documented approximation or raises through `btv._notimpl`
— never a silent no-op that looks like it worked.

## What's already in place (the seams these phases extend)

- **The projection / merge layer.** `Server::highlights_for`
  (`crates/bemtvi-server/src/treesitter.rs`) already merges treesitter spans
  (priority `bemtvi_core::TS_HL_PRIORITY = 100`) with the line's hl_group extmarks
  (`bemtvi_core::DEFAULT_PRIORITY = 4096`) through
  `crate::extmarks::merge_intervals`, emitting one non-overlapping winning span
  list under the `highlights` window key. Semantic tokens become a **third source
  of `HlInterval`s** at priority `125` — no new render key, no client change.
- **The per-buffer LSP state.** `LspDocState` (`crates/bemtvi-server/src/lsp/mod.rs`)
  holds the document-sync bookkeeping and the `diagnostics` cache, keyed by
  `BufferId` in `Server::lsp_states`. The token cache is a new field beside it.
- **Per-server runtime + caps capture.** `ServerRuntime` (encoding, sync_kind,
  client_id) is built from the `initialize` reply in `Server::on_lsp_event`
  (`lsp/sync.rs`), out of `ServerCaps` (`bemtvi-lsp/protocol.rs`), itself distilled
  by `provider_caps` / `encoding_of` / `sync_kind_of` (`bemtvi-lsp/client.rs`). The
  **legend** rides the same path as a new distilled field.
- **The typed request/reply machinery.** `LspRequest`/`LspReply`
  (`bemtvi-lsp/protocol.rs`), `LspReqKind` + the generation/tick stale-drop gate
  (`lsp/mod.rs`, `lsp/request.rs`). Semantic tokens add one `LspReqKind`, one
  `LspRequest` variant, one `LspReply` variant — but request *whole-buffer*, not
  at the cursor, and refresh on edit rather than on cursor move.
- **Encoding-aware column conversion.** `byte_col(encoding, line, char)` and the
  `virtcol` tab/wide-char screen-column mapping the diagnostics underline uses are
  exactly what decode needs (LSP char offsets → bytes → screen columns).
- **The client capabilities handshake.** `client_capabilities()`
  (`bemtvi-lsp/client.rs`) is where we advertise `textDocument.semanticTokens` so
  the server returns its legend and answers the request.
- **The scripted mock + redraw test harness.** `bemtvi-lsp/src/mock.rs` scripts
  deterministic replies per method; the `crates/bemtvi/tests/lsp/` and
  `crates/bemtvi-server/tests/` suites drive it over the in-process pipe and assert
  on `nvim_buf_get_lines` / the `redraw` view. A `semantic_tokens` script field +
  a `highlights`-key assertion is the test shape.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 1 — Full-document semantic highlighting (the core bridge) ✅

**Goal.** Paint `textDocument/semanticTokens/full`: when a buffer's server
advertises `semanticTokensProvider`, request the whole-buffer token set on open
and after every change, decode it, and project it as highlight intervals at
priority `125` — over treesitter, under user extmarks — resolving the
`@lsp.type.<type>` / `@lsp.typemod.<type>.<modifier>` highlight groups. The
headline deliverable: server-authoritative highlighting layered on the syntactic
floor.

**Why.** This is the entire user-visible payoff; delta (Phase 2) and the Lua
control surface (Phase 3) only optimize and expose what Phase 1 paints. It also
establishes the legend-decode machinery and the third-source merge the later
phases reuse.

**Scope (files).**
- `crates/bemtvi-lsp/src/client.rs` — `client_capabilities()` advertises
  `text_document.semantic_tokens` (`SemanticTokensClientCapabilities`: request the
  `full` token set, declare the token-type/modifier vocabulary we understand,
  formats `relative`). Add a `legend` field to the distilled `ServerCaps` (the
  `semanticTokensProvider.legend.{tokenTypes,tokenModifiers}` string arrays), set
  by a new `semantic_legend(caps)` distiller beside `provider_caps`; surface a
  `semantic_tokens: bool` in `ProviderCaps`.
- `crates/bemtvi-lsp/src/protocol.rs` — `LspRequest::SemanticTokensFull { uri }`;
  `LspReply::SemanticTokens(SemanticTokensData)` where `SemanticTokensData` carries
  the server's `result_id: Option<String>` and the raw `data: Vec<u32>` (the
  manager forwards the packed array verbatim — decode happens editor-side, where
  the buffer text and encoding live, mirroring how completion edit ranges stay in
  the negotiated encoding for the editor to convert). `ServerCaps.legend`.
- `crates/bemtvi-lsp/src/manager.rs` / `dispatch.rs` — issue the typed
  `semanticTokens/full` request and normalize the `SemanticTokens` /
  `SemanticTokensPartialResult` reply shapes to `SemanticTokensData`; carry the
  legend on `LspEvent::Initialized`.
- `crates/bemtvi-server/src/lsp/mod.rs` — `LspReqKind::SemanticTokens`; store the
  legend on `ServerRuntime`; a `SemanticTokens` cache type on `LspDocState`
  (`result_id`, the raw `data`, and the decoded **per-line byte spans** keyed like
  the syntax `spans`, each `{ start, end, group }`).
- `crates/bemtvi-server/src/lsp/sync.rs` — capture `legend` into `ServerRuntime` on
  `Initialized`; fire a `semanticTokens/full` request when a buffer opens and after
  a `didChange` is flushed (gated on the server advertising support), under the
  same dirty/coalescing the sync loop already runs — debounced to the post-change
  flush, not per keystroke.
- `crates/bemtvi-server/src/lsp/request.rs` — register/dispatch the request; on the
  reply, decode `data` against the server's legend + encoding into per-line byte
  spans and cache them; mark `lsp_dirty`. The stale-drop gate is **content-version
  (`tick`)**, not cursor (tokens are whole-buffer): a reply computed against
  superseded text is dropped, exactly like the formatting apply-guard.
- `crates/bemtvi-server/src/lsp/semantic.rs` *(new)* — the decode + projection:
  `decode_semantic_tokens(data, legend, encoding, buffer)` (cumulative
  `(deltaLine, deltaStartChar, length, tokenType, tokenModifiers)` 5-tuples →
  absolute `(line, char, len)` → bytes → per-line spans, resolving each token to
  its **most-specific resolvable** `@lsp.*` group) and `semantic_intervals(buffer,
  line_idx)` returning the line's `HlInterval`s at priority `125`.
- `crates/bemtvi-server/src/treesitter.rs` — `highlights_for` folds the semantic
  intervals in as a third merge source (between TS and extmarks by priority).
- `crates/bemtvi-lsp/src/mock.rs` — a `semantic_tokens` script field (a legend +
  packed `data`) returned for `semanticTokens/full`, plus the legend in the mock's
  advertised `initialize` capabilities.

**Approach.**
- **Decode.** The packed `data` is 5-integers-per-token, line/char deltas
  *relative to the previous token* (LSP §`semanticTokens`). Walk it
  cumulatively to absolute `(line, startChar, length)`; convert `startChar` and
  `startChar+length` through the negotiated encoding (`byte_col`) to a line-local
  byte span; bucket by line — the same per-line `BTreeMap<usize, Vec<Span>>` shape
  the syntax engine caches, so projection mirrors `highlights_for`'s fast path.
- **Token → group.** Map `tokenType` index → legend type name, and each set
  `tokenModifiers` bit → legend modifier name. Resolve, most-specific first:
  `@lsp.typemod.<type>.<modifier>` (for each active modifier) then
  `@lsp.type.<type>`, via `highlights.resolve_capture`. **A token whose group does
  not resolve to a style is dropped from the projection** — it must not enter the
  merge, or it would win priority `125` over the treesitter span beneath and blank
  the cell (the unstyled-extmark trap neovim sidesteps by not applying an undefined
  hl_group). So a theme with no `@lsp.*` definitions (the built-in default) shows
  the treesitter floor unchanged; catppuccin et al., which define `@lsp.type.*`,
  light up. Resolution is **server-side** (we own the registry), unlike treesitter
  spans which pass the group name for the client to resolve — because the drop
  decision needs the resolved style here.
- **Priority `125`.** Neovim's `vim.highlight.priorities.semantic_tokens`. Above
  `TS_HL_PRIORITY` (100), below `DEFAULT_PRIORITY` (4096), so a user extmark still
  wins. Add `SEMANTIC_HL_PRIORITY = 125` next to the others in
  `bemtvi-core/src/extmark.rs`.
- **One group per cell (approximation).** bemtvi's `cell_style` takes the *winning*
  span and does not blend stacked hl_groups, so a token paints its single
  most-specific resolvable group rather than neovim's stack of
  type + per-modifier extmarks. This matches how the merge layer already resolves
  treesitter/extmark overlaps to one winner — recorded as an approximation, not a
  silent divergence.

**Tests** (`crates/bemtvi/tests/lsp/` via the scripted mock + redraw, and a Tier-2
paint test under `crates/bemtvi-server/tests/`):
- a `semanticTokens/full` reply with a legend + packed data paints the named token
  span in the window's `highlights` with the resolved `@lsp.type.*` style, over the
  treesitter group it overlaps;
- a token whose `@lsp.*` group is undefined in the active theme leaves the
  treesitter span beneath intact (the drop rule) — no blanked cell;
- a user extmark at `DEFAULT_PRIORITY` over the same range still wins (priority
  ordering);
- the decode is encoding-correct: a UTF-16 server's char offsets over a wide-char
  line land on the right bytes/screen columns (mirrors the diagnostics encoding
  test);
- editing the buffer re-requests and repaints (a second scripted token set lands
  after a `didChange`).

**Done when.** ✅ A buffer whose server advertises `semanticTokensProvider` shows
server-classified highlighting layered over treesitter — visible in the
`highlights` redraw key and on the rendered grid — while a theme without `@lsp.*`
groups, or a server without the capability, renders exactly as today. The client
capability is advertised in `client_capabilities()` (full token set,
`augments_syntax_tokens`); the legend is distilled by `semantic_legend(caps)` onto
`ServerCaps.legend`, rides `LspEvent::Initialized` → `ServerRuntime.legend`. The
typed `LspRequest::SemanticTokensFull` / `LspReply::SemanticTokens` (carrying the
packed `SemanticTokensData`) are issued from `sync_lsp` on open + after each
`didChange` (`request_semantic_tokens`, gated on the server having a legend) and
normalized in `dispatch.rs`. The reply is stale-dropped on the *issuing buffer's*
content (`tick`) change, decoded against that buffer's legend + encoding into the
per-line `SemanticTokensCache.spans` on `LspDocState`
(`crates/bemtvi-server/src/lsp/semantic.rs`), and projected by `semantic_intervals`
at `SEMANTIC_HL_PRIORITY = 125` as a third source folded into `highlights_for`'s
`merge_intervals` (between treesitter at 100 and extmarks at 4096). Resolution is
server-side and a token whose candidate `@lsp.*` groups all fail to resolve is
**dropped** from the projection, so the treesitter span beneath never blanks.
Verified by `semantic_tokens_paint_over_the_treesitter_floor` /
`an_undefined_semantic_group_is_dropped_so_treesitter_shows` /
`semantic_token_columns_are_encoding_correct` /
`editing_re_requests_and_repaints_semantic_tokens`
(`crates/bemtvi/tests/lsp/semantic.rs`), and end-to-end against real `lua_ls` (it
advertises a legend → bemtvi captures it → `→ semanticTokens/full` fires on open and
change). Runnable demo: `examples/semantic-tokens/`.

*Known approximations:* one resolvable group per cell (no neovim-style
type + modifier stacking/blending); whole-document only (delta is Phase 2, range
is unsupported); resolution is theme-gated (no `@lsp.*` definitions ⇒ no semantic
paint, treesitter floor shows); refresh re-requests the full token set on every
change until Phase 2.

**Depends on.** Nothing (extends the existing projection + request machinery).

---

## Phase 2 — Incremental `semanticTokens/full/delta` ✅

**Goal.** After the first full response (which carries a `resultId`), send
`textDocument/semanticTokens/full/delta { previousResultId }` on refresh; the
server replies with *edits* to the previous packed array (or a fresh full set).
Apply the edits to the cached `data`, re-decode, repaint — so a keystroke-driven
refresh ships a small diff instead of the whole token array.

**Why.** `full` re-requests the entire token set on every change; on a large file
with rust_analyzer that is a non-trivial payload per edit. Delta is what every
real editor uses for the steady-state. It is a pure optimization over Phase 1's
cache — same paint, less wire.

**Scope.**
- `crates/bemtvi-lsp/src/protocol.rs` — `LspRequest::SemanticTokensDelta { uri,
  previous_result_id }`; the reply normalizes both `SemanticTokens` (a fresh full
  set) and `SemanticTokensDelta` (an `edits: [{ start, deleteCount, data }]` list)
  into the existing `SemanticTokensData` plus an `edits` discriminant.
- `crates/bemtvi-lsp/src/manager.rs` / `client.rs` — declare `delta` support in the
  client capability; issue `full/delta` when a `previousResultId` is known.
- `crates/bemtvi-server/src/lsp/{request.rs,semantic.rs}` — choose `full` vs.
  `full/delta` by whether the cache holds a `result_id`; apply the splice edits
  (`data.splice(start..start+deleteCount, new)`) to the cached array, then
  re-decode the whole (cheap) array. A delta whose `previousResultId` no longer
  matches the cache (a dropped/again-stale reply) falls back to a full request.

**Approach.** The cache already holds `result_id` + raw `data` from Phase 1;
delta is "patch the array, re-decode." Re-decoding the full array each time (rather
than incrementally patching the per-line span map) keeps the decode path single
and correct — the win is wire size, not decode cost, which is already negligible.

**Tests.**
- after a full reply, an edit triggers a `full/delta` request carrying the prior
  `resultId` (assert on the mock's recorded request);
- a scripted delta (`{ start, deleteCount, data }`) applied to the cached array
  produces the same paint as the equivalent full set would;
- a delta with a stale/absent `previousResultId` falls back to a full request.

**Done when.** ✅ Once a buffer has cached a `resultId` *and* its server advertised
`semanticTokensProvider.full.delta`, every refresh sends
`LspRequest::SemanticTokensDelta { previous_result_id }` instead of `…Full` (the
choice is in `request_semantic_tokens`, `crates/bemtvi-server/src/lsp/semantic.rs`,
gated on the new `ServerRuntime.semantic_tokens_delta` captured from caps by
`semantic_tokens_delta(caps)` in `bemtvi-lsp/client.rs`). The reply
(`LspReply::SemanticTokens`) is now a `SemanticTokensData::{Full,Delta}` enum
normalized in `dispatch.rs`: a `TokensDelta`/`PartialTokensDelta` becomes `Delta`,
a `Tokens` (the server's full-set fallback) becomes `Full`. `on_semantic_tokens_reply`
replaces the cache on `Full` and, on `Delta`, applies the edits to the cached
packed tokens (`SemanticTokensCache.tokens`) via `apply_token_edits` — a flat-
integer-array splice rebuilt segment-by-segment in ascending `start` order
(neovim's scheme) — then re-decodes; a delta with no cached base to patch clears
the cache and re-requests `full`. The client advertises
`full: { delta: true }`. Verified by
`editing_after_a_full_result_sends_a_delta_request` (the delta request quotes the
prior `resultId`) / `a_delta_patches_the_cached_token_array` (a scripted edit
repaints to match the equivalent full set) /
`a_delta_request_answered_with_a_full_set_replaces_the_cache` (the Tokens-variant
fallback) (`crates/bemtvi/tests/lsp/semantic.rs`), the scripted-delta mock
(`bemtvi-lsp/src/mock.rs`), and the extended `examples/semantic-tokens/`.

**Depends on.** Phase 1 (the cache + decode).

---

## Phase 3 — The `vim.lsp.semantic_tokens` Lua surface + caps gate ✅

**Goal.** Expose the neovim control surface — `vim.lsp.semantic_tokens.start` /
`stop` / `force_refresh` (and `get_at_pos`) — surface `semanticTokensProvider` in
`client.server_capabilities`, and add a config gate to disable the feature. Match
neovim's *auto-enable on attach* (a server that advertises the capability lights
up without the user calling `start`), with `start`/`stop` as the override.

**Why.** Configs and plugins call this surface by name (`vim.lsp.semantic_tokens`
is currently a nil index — a loud gap waiting). It is also how a user turns the
feature off per buffer or forces a refresh, and how an `on_attach` branches on
`client.server_capabilities.semanticTokensProvider`.

**Scope.**
- `crates/bemtvi-lsp/src/protocol.rs` / `client.rs` — `ProviderCaps.semantic_tokens`
  → `provider_caps_to_lua` so `client.server_capabilities.semanticTokensProvider`
  reads truthy (Phase 1 already plumbs the bool; this exposes it to Lua).
- `crates/bemtvi-lua/src/prelude/lsp.lua` — `vim.lsp.semantic_tokens` table:
  `start(bufnr, client_id, opts)` / `stop(bufnr, client_id)` →
  `btv._lsp_semantic(bufnr, on)` ops enabling/disabling the per-buffer projection;
  `force_refresh(bufnr)` → re-request; `get_at_pos(bufnr, row, col)` → the cached
  token(s) under a position (read from the mirror). `highlight_token` (the
  per-token highlight-customization callback) stays `btv._notimpl` for v1 — it
  needs a Lua callback on the decode hot path, out of scope — recorded as a loud
  gap.
- `crates/bemtvi-lua/src/ops.rs` + `crates/bemtvi-server/src/lsp/sync.rs` — the
  enable/disable/refresh ops; a per-buffer `semantic_enabled` flag on `LspDocState`
  consulted by the projection (auto-on when the server advertises support, off when
  `stop`ped or globally disabled).
- A global config gate (mirroring `DiagnosticConfig`) so a user can switch semantic
  tokens off editor-wide.

**Approach.** The projection already keys off the cache; `start`/`stop` flip a
per-buffer flag the projection consults (and `start` kicks a request if the cache
is cold), `force_refresh` clears the `result_id` and re-requests full. Auto-enable
is the Phase-1 default (advertised ⇒ request on open); this phase makes it
*controllable*, not *conditional*. `get_at_pos` reads the decoded cache; the
deep-customization hooks fail loud.

**Tests.**
- `vim.lsp.semantic_tokens.stop(bufnr)` removes the semantic paint (the
  `highlights` key drops back to the treesitter-only spans); `start` restores it;
- `force_refresh` re-issues a full request (mock-recorded);
- `client.server_capabilities.semanticTokensProvider` is truthy for a server that
  advertised the legend, falsy for one that didn't;
- the global gate off ⇒ no semantic paint anywhere.

**Done when.** ✅ `vim.lsp.semantic_tokens.start`/`stop`/`force_refresh`/`get_at_pos`
drive the per-buffer projection from user Lua, `semanticTokensProvider` is readable
on `client.server_capabilities`, an editor-wide gate disables the feature, and the
genuinely-unimplemented `highlight_token` raises through `btv._notimpl` rather than
no-op.

The capability exposure threads `ProviderCaps.semantic_tokens` (Phase 1) →
`LspServerCapabilities.semantic_tokens` (`provider_caps_to_lua`) →
`caps.set("semanticTokensProvider", …)` in `LuaRuntime::set_lsp_client`, so
`client.server_capabilities.semanticTokensProvider` reads truthy for a server that
advertised a legend, falsy otherwise. The control surface
(`crates/bemtvi-lua/src/prelude/lsp.lua`) resolves `0`/`nil` → the current buffer
and enqueues an `LspOp`: `start`/`stop` → `LspOp::SemanticTokensEnable { bufnr,
enabled }` (sets the per-buffer `LspDocState::semantic_enabled` override —
`Some(false)` hides the paint, the cache surviving so `start` repaints without a
round-trip, and `start` re-requests a cold cache); `force_refresh` →
`LspOp::SemanticTokensRefresh` (drops the cached `result_id`, re-requests `full`);
`enable` → `LspOp::SemanticTokensConfig { enabled }` (the editor-wide
`Server::semantic_tokens_enabled` gate, re-requesting every attached buffer when
flipped back on). Both `semantic_intervals` (the projection) and
`request_semantic_tokens` consult the global gate and the per-buffer flag.
`get_at_pos` reads a new Rust→Lua mirror `btv._semantic_tokens[bufnr]` (the
diagnostics-mirror analogue): `on_semantic_tokens_reply` pushes the decoded tokens
(`{ line, start_col, end_col, type, modifiers, client_id }`, byte columns) via
`LuaRuntime::set_semantic_tokens` each reply, built from the `SemanticSpan`'s new
`ty`/`mods` fields. `highlight_token` raises `btv._notimpl`. Verified by
`stop_hides_the_paint_and_start_restores_it` /
`the_editor_wide_gate_off_hides_all_semantic_paint` /
`force_refresh_re_issues_a_full_request` /
`server_capabilities_reports_the_semantic_tokens_provider` /
`server_without_a_legend_reports_no_semantic_tokens_provider` /
`get_at_pos_returns_the_token_under_the_position`
(`crates/bemtvi/tests/lsp/semantic.rs`). The demo's `on_attach` branches on
`client.server_capabilities.semanticTokensProvider`, with keymaps for
stop/start/force_refresh/get_at_pos (`examples/semantic-tokens/`).

*Known approximations:* `get_at_pos` reads the cached mirror regardless of the
per-buffer enable flag (a `stop`ped buffer still answers from its surviving cache —
neovim returns nothing once a buffer is stopped); auto-enable is the default and
`start`/`stop` are a per-buffer override (no per-client granularity — `client_id`
is accepted but bemtvi keeps one semantic cache per buffer); `highlight_token` is a
loud gap.

**Depends on.** Phase 1 (the projection + cache). Independent of Phase 2.

---

## Known approximations to expect

- **One group per cell.** bemtvi's `cell_style` takes the winning span and does not
  blend stacked hl_groups; a token paints its single most-specific resolvable
  `@lsp.*` group, not neovim's stack of `@lsp.type.<t>` + one
  `@lsp.mod.<m>` / `@lsp.typemod.<t>.<m>` per modifier. (Same single-winner model
  the treesitter/extmark merge already uses.)
- **Theme-gated.** Semantic paint appears only where the active theme defines the
  `@lsp.*` group; with no definitions the treesitter floor shows through (this is
  also the safety rule that keeps an undefined-group token from blanking a cell).
- **No `range` requests.** Only `full` (Phase 1) and `full/delta` (Phase 2);
  `semanticTokens/range` (the viewport-scoped request neovim uses for huge files)
  is unsupported — the whole document is tokenized.
- **`highlight_token` is a loud gap.** The per-token highlight-customization
  callback raises `btv._notimpl` (it would put a Lua call on the decode path).
- **`update_in_insert`-equivalent is always on.** Tokens repaint as soon as a reply
  lands, including mid-insert; neovim can defer. Out of scope.

## Suggested order

`1 → 2 → 3`. Phase 1 carries the legend-decode + the third-source merge both later
phases lean on; 2 (wire efficiency) and 3 (the Lua surface) are independent of each
other. Each phase ships with a runnable `examples/semantic-tokens/` config + sample
file proving the surface end to end (extending the same example across phases).
