# Markdown rendering for read-only popups (Option A, pulldown-cmark)

Status: **implemented** (phases 1–4) · 2026-07-04

Implementation notes vs the plan:
- **Renderer** lives in `crates/bemtvi-core/src/markdown.rs`; `render(src)` (no width
  param — thematic breaks / table separators emit `MdFill`s the caller expands).
- **Lua surface** `btv.markdown.render(src) → { lines, highlights, fills }`
  (char-column spans, `@markup.*` groups) over the native `btv._markdown_render`.
- **Hover** renders through `Editor::open_markdown_float` (`DOC_MD_NS` extmarks +
  per-fence `preview_highlights`).
- **Completion docs sidebar** renders too, but **text-only** (stripped, uncolored):
  that surface's wire has no highlight channel, so coloring it would need a wire +
  3-client change — left as future work.
- **Scope correction:** there is no "picker markdown preview" to convert — the picker
  preview is file/location-only. The only markdown-content surfaces are hover and the
  docs sidebar.
- **Phase 4** (tables / task lists / block-quote bar / thematic-rule fills / link-URL)
  all landed in the renderer; shipped `examples/markdown/` (renders a buffer into an
  `btv.ui.float` popup) as the end-to-end example.

## Goal

Render markdown *properly* in bemtvi's read-only doc popups — starting with **LSP
hover** — instead of showing literal `**bold**`, `# heading`, ` ``` ` fences and
unaligned `|` tables. We parse the markdown **at ingest** into (a) stripped
display lines and (b) a set of highlight / decoration extmarks, then render it
through the float + extmark + off-buffer-syntax plumbing that already exists. No
wire-protocol change; no new dependency on an installed tree-sitter markdown
grammar.

This is **Option A** from the design discussion: a native renderer at the
existing `markup_lines` seam. It is popups-only.

## Non-goals

- Rendering markdown while *editing* `.md` buffers. That needs a `conceal`
  primitive in core + the redraw protocol + all three clients (Option B) and is a
  separate, larger project.
- Signature help styling — it renders code in the source language, keeps its
  current source-filetype float, and is untouched here.
- Images, and full box-drawn table borders. Table *column alignment* is in scope
  (phase 4); ornate borders are a later polish.

## Why this seam

Confirmed in the code today:

- `crates/bemtvi-lsp/src/convert.rs:228` `markup_lines()` only splits the server's
  markdown on `\n` and decodes HTML entities — its own doc comment flags styling
  as *"a follow-up, tracked with hover."* So `LspReply::Hover(Vec<String>)`
  already carries **raw markdown**, line-split.
- Both the native path (`bemtvi-lsp/src/dispatch.rs:216`) and the wasm sync path
  (`bemtvi-lsp/src/sync_client.rs:739`) build `LspReply::Hover` and both funnel
  into the **single** render chokepoint `EditHost::show_hover`
  (`crates/bemtvi-server/src/lsp/request.rs:496`), which calls
  `Editor::open_doc_float(name, lines, "markdown")`
  (`crates/bemtvi-core/src/editor/float.rs:192`).
- The doc float is a **real, scrollable, non-focusable window over a scratch
  buffer**. Its highlights come from the buffer's filetype tree-sitter pass —
  which requires an *installed* markdown grammar (bemtvi bundles none), and even
  then leaves the markup characters literal.
- The primitives we need already exist: range **highlight extmarks** with an
  `hl_group` (`ExtmarkStore::set`, `crates/bemtvi-core/src/extmark.rs:200`; used by
  the listing panel at `panel.rs:185`), `virt_lines` / `line_fill` decor
  (`VirtDecor`, `extmark.rs:134`), and a **stateless off-buffer highlighter**
  `Editor::preview_highlights` → `SyntaxEngine::highlight_text`
  (`crates/bemtvi-core/src/editor/syntax.rs:290`) that the picker preview uses to
  color arbitrary text in a language.
- The redraw wire already ships per-line highlight spans as
  `[start, end, group, style_id]` for windows (same shape as the preview), so
  **no protocol or client change is required** — we only need to produce spans on
  the core side.

Rendering at `show_hover` (not in `markup_lines`) means it works regardless of how
the `Vec<String>` was produced (native vs wasm), keeps `bemtvi-lsp` free of the
markdown dep, and leaves the transport type unchanged.

## Dependency

Add to root `Cargo.toml` `[workspace.dependencies]`, pinned exactly:

```toml
pulldown-cmark = { version = "=0.12.2", default-features = false }   # confirm latest patch
```

- `default-features = false` drops its `getopts`/CLI bits; we only want the
  pull parser. It is pure Rust with no I/O, so it satisfies the
  "`bemtvi-core` stays pure and synchronous" rule and compiles for
  `wasm32-unknown-emscripten` (verify in phase 2's build check).
- Pull into `bemtvi-core` with `pulldown-cmark.workspace = true`.
- Enable GFM via `Options::ENABLE_TABLES | ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS`.

## Where the renderer lives

New **pure** module `crates/bemtvi-core/src/markdown.rs`, a sibling to `buffer.rs`.
Core is the right home: `open_doc_float`, `preview_highlights`, and the extmark
store are all in core, and core is shared by native + wasm so the renderer serves
every front end. (A standalone crate would tempt `#[test]` unit tests, which the
repo bans — behavior is verified end-to-end instead.)

