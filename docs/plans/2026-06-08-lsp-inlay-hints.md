# LSP inlay hints — completion plan

> **Status: Phase 1 complete ✅. Phase 2 caps/`get`/resolve complete ✅; the
> `range` (viewport-scoped) request is deferred ⬜ (see below).**
> Inlay hints are the inline `: i32` type annotations and `name:` parameter
> labels a server injects *between* the buffer's own glyphs. They are the third
> async, server-side **decoration** projection — after the diagnostic
> virtual-text/signs and the semantic-token highlight floor — but the first that
> renders *inline*: the hint text is inserted mid-line and pushes the real glyphs
> (and the cursor) to the right, rather than sitting at end-of-line (diagnostic
> virt-text) or recoloring an existing cell (semantic tokens).

## Why this document exists

`textDocument/inlayHint` is, today, only a relayable method string in the
`dyn_requests!` table (`crates/nxvim-lsp/src/dispatch.rs`) — a config could
`client:request` it, but nxvim has no *feature* behind it: no `LspReqKind`, no
cache, no projection, no render surface, and `vim.lsp.inlay_hint` is a nil index.
Inlay hints are the most-requested still-missing everyday LSP surface after
diagnostics and semantic tokens: a real nvim 0.10+ setup shows `let x: i32 = …`
and `foo(count: 3)` the moment you call `vim.lsp.inlay_hint.enable(true)`.

The fail-loud, no-silent-stub rule from the
[LSP completion plan](2026-06-05-lsp-completion.md) applies: a part of the API we
don't honor yet stays a documented approximation or raises through
`nx._notimpl` — never a silent no-op.

## What's already in place (the seams these phases extend)

- **The decoration projection.** `Server::diagnostics_virt_text_for`
  (`crates/nxvim-server/src/lsp/diagnostics.rs`) already projects a per-row
  inline decoration as `[text, …, style_id]` under a window key, and the
  semantic-tokens path (`crates/nxvim-server/src/lsp/semantic.rs`) shows the
  buffer-scoped request/decode/cache shape (request on open+change, stale-drop on
  `tick`, decode char→byte against the negotiated encoding, bucket by line).
  Inlay hints reuse both: the semantic request/cache shape and a *positioned*
  variant of the virt-text projection.
- **Per-buffer LSP state.** `LspDocState` (`crates/nxvim-server/src/lsp/mod.rs`)
  holds the `diagnostics` and `semantic` caches keyed by `BufferId`; the inlay
  cache is a new field beside them, with a per-buffer `inlay_enabled` flag
  (default **off** — unlike semantic tokens, neovim's inlay hints are opt-in via
  `vim.lsp.inlay_hint.enable`).
- **The typed request/reply machinery.** One `LspReqKind`, one `LspRequest`, one
  `LspReply` variant, normalized in `dispatch.rs` — exactly the semantic-token
  shape, but the request carries a whole-buffer `range` (inlay hints are a
  range request; we send `0..line_count`).
- **The render walk.** `highlight_line` (`crates/nxvim-tui/src/render.rs`) walks
  each row's cells keyed on the **absolute** screen column, so the selection /
  search / highlight / diagnostic overlays all resolve per real glyph. Inserting
  an inline hint span into that stream keeps every overlay correct for free
  (styles travel with each glyph); the only coordinate that needs the inline
  shift is the **terminal cursor** (painted separately from `cursor_screen_col`).
- **The scripted mock + redraw harness.** `nxvim-lsp/src/mock.rs` scripts replies
  per method; an `inlay_hints` script field + an `inlay_hints`-key redraw
  assertion is the test shape (mirroring `semantic_tokens`).

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 1 — Enable + inline paint (the core bridge) ✅

