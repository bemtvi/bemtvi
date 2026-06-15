# `nx.snippet` — the native snippet engine — phased plan

> Implements the pending half of completion **Phase 4-D**
> (`docs/plans/2026-06-15-nx-complete-completion-engine.md` → *Snippets*) and the
> `nx.snippet` surface from the native-plugin-API spec
> (`docs/specs/2026-06-11-native-plugin-api.md` §4). First snippet engine in the
> repo — none exists today. **No silent stubs**: unsupported snippet constructs
> error loud rather than inserting raw `$1`.

## Design

The server owns the LSP snippet grammar, expansion, the tabstop session, mirrored
placeholders, and `${1|a,b|}` choices. The tabstop session is modeled on the
multi-cursor *placement* precedent: tabstop occurrences are anchored as
**extmarks** (a reserved `SNIPPET_NS`) so the buffer's single edit choke point
auto-shifts every tabstop/mirror as the user types — no manual offset bookkeeping.
The session lives *alongside Insert mode* (an `Option<SnippetSession>` on the
`Editor`), not as a new `Mode`: snippet editing **is** insert-mode editing, with
`<Tab>`/`<S-Tab>` (configurable) jumping between tabstops. This matches how
LuaSnip/VS Code snippets actually feel and avoids a fourth grab mode.

### Data model

- **Parser** (`nxvim-core/src/snippet.rs`, pure): `parse_snippet(&str) -> Result<ParsedSnippet, SnippetError>`.
  - `ParsedSnippet { text: String, stops: Vec<TabStop> }`
  - `TabStop { index: u32 /* 0 = final $0 */, spans: Vec<Range<usize>> /* spans[0] primary, rest mirrors, byte ranges into text */, choices: Vec<String> }`
  - Supports: `$N`, `${N}`, `${N:default}` (default may nest tabstops), `$0`,
    `${N|a,b,c|}` choices, mirrors (same `N` appearing more than once), `\$ \} \\`
    escapes. **Fails loud** on variables (`$TM_FILENAME`) and transforms
    (`${1/regex/fmt/}`) — `SnippetError::Unsupported`.
- **Session** (`nxvim-core/src/editor/snippet.rs`): `Option<SnippetSession>` on `Editor`.
  - Stops ordered by tab order (1,2,…,N, then 0). Each occurrence is a `SNIPPET_NS`
    range extmark (`hl_group = "SnippetTabstop"`, active one `"SnippetTabstopActive"`).
  - `expand_snippet(anchor, replace_end, parsed)`: re-indent continuation lines to
    the anchor line's indent, replace the range, drop the extmarks, jump to the
    first stop. Folds into the surrounding insert undo group.
  - `snippet_jump(dir)`: move to next/prev stop, place cursor at its primary span.
  - Live mirror sync: after each insert-mode edit inside the active tabstop, copy
    the active primary span's text to its mirror spans.
  - Session ends on `<Esc>` to Normal, on reaching `$0` past the last jump, or when
    an edit lands outside every tabstop range.

## Phases

- **S-1 — Parser** ✅: pure `snippet.rs`, the data model above. Verified through S-2 (black-box).
- **S-2 — Expansion + tabstop session** ✅: `expand_snippet` / `snippet_jump` /
  mirror sync in core; `SNIPPET_NS`; cursor placement; `<Tab>`/`<S-Tab>` in insert.
  View projection of tabstop highlight spans; TUI/GUI render. `nx.snippet.expand("body")`
  test hook.
- **S-3 — `nx.snippet` Lua API + `snippets` source** ✅: `nx.snippet.setup{ jump_next, jump_prev }`,
  `nx.snippet.add(ft, {{trigger, body}})` (string bodies; **function bodies error loud**,
  deferred to S-5), `nx.snippet.expand(body)`, and the built-in `snippets` completion
  source (offers triggers for the buffer's filetype, expands the body on accept).
  `prelude/snippet.lua` + `nx._snippet_*` bridges + the server `snippet.rs` store/source
  + `effects.rs` drains. Feature-agnostic, so it works on the wasm edit-host too.
  Snippet-source rows are keyed by `SNIPPET_COMPLETE_KEY_BASE` so the delegated-accept
  drain routes them to the snippet applier vs the LSP one.
- **S-4 — LSP snippet expansion on accept** ✅: `is_snippet` (`insertTextFormat == 2`)
  carried on `CompletionItemData`; `complete_lsp_accept` expands via the session instead
  of literal insert (applying `additionalTextEdits` first and shifting the primary range),
  falling back to the plain label with a loud message on a parse error. The shared
  `<Tab>`/`<S-Tab>` resolve **snippet-jump-first** while a session is live (the popup
  stays navigable via `<C-n>`/`<C-p>`).
- **S-5 — Choices + select-default + function bodies (deferred)**: `${1|a,b|}` rendered
  through the native menu on jump; placeholder default *selected* on jump (select-mode)
  so typing replaces; `nx.snippet.add` function bodies (dynamic / context-aware); a
  client-distinct active-tabstop colour. The tabstop highlight already renders today
  (the `SnippetTabstop` / `SnippetTabstopActive` extmark groups ride the existing
  extmark→highlight projection — no new view field).

## Verification (each phase)

1. `cargo build --workspace`; `cargo test -p nxvim-server --test snippet` (new suite).
2. `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all -- --check`.
3. Build the wasm edit-host `--no-default-features --features lua51`.