### Public shape

```rust
// crates/bemtvi-core/src/markdown.rs
pub struct Rendered {
    pub lines: Vec<String>,          // stripped display text, wrapped to `width`
    pub spans: Vec<MdSpan>,          // inline highlight ranges (byte offsets within a line)
    pub fills: Vec<MdFill>,          // whole-line fills (horizontal rules)
    pub code: Vec<MdCode>,           // fenced blocks, for the caller to ts-highlight
}
pub struct MdSpan { pub line: usize, pub start: usize, pub end: usize, pub group: &'static str }
pub struct MdFill { pub line: usize, pub ch: char, pub group: &'static str }
pub struct MdCode { pub first_line: usize, pub len: usize, pub lang: Option<String> }

/// Render CommonMark+GFM `src` into stripped lines + styling, wrapping prose to
/// `width` columns. Pure; never fails — any construct we don't specially style
/// still contributes its text (no silent drops).
pub fn render(src: &str, width: usize) -> Rendered;
```

Offsets are **byte** offsets within a line (extmarks are byte-based). The walker
tracks the current line string, a byte cursor, and a stack of open inline styles;
on `Event::Text` it appends text and, on the matching `End`, emits an `MdSpan`
from the style's start byte to the current byte.

### Markup → highlight-group map

Emit neovim's `@markup.*` capture names so existing colorschemes style them with
zero extra config (they resolve through the same `resolve_capture` → `StyleTable`
path as buffer highlights, including bold/italic attributes):

| Markdown construct        | display transform                    | hl group (`&'static str`)        |
|---------------------------|--------------------------------------|----------------------------------|
| `#`..`######` heading     | drop the `#`s + space                | `@markup.heading.1` … `.6`       |
| `**strong**`              | drop the `**`                        | `@markup.strong`                 |
| `*emphasis*` / `_em_`     | drop the marker                      | `@markup.italic`                 |
| `~~strike~~`              | drop the `~~`                        | `@markup.strikethrough`          |
| `` `inline code` ``       | drop the backticks                   | `@markup.raw`                    |
| fenced ` ```lang ` block  | drop the fence lines; keep body      | per-fence ts (see below)         |
| `[text](url)`             | keep `text` (URL dimmed in phase 4)  | `@markup.link.label`             |
| `- ` / `* ` list item     | replace marker with `•` + space      | `@markup.list`                   |
| `1.` ordered item         | keep the number                      | `@markup.list`                   |
| `> ` block quote          | prefix `▎ ` (phase 4)                | `@markup.quote`                  |
| `---` thematic break      | `line_fill` of `─` across width      | `@punctuation.special`           |

Group names live in one `const` table in `markdown.rs` so the set is auditable.

## Rendering the hover float

New core method (extract the sizing/placement half of `open_doc_float` into a
shared helper so both entry points reuse it):

```rust
// crates/bemtvi-core/src/editor/float.rs
pub fn open_markdown_float(&mut self, name: &str, src: &str) {
    let r = markdown::render(src, MAX_W);          // MAX_W already = 80 here
    // load r.lines into the reused scratch buffer; leave filetype EMPTY
    //   (we already stripped syntax — do NOT re-type it "markdown")
    // for each MdCode: self.preview_highlights(lang, block_text, ..) → byte spans
    // set every span (ours + code) as an hl_group range extmark under DOC_MD_NS
    // set r.fills via VirtDecor::line_fill
    // size + place + open the float window (shared helper), enter = false
}
```

- New reserved namespace `DOC_MD_NS` in `extmark.rs` (next id below
  `LISTING_HL_NS`), cleared on each re-open like the panel does.
- Fenced code: `MdCode.lang` → `preview_highlights(lang, text, first_line, len)`;
  degrades to plain when that grammar isn't installed (fail-soft is correct here —
  the text still shows). This is the *same* mechanism the picker preview uses.
- The buffer is **not** typed `markdown` anymore, so we no longer depend on an
  installed markdown grammar for the markup styling — a strict improvement over
  today.

Route the hover through it:

```rust
// crates/bemtvi-server/src/lsp/request.rs  show_hover()
self.editor.open_markdown_float("[Hover]", &lines.join("\n"));
```

`open_doc_float` stays for signature help (source-language code, not markdown).

## Lua surface (plugin API + test hook)

Expose the pure renderer so plugins can reuse it and tests can drive it without a
live LSP:

```lua
-- returns { lines = {..}, highlights = { { line, col_start, col_end, group }, .. } }
btv.markdown.render(markdown_string)
```

Backed by a native `btv._markdown_render` in the Lua prelude bridge (columns as
1-based char columns for Lua ergonomics, converted from the byte spans). Add to
`crates/bemtvi-lua/src/prelude/` and register the native fn in the server bridge.

## Phases

**Phase 1 — renderer + Lua surface (pure, no float).**
- Add the dep; write `markdown.rs` `render()` + the group table; wire
  `btv.markdown.render`.
- Tests (black-box, via `exec_lua`): feed strings with heading / strong / emphasis
  / inline code / list / link / fenced block; assert `lines` are stripped (no `#`,
  `*`, backticks) and `highlights` carry the expected `@markup.*` groups at the
  right columns. Assert prose wraps at the given width.