**Goal.** `vim.lsp.inlay_hint.enable(true[, { bufnr }])` turns a buffer's hints
on; the server requests `textDocument/inlayHint` for the whole buffer on enable
and after every change, decodes the reply against the negotiated encoding, and
the client paints each hint **inline** at its column — pushing the real text and
the cursor right — colored by the `LspInlayHint` group (a dim built-in fallback
when the theme leaves it undefined). `is_enabled` reports the state.

**Why.** This is the entire user-visible payoff. `get`, `inlayHint/resolve`
(lazy labels, tooltips, clickable label-part locations), and `range` requests
(Phase 2) only read or refine what Phase 1 paints.

**Scope (files).**
- `crates/nxvim-lsp/src/client.rs` — advertise
  `text_document.inlay_hint` (`InlayHintClientCapabilities`, `resolveSupport` for
  `label`/`tooltip`/… declared so the server offers lazy hints); add
  `ProviderCaps.inlay_hints` set by a `present("inlayHintProvider")` probe.
- `crates/nxvim-lsp/src/protocol.rs` — `LspRequest::InlayHint { uri, range }`;
  `LspReply::InlayHints(Vec<InlayHintData>)`; `InlayHintData { line, character,
  label, kind, padding_left, padding_right }` (position in the negotiated
  encoding, `label` the string form — label parts joined — `kind` 1=Type/2=Param/
  0=unset). `ProviderCaps.inlay_hints`.
- `crates/nxvim-lsp/src/dispatch.rs` — issue `inlay_hint(params)` and normalize
  the `InlayHint[]`/`null` reply to `Vec<InlayHintData>` (label-part fold,
  padding, kind); `describe_request` arm.
- `crates/nxvim-server/src/lsp/mod.rs` — `LspReqKind::InlayHints`; an
  `InlayHintsCache` on `LspDocState` (per-line `Vec<InlayHintSpan { byte_col,
  text, kind }>`); a per-buffer `inlay_enabled: bool` (default off);
  `ProviderCaps.inlay_hints` → `provider_caps_to_lua`.
- `crates/nxvim-server/src/lsp/inlay.rs` *(new)* — `request_inlay_hints(buffer)`
  (gated on the buffer being enabled and its server advertising the provider;
  buffer-scoped, whole-buffer range, stale-dropped on `tick`),
  `on_inlay_hints_reply` (decode char→byte, build the per-line text, bucket),
  and `inlay_hints_for(buffer, numbers, styles)` → per-row
  `[[virtcol, text, style_id], …]` sorted by column (virtcol via the same
  tab/wide-char mapping the diagnostics underline uses).
- `crates/nxvim-server/src/lsp/{request.rs,sync.rs}` — register/dispatch the
  request and reply (reuse the semantic stale-drop path); fire
  `request_inlay_hints` from `sync_lsp` on the same `content_synced` trigger as
  semantic tokens, and on the enable op.
- `crates/nxvim-server/src/redraw.rs` — project under an `inlay_hints` window key.
- `crates/nxvim-view/src/{parse.rs,view.rs}` — `InlayHint = (u16 col, String
  text, Option<usize> style_id)`; `WindowView.inlay_hints: Vec<Vec<InlayHint>>`;
  `parse_inlay_hints`.
- `crates/nxvim-tui/src/render.rs` — thread the row's hints into
  `highlight_line`; insert each hint's span at its screen column (before the real
  glyph there, after `leftcol` clipping), advancing the painted-width counter so
  the trailing diagnostic virt-text still clears the text; compute the terminal
  cursor's inline shift (sum of hint widths at columns `≤ cursor_screen_col` on
  the cursor row) in `render_window`/`render`. `inlay_hint_style` (palette id or a
  dim fallback).
- `crates/nxvim-lua/src/{ops.rs,install.rs}` + `prelude/lsp.lua` —
  `vim.lsp.inlay_hint.enable(enable, filter)` / `is_enabled(filter)` →
  `nx._lsp_inlay_hint_enable(bufnr, enabled)` → `LspOp::InlayHintEnable`, with a
  Lua-side `nx._inlay_hint_enabled[bufnr]` mirror so `is_enabled` is pure Lua.
