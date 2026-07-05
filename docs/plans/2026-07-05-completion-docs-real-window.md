# Completion docs sidebar → a real doc-float window

Status: **done** · 2026-07-05

Landed across four commits: the core doc-float infra (`fac6fd18`), completion docs →
window (`feat(complete)`), cmdline wildmenu docs → window (`feat(cmdline)`), and the
client `menu.docs` renderer deletion (`refactor`). Both docs surfaces now render as real
doc-float windows; the `menu.docs` wire is fully retired. Notes vs. the plan below:

- **Core owns placement**, computed from an *immutable* `View::from_editor` (via
  `menu_anchor`) — not the `&mut` `Editor::view`, which can't run on the input path.
  `open_completion_docs_float(md, wrap)` renders + places; the server only sources the
  markdown. Positioning is idempotent (a `CompletionDocsSig`) so a mouse wheel over the
  float keeps its scroll instead of snapping to the top on reopen.
- **Native wheel scroll** is 3 lines/notch (`'mousescroll'` ver:3), replacing the old
  1-line bespoke scroll. Horizontal docs scroll is gone — `docs_wrap` (default on)
  handles wide lines instead.
- Internal doc-float windows are excluded from the lifecycle window diff
  (`is_doc_float_window`) so per-keystroke reopen fires no user window autocmds.
- The docs float uses standard `NormalFloat`/`FloatBorder` chrome (like hover), **not**
  the old sidebar's `CmpDocumentation` theming (`winhighlight` can't express its fallback
  chain). The LSP docs source is no longer `#[cfg(native)]`-gated (the web demo shows
  completion docs too).

---

Status: planned · 2026-07-05

## Goal

Replace the bespoke completion **docs sidebar** (a server-projected `menu.docs`
overlay with its own scroll/hit-test/geometry and a plain-lines-only wire) with a
**real, non-focusable float window over a reused scratch buffer** — the exact model
LSP hover / signature help use (`Editor::open_markdown_float`). This buys:

- **Syntax highlighting for free** — the docs render through the normal window
  `highlights` wire (`DOC_MD_NS` extmarks + per-fence `preview_highlights`), same as
  hover. No new highlight channel, no per-client span renderer.
- **Native scroll** — a real window scrolls with the wheel natively, deleting the
  bespoke `complete_docs_scroll`/`hscroll`/`CompleteDocsHit`/`stash_*` apparatus.
- **Less code, in one place** — deletes `project_complete_docs`, `place_docs_beside`
  (server), the core scroll/hit state, and the `menu.docs` renderers in all three
  clients.

## The division of responsibility (mirrors hover)