**Phase 2 — wire hover to the rendered float.**
- Extract the float sizing/placement helper; add `open_markdown_float` + `DOC_MD_NS`;
  route `show_hover`.
- Fenced-code ts highlighting via `preview_highlights`.
- Tests (via the mock LSP: `bemtvi --__lsp-mock`, `$BEMTVI_LSP_CMD`, `await_float` /
  `window_lines` in `crates/bemtvi/tests/lsp_config.rs` & `lsp_stderr.rs`): a hover
  reply with `# Title`, `**bold**`, and a ` ```rust ` fence renders a float whose
  lines are stripped and whose redraw carries `@markup.heading.1` / `@markup.strong`
  highlight spans and Rust highlights inside the fence.
- **Build both configs**: `cargo build -p bemtvi-server` (default) and
  `-p bemtvi-server --no-default-features` (wasm edit-host) to confirm
  pulldown-cmark compiles wasm-side.

**Phase 3 — reuse for the other popups.**
- Completion docs sidebar (`redraw.rs` `project_complete_docs` /
  `complete_doc_lines`) and the picker **markdown-kind** preview render through the
  same `markdown::render`. Tests assert stripped lines + groups in each surface.

**Phase 4 — polish (optional, incremental).**
- GFM table column alignment; block-quote bar (`▎` via prefix or `virt_text`);
  thematic-rule `line_fill`; dim the URL of `[text](url)`; task-list `☐`/`☑`.

## Testing & conventions

- Black-box only, per repo policy — no `#[test]` unit tests in the crates. Phase 1
  exercises the pure renderer through `btv.markdown.render` (`exec_lua`); phases 2–3
  through the running server + redraw assertions.
- **Fail-loud**: `render` never silently drops content — any unstyled construct
  still emits its text. Fenced-code highlighting fail-soft (plain when the grammar
  is absent) is intentional and distinct from dropping content.
- Redraw helpers take the **latest** frame (`drain_to_latest_redraw`), per the
  harness rule.
- Example config per repo convention: `examples/markdown-hover/` with an init that
  configures a language server (or a tiny `btv.markdown.render` demo command) and a
  sample doc, verified end-to-end.

## Risks / decisions

- **Renderer in core, not a new crate** — keeps it pure + wasm-shared and avoids
  the unit-test temptation the no-unit-test rule forbids.
- **pulldown-cmark on wasm** — pure Rust, expected to compile for
  `wasm32-unknown-emscripten`; the phase-2 `--no-default-features` build is the
  gate. If it ever regressed, the fallback is the tree-sitter markdown grammar as
  an off-buffer highlighter (`preview_highlights`), but that reintroduces the
  installed-grammar dependency, so pulldown is preferred.
- **We stop typing the float buffer `markdown`** — intentional; styling now comes
  from our spans, not an optional grammar.
- **Byte vs char offsets** — extmarks are byte-based; the renderer emits byte
  offsets within a line; the Lua surface converts to 1-based char columns.
- **Wrapping** — prose wraps to `MAX_W` (80) carrying active spans across wraps;
  code blocks and tables don't wrap (the float scrolls).