- `crates/nxvim-lsp/src/mock.rs` — an `inlay_hints` script field (an
  `InlayHint[]`) returned for `textDocument/inlayHint`, and the
  `inlayHintProvider` capability when scripted.

**Approach.**
- **Opt-in.** Default off per buffer. `request_inlay_hints` returns early unless
  the buffer is enabled (and the server advertises the provider); the projection
  returns empty unless enabled, so a disabled buffer renders exactly as today.
- **Whole-buffer range.** `InlayHintParams.range = (0,0)..(line_count, 0)`. A
  viewport-scoped range is a Phase-2 follow-up (recorded as an approximation).
- **Decode.** Each hint's `position.character` → line-local byte through the
  negotiated encoding (the shared `byte_col`); `text = pad_left? " " : "" + label
  + pad_right? " " : ""`; bucket by line, sorted by column.
- **Inline render.** The hint span is inserted into the paint stream at its
  absolute screen column; the real glyphs after it shift right automatically (the
  spans are emitted in order). Overlay styles stay keyed on the original absolute
  column, so selection/highlight/diagnostic spans remain correct per glyph. The
  cursor, painted from `cursor_screen_col`, is shifted right by the summed widths
  of hints at columns `≤ cursor_screen_col` on its row.

**Tests** (`crates/nxvim/tests/lsp/` via the scripted mock + redraw, plus a
Tier-2 paint test under `crates/nxvim-server/tests/`):
- enabling inlay hints paints a scripted hint inline at its column in the
  `inlay_hints` redraw key (and on the rendered grid);
- the default (not enabled) shows nothing there;
- the decode is encoding-correct (a UTF-16 server's char offset over a wide-char
  line lands on the right byte/screen column);
- editing the buffer re-requests and repaints (a second scripted set lands after
  a `didChange`);
- the inline hint shifts the following text (and the cursor) right on the grid.