The docs *content* is **server-owned** (the selected item's LSP `documentation`,
lazily resolved async; or a plugin row's inline `doc`). So — exactly like hover —
the **server** pushes markdown into the core doc-float buffer, and **core** owns the
window and its placement:

- **Server** decides *what* (source the markdown from the 3 sources
  `project_complete_docs` uses today) and *when* (selection settled / resolve landed /
  menu closed), during **input handling** (a tick), not redraw projection (opening a
  window mutates the tree — can't happen in the read-only redraw pass).
- **Core** owns the window: `open_completion_docs_float(md)` renders via
  `crate::markdown::render`, loads the reused `"[CompletionDocs]"` scratch buffer,
  paints `DOC_MD_NS` extmarks, and **positions it beside the current completion menu**
  using `menu_geom` (core-owned) + `place_docs_beside`'s flip logic (ported into core).

## Positioning

`menu_geom` (`menu.rs:1708`) already computes the menu box in the focused window's
text-area cells (row/col/width/height, up/down flip). Port `place_docs_beside`
(`redraw.rs:1447`) into core to pick the beside-column (right of the box, flipping
left when the left side has more room), then open the float with
`FloatRelative::Win(focused)` (or `Editor` + absolute cells) at that position, inner
size clamped to `MAX_DOCS_W=60 × MAX_DOCS_H=12`. `place_float`
(`windows.rs:801`) clamps it on-screen. The existing
`docs_sidebar_butts_against_the_popup` test guards flush adjacency and will catch any
off-by-one against the (still server-projected) menu overlay.

Note: the completion **menu** itself stays a server-projected overlay — we are *not*
converting it to a window (out of scope). We get a real docs window beside an overlay
menu; they don't overlap, so z-order is fine.

**Wrap is a configurable option, default on.** Add a `docs_wrap: bool` field to the
completion config (`CompleteConfig`, `complete.rs:112`, default `true`), surfaced via
`nx.complete.setup { docs_wrap = true|false }`. When true the docs float sets its
window `wrap` option so a doc line wider than the float wraps within it; when false it
does not (long lines truncate at the edge). Default-on also makes horizontal scroll
unnecessary in the common case — reinforcing the `hscroll` deletion in Phase 2. Height
still clamps to `MAX_DOCS_H` and scrolls vertically via the wheel. The cmdline wildmenu
docs window always wraps (plain help text, no per-surface config).

## Persistence & teardown

- **Persist across keystrokes**: extend `close_transient_doc_floats` (`float.rs:407`)
  to keep a *set* of protected names — add `"[CompletionDocs]"` when
  `completion_active()` (`complete.rs:168`), exactly as it keeps `SIGNATURE_DOC_FLOAT`
  when `signature_session`. Re-populate each keystroke via `open_completion_docs_float`
  (which closes+replaces in place).
- **Explicit close at every completion-close site** (the float is not tied to the menu
  view, so it must be torn down explicitly, like `end_signature_session` closes
  `SIGNATURE_DOC_FLOAT`): `close_completion` (`menu.rs:1153`), `complete_take_accept`
  (`menu.rs:1146`), `complete_finish` (`complete.rs:294`), `complete_accept*`
  (`complete.rs:303/321`), `complete_trigger` re-open (`complete.rs:204/217`). A single
  `close_completion_docs_float()` helper called from each.

## Phases

**Phase 1 — the real window works (core + server); old projection disabled.**
- Core: `open_completion_docs_float(md)` + positioned place (port `place_docs_beside`)
  + `close_completion_docs_float()` + the persistence keep-set + close-site hooks.
- Server: on selection-settle / resolve-landed during input, source the markdown (the
  3 sources) and call `open_completion_docs_float`, or close when the row has none;
  close on menu close. Gate `project_complete_docs` to return `None` so only the new
  window shows.
- Test (new): a completion with LSP/inline docs opens a **float window** (in
  `windows[]`) beside the menu whose lines are stripped markdown and whose
  `highlights` carry `@markup.*` + fenced-code spans. Commit.

**Phase 2 — delete the bespoke server + core machinery.**
- Remove `project_complete_docs`, `CompleteDocsMeta`, `place_docs_beside` (server), the
  `menu.docs` emission for completion, `stash_complete_docs_hit` calls.
- Remove core scroll/hit state: `complete_docs_scroll`/`hscroll`/`hit`,
  `CompleteDocsHit`, `scroll_complete_docs`/`_h`, `stash_complete_docs_hit`,
  `complete_docs_hit_at`, the `mouse_complete_wheel` docs arms + the mouse event arms.
  (Wheel over the docs window now scrolls it natively via the normal window mouse path.)
- Drop the docs-scroll resets in `complete_select_navigate`/`_index`. Commit.

**Phase 3 — delete client rendering + view decode.**
- TUI `render_menu_docs` (`render.rs:2844`) + call site; GUI docs block
  (`render.rs:2638`); web docs block (`index.html:2341`) + `.pmenu-doc` CSS.
- `nxvim-view::MenuDocs` + `MenuData.docs` decode — **only if** cmdline wildmenu docs
  are also migrated (see Decision). Otherwise these stay for the cmdline path. Commit.

**Phase 4 — tests + web verify.**
- Rewrite the ~11 `complete.rs` docs tests (`poll_menu_docs`/`poll_docs_map`/
  `docs_of`/`poll_docs_box` → read the docs **window** from `windows[]`). The
  wheel/hscroll tests become native-window-scroll assertions (or fold into existing
  window-scroll coverage). Update `verify-basedpyright-completion.mjs` /
  `verify-cmdline-complete.mjs` to read the window. Commit.

## Decision — RESOLVED: migrate both

**Cmdline wildmenu docs** (`project_cmdline_docs`, `redraw.rs:2101`) are migrated too:
they become a real doc-float window (bottom-anchored beside the wildmenu box, plain
text — no markdown render needed there, so a plain `open_doc_float` with an empty
filetype). This fully deletes the `menu.docs` wire, all three client `MenuDocs`
renderers, and `nxvim-view::MenuDocs`. The completion docs use the markdown path;
the cmdline docs use the plain path — both on the shared doc-float window infra.

## Risks

- **Adjacency alignment** — a core-positioned window beside a server-projected menu
  overlay must be cell-perfect; both derive from `menu_geom`, and the flush-adjacency
  test guards it, but this is the #1 bug surface.
- **Window leaks** — `doc_float_wins` can go stale on `close_all_floats` /
  `restore_snapshot` (layout restore drops floats without touching the registry). Mitigated
  by opening-replaces-by-name + explicit close at every completion-close site; add a
  guard so a stranded `"[CompletionDocs]"` entry self-heals.
- **Insert-mode float** — proven safe by signature help (a doc-float window shown during
  insert-mode completion, non-focusable, non-grabbing). Must never route it through
  `focus_window`/`switch_buffer`.