**Done when.** ✅ A buffer with `vim.lsp.inlay_hint.enable(true)` shows the
server's hints inline; disabled (the default) renders as today; the decode is
encoding-correct; an edit re-requests and repaints; the paint shifts the text and
cursor right. The client capability is advertised in `client_capabilities()`;
`ProviderCaps.inlay_hints` is captured onto `ServerRuntime.inlay_hints` at
`Initialized`; the typed `LspRequest::InlayHint { uri, range }` /
`LspReply::InlayHints(Vec<InlayHintData>)` are issued by `request_inlay_hints`
(`crates/nxvim-server/src/lsp/inlay.rs`, gated on the per-buffer `inlay_enabled`
flag and the server's provider) on enable + after each `didChange`, normalized in
`dispatch.rs` (label-part fold + padding + kind). The reply is stale-dropped on
the issuing buffer's `tick`, decoded char→byte against the negotiated encoding
into `InlayHintsCache.hints` on `LspDocState`, and projected by `inlay_hints_for`
under the `inlay_hints` window key as `[col, text, style_id]` (screen columns).
The TUI `highlight_line` splices each hint's span into the paint stream at its
column (`emit_inlay_hint`), shifting the real glyphs right; `inlay_cursor_shift`
moves the terminal cursor by the same width. The Lua surface
`vim.lsp.inlay_hint.enable(enable, filter)` / `is_enabled(filter)` →
`LspOp::InlayHintEnable` flips the per-buffer flag (with a `nx._inlay_hint_enabled`
Lua mirror for `is_enabled`). Verified by `inlay_hints_paint_when_enabled` /
`inlay_hints_are_off_by_default` / `inlay_hint_columns_are_encoding_correct` /
`editing_re_requests_inlay_hints` / `an_inlay_hint_shifts_the_text_right_on_the_grid`
(`crates/nxvim/tests/lsp/inlay.rs`). Runnable demo: `examples/inlay-hints/`.

*Known approximations:* off-by-default and per-buffer enable only (no per-client
granularity); whole-document only (no `range`); the **string** label form (label
parts are joined to their `value`s — no clickable per-part `location`s, no
tooltip, no `textEdits`-on-accept); lazy hints needing `inlayHint/resolve` show
their unresolved label (resolve is Phase 2); one `LspInlayHint` group for all
kinds (no `LspInlayHintType`/`Parameter` split); horizontal scroll (`leftcol>0`)
+ inline hints is best-effort (the cursor shift ignores scrolled-off hints);
repaints on every reply including mid-insert (`update_in_insert` always on).

**Depends on.** Nothing (extends the existing projection + request machinery).

---

## Phase 2 — caps / `get` / resolve ✅ (range deferred ⬜)

**Goal.** `client.server_capabilities.inlayHintProvider` reads truthy;
`vim.lsp.inlay_hint.get(filter)` reads the cached hints under a position/range
from a Rust→Lua mirror; and `inlayHint/resolve` fills a **lazy** hint's `label` on
demand. The viewport-scoped `range` request is **deferred** — see below.

**Why.** `get` is the read half of the surface configs call; resolve is what makes
a server's lazy hints (an empty label + `data`) actually paint; the cap is what an
`on_attach` branches on to bind inlay keymaps.

**What shipped.**
- **Caps.** `ProviderCaps.inlay_hints` (already captured at `Initialized` in
  Phase 1) now flows through `provider_caps_to_lua` →
  `LspServerCapabilities.inlay_hints` → `set_lsp_client` as the
  `inlayHintProvider` key on `client.server_capabilities`, beside
  `semanticTokensProvider`.
- **`get`.** A `nx._inlay_hints[bufnr]` mirror (the `nx._semantic_tokens`
  analogue) is pushed on every reply, after a resolve fills a label, and cleared
  on disable — built by `inlay_mirror` (`lsp/inlay.rs`) and set via
  `LuaRuntime::set_inlay_hints` (`InlayHintMirrorData`). `vim.lsp.inlay_hint.get`
  reads it, resolving `filter.bufnr` and optionally narrowing to `filter.range`,
  and returns neovim's `{ bufnr, client_id, inlay_hint = { position, label,
  kind } }` shape (byte columns — the diagnostics/semantic-mirror convention).
- **Resolve.** `LspRequest::ResolveInlayHint { hint }` /
  `LspReply::ResolvedInlayHint { label }` (`protocol.rs`) round-trip the original
  hint and bring back the resolved label, distilled in `dispatch.rs` via
  `sock.inlay_hint_resolve`. A lazy hint (`inlay_hint()` sets `resolve_data` when
  the joined label is empty **and** the hint carried `data`) is cached as an empty
  placeholder; `issue_inlay_resolves` fires one `inlayHint/resolve` per placeholder
  after the per-line sort, recorded in `Server.inlay_resolves` keyed by the
  `cb_id` its token carries (so concurrent resolves don't collide in the
  single-slot kind-map — they route by `cb_id` in `on_lsp_event` like a generic
  `client:request`). `on_inlay_hint_resolved` fills the span's `text`, guarded by
  the buffer's `tick`. The projection + mirror both skip empty-`text` placeholders,
  so an unresolved hint paints nothing.

**Tests** (`crates/nxvim/tests/lsp/inlay.rs`):
`inlay_hint_provider_capability_is_truthy` /
`server_without_inlay_hints_reports_no_provider` (caps),
`get_returns_cached_inlay_hints` (get + range filter + disabled→empty),
`a_lazy_inlay_hint_label_resolves` (the placeholder paints nothing until the
scripted `inlay_resolve` fills its label, then it paints and reads back via
`get`). The shipped example is guarded by
`crates/nxvim-lua/tests/inlay_example_load.rs`.

**Done when.** ✅ The caps/`get`/resolve surface is live (above). Runnable demo:
`examples/inlay-hints/` (the `on_attach` now gates on
`server_capabilities.inlayHintProvider`; `<leader>ic` reads the cursor line's
hints back through `get`).

### Making it work with a real server (the two LSP enablers)

The surface above is necessary but not sufficient for a *real* server — testing
the example against `lua-language-server` surfaced two general LSP gaps that left
hints "enabled but empty". Both are fixed (in `nxvim-lsp/src/client.rs` +
`nxvim-server/src/lsp/sync.rs`), and are general — they also benefit semantic
tokens, diagnostics, and any settings-driven server (gopls, …):

- **`workspace/configuration` (pull config).** lua_ls reads its `hint.enable`
  *only* by requesting `workspace/configuration` — it ignores the
  `didChangeConfiguration` push for those options. nxvim now advertises
  `workspace.configuration` and answers each requested `section` with that dotted
  path into the config's `settings` (`configuration_reply` / `config_section`).
  Without it, lua_ls produced **zero** hints (verified directly against the real
  server). Covered by
  `attach::workspace_configuration_pull_is_answered_from_settings`.
- **`workspace/{inlayHint,semanticTokens}/refresh`.** A server that computes
  decorations asynchronously returns nothing on the first request and signals
  readiness with a refresh request. nxvim now advertises `refreshSupport` and, on
  a refresh, re-issues the whole-buffer request for every buffer that server owns
  (`on_workspace_refresh`, via the new `LspEvent::WorkspaceRefresh`). Covered by
  `inlay::inlay_hints_appear_after_workspace_refresh`.

The real round-trip is guarded end to end by
`real_server::real_lua_ls_inlay_hints_appear` (skips when `lua-language-server`
isn't installed).

**Depends on.** Phase 1 (the cache + projection).

### Deferred: the `range` (viewport-scoped) request ⬜

The whole-buffer `InlayHintParams.range = 0..line_count` from Phase 1 stays. A
viewport-scoped request — re-fetching only the window's visible line span, and
re-requesting on **scroll** — was scoped out: it changes the request trigger model
(scroll becomes an LSP trigger) and turns the per-buffer cache into a partial /
windowed one (merge instead of replace). It is a pure optimization for very large
files; the whole-buffer fetch is already correct. Recorded here as the remaining
Phase-2 follow-up.

---

## Known approximations to expect

- **Off by default, per-buffer.** Neovim's inlay hints are opt-in; nxvim matches
  that, and `enable`/`is_enabled` are per-buffer (a `filter.bufnr`), no
  per-client split.
- **String labels only.** Label *parts* are joined to their `value`s. Resolve
  (Phase 2) fills a lazy hint's **label**, but the per-part `location` (go-to on
  click), `tooltip`, and `textEdits`-on-accept are still dropped — nxvim renders
  label-only (no hover/click on a hint yet).
- **One group, no kind split.** All hints paint `LspInlayHint`; neovim also has
  `LspInlayHintType`/`Parameter`. Theme-gated like the other decorations (a dim
  built-in fallback when the group is undefined).
- **`get` exposes byte columns.** `vim.lsp.inlay_hint.get` returns
  `inlay_hint.position.character` as a **byte** column (the mirror's decoded
  form), not the server's encoding-native character — consistent with the
  diagnostics / `semantic_tokens.get_at_pos` mirrors.
- **Whole document, no `range`.** The viewport-scoped request is deferred (see
  *Phase 2 → Deferred*); the whole buffer is requested on every change.
- **`update_in_insert`-equivalent is always on.** Hints repaint as soon as a
  reply lands, including mid-insert.

## Suggested order

`1 → 2`. Phase 1 carried the request/decode/cache + the inline-render capability
both the `get` mirror and resolve build on. Phase 2 (caps/`get`/resolve) is done;
the `range` request is the remaining deferred follow-up. Each phase ships with a
runnable `examples/inlay-hints/` config + sample file proving the surface end to
end.
