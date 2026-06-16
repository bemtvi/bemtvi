# The unified float-list widget — one surface under completion, picker, and select

**Status:** **proposed (2026-06-14).** Defines the single server-side UI
component that completion, the fuzzy picker, and `nx.ui.select` all render
through: a floating, match-highlighted, selectable **list**, with an optional
**preview** pane and an optional **prompt** field, placed either under the
cursor or over the editor. The [native plugin API](2026-06-11-native-plugin-api.md)
names the *engines* (`nx.complete`, `nx.picker`, `nx.ui.select`); this document
designs the *widget* they share, so the rendering, navigation, matching, and
float placement are built once. It is a dependency of the completion and picker
work (build-order steps 2–3) and of [`nx.lsp`](2026-06-14-nx-lsp-design.md)
Phase C (LSP locations → picker, hover/docs floats).

## Why a shared widget

Completion menus, fuzzy finders, and choice menus look like three features but
are one widget under three orchestrations. Each is *a float containing a
selectable list with matched-character highlighting*, differing only in whether
it carries a side **preview** and a **prompt**, and in where it floats. Building
three of them — three renderers, three navigation handlers, three match
highlighters — would triplicate the hot path and the float bookkeeping, and the
PUC 5.1 / no-frame-time-Lua constraints (ADR 0002) mean none of it can live in
Lua anyway. So the widget is **one Rust component**; the engines on top are thin.

The split this document insists on (and the reason it is not just "merge
completion and picker"):

- **The widget** — rendering, selection, navigation keys, the matcher, float
  placement, the preview, the prompt. Unified here.
- **The engines** — `nx.complete` / `nx.picker` / `nx.ui.select` — stay separate
  (plugin-API spec), because their *orchestration* genuinely differs: trigger
  detection + debounce + buffer-as-query (completion) vs. explicit open + prompt
  + arbitrary confirm action (picker) vs. a one-shot choice (select). They are
  thin drivers that configure the widget and handle confirm; they do not
  re-implement it.

## What already exists (and the one new primitive)

The widget is mostly **consolidation**, not new ground:

| Piece | Status | Backed by |
| --- | --- | --- |
| Float placement (both modes) | ✅ exists | `FloatConfig` / `FloatRelative::{Cursor, Editor}` / `FloatAnchor` (room-flipping NW↔SW), borders — the [floating-windows plan](../plans/2026-06-06-floating-windows.md), all phases done |
| List rendering under the cursor | ✅ exists | the `pmenu` projection (match-highlighted, selectable) |
| Selectable list + on-select action | ✅ exists | the bottom `panel` (loclist: `set_panel_targets` / `set_panel_on_select` / `set_panel_cursor`) |
| **Prompt input-grab** | ⬜ **new** | the one net-new core primitive (below) |
| Preview pane | 🚧 partial | floats compose; a float beside / a pane within — rendering by kind is new |
| Fuzzy matcher | ⬜ new | Rust, nucleo-class, shared by all static-source consumers |

The **two placement modes map onto existing `FloatRelative` variants** with no
new positioning code: "under the cursor" is `FloatRelative::Cursor` with the
anchor flipping NW↔SW for room (the pmenu already does this); "over the editor"
is `FloatRelative::Editor`, centered. `FloatRelative::Win` exists too but is not
used by this widget.

## The widget configuration

Two **orthogonal** capabilities, not three named variants — orthogonal flags
cover the combination an enum would miss (list + prompt, no preview = a command
palette) and keep the widget one shape:

```
widget {
  list      = <required>     -- selectable, match-highlighted
  preview   = <optional>     -- side pane; rendered by kind (below)
  prompt    = <optional>     -- input-grab field; presence flips the query source (below)
  placement = cursor | editor
  multi     = false          -- optional toggle-marks (pickers); off for completion
}
```

The four configurations the engines use:

| Configuration | preview | prompt | placement | Engine |
| --- | --- | --- | --- | --- |
| **Completion** | docs sidebar | — | cursor | `nx.complete` |
| **Choice / code-action** | — | — | cursor | `nx.ui.select` |
| **Command palette** | — | ✅ | editor | picker (no preview) |
| **Fuzzy finder** | ✅ | ✅ | editor | `nx.picker` |

## The query-source axis — what "prompt present?" actually decides

The presence of the prompt is not cosmetic; it decides **where keystrokes are
routed**, which is the deepest difference between the engines:

- **No prompt → the query is the buffer.** Keystrokes go to the document. The
  completion engine watches the input path (trigger chars, debounce) and
  re-queries against the buffer prefix. The editor *is* the input field — which
  is exactly why completion has no separate prompt.
- **Prompt present → the prompt grabs input.** Keystrokes are captured into the
  widget's query buffer and never reach the document; the widget matches (static
  sources) or forwards "query changed" (dynamic sources). This is the picker.

Modeling the prompt as an optional capability is therefore the same decision as
"query from buffer vs. query from a grabbed field" — one flag, both behaviors.

## The new primitive: the input-grab prompt

The only piece with no existing analogue. A native prompt that:

1. **Grabs all input** while open — keystrokes edit the query buffer, navigation
   keys (`<C-n>`/`<C-p>`/`<Down>`/`<Up>`, `<CR>`, `<Esc>`, the configured keys)
   drive the list, and **nothing reaches the document**. This generalizes the
   bottom panel's selection-key handling to a full input line. When a preview pane
   is shown, `<C-d>`/`<C-u>` (half page) and `<C-f>`/`<C-b>` (full page) scroll the
   *preview*; core only names the gesture (it can't see the pane height or file
   length), and the server folds it into a per-selection scroll offset.
2. **Emits "query changed" natively** — the matcher re-ranks (static sources) or
   the engine re-runs the source (dynamic sources) off the input path, debounced.
   **No Lua runs per keystroke** (rule 4); a static-source picker never re-enters
   Lua while you type.
3. **Carries a generation token** on every query edit, so a source response for a
   query the user has already typed past is dropped (rule 5 / engine
   Decision 3) — superseded `nx.spawn` jobs (live grep) are cancelled via
   `ctx.on_cancel`.

It is modeled like the existing input modes (cmdline / the multi-cursor
placement mode are the in-repo precedents for "a bounded mode that owns input"),
not as a Lua loop.

## Matching

One fuzzy matcher in Rust (nucleo-class), shared by every **static-source**
consumer — completion, `select`, and static pickers (`files`). The widget filters
and ranks locally as the query changes and highlights matched characters in the
list. A **dynamic** source (`dynamic = true`: live grep) **bypasses the matcher**
entirely — the widget forwards each query change to the source and renders what
streams back, in order. So the widget needs a per-source `dynamic` flag deciding
*match-locally* vs. *forward-query*.

## Preview — server-rendered, by kind

The preview is **declarative and rendered natively** — no Lua at frame time
(rule 4). The engine/source declares a kind; the server renders it:

| `preview` kind | Renders | For |
| --- | --- | --- |
| `"markdown"` | the resolved markdown (LSP `completionItem/resolve`, hover) | completion docs |
| `"file"` | the file at `item.path` (rope + native treesitter) | file picker |
| `"location"` | the file at `item.path`, scrolled to `item.row`/`col`, range-highlighted | references / grep |
| none | — | choice menus |

Same logical slot, two layouts the **widget** owns so plugins never compute
geometry: in **cursor** placement the preview is a *float beside* the list (docs
to the right, flipping left for room — the pmenu's existing doc-float behavior);
in **editor** placement it is a *pane within* the centered layout.

## What stays out of this widget

**Hover, signature help, and diagnostic floats are not this widget.** They have
no list and no selection — they are *content floats*. Folding them in only to be
"unified" would add a list-less degenerate mode that muddies the widget. Keep
them as `nx.ui.float` (rendered content), a **sibling** of the list-widget on the
shared `FloatConfig` placement layer. The placement layer is the real shared
foundation; the list-widget and the content-float are two consumers of it.

`nx.tree` docks are also separate (a persistent edge-anchored window, not a
float).

## How the engines drive it

Thin drivers — the widget does the work:

- **`nx.complete`** opens the widget `{ list + preview="markdown", no prompt,
  cursor }` on trigger; the buffer is the query; accept inserts text / expands a
  snippet. The `"lsp"` source is the native client ([`nx.lsp`](2026-06-14-nx-lsp-design.md) Phase C).
- **`nx.picker`** opens `{ list + prompt + preview, editor }`; static sources
  match locally, dynamic sources forward the query; confirm runs the source's
  `confirm(item)`; multi-select sends marks to a confirm-all. LSP locations
  (definition/references/symbols) route here in `nx.lsp` Phase C.
- **`nx.ui.select`** opens `{ list, no preview, no prompt (optional filter),
  cursor }`; confirm resolves the returned promise to the chosen item (it is
  promise-only; the `vim.ui.select` compat alias keeps neovim's `(item, index)`
  callback). Code actions and any `nx.ui.select` caller use it.

The public surface stays exactly the three task-shaped engines the plugin-API
spec names. Optionally, a low-level `nx.ui.menu` exposes the widget directly for
plugin authors who want a raw list-float without completion/picker semantics —
deferred until a consumer needs it.

## Phasing

1. **The list-float widget core** — list rendering in a float (generalize the
   pmenu projection), native navigation, the Rust matcher, match highlighting,
   the `cursor`/`editor` placement over `FloatConfig`. No prompt, no preview yet.
   Immediately backs **`nx.ui.select`** (code actions) — the simplest consumer.
2. **The prompt input-grab** + dynamic-source forwarding + generation tokens →
   unlocks **`nx.picker`** (static + dynamic sources, build-order step 2).
3. **The preview pane** (by kind, both layouts) → completes the picker and backs
   the completion docs sidebar.
4. **`nx.complete`** drives the widget `{ cursor + preview }` from the input path
   (build-order step 3); the `"lsp"`/`"buffer"`/`"snippets"` sources land here.

Each phase is independently shippable: 1 gives working choice menus, 2 a working
picker, 3 previews, 4 completion.

## Testing (black-box, per the no-unit-test rule)

Drive keys against the running server; assert on the `redraw` view (the widget's
projection: list rows, selected index, matched-char highlight spans, preview
content, prompt text) and on the resulting buffer/cursor after confirm.

- **Widget:** open a `select`, navigate, confirm fires the right index; match
  highlighting tracks the query; `<Esc>` closes without action.
- **Prompt:** typing edits the query and re-ranks **without touching the
  document buffer**; a stale source response (old generation) is dropped; a
  superseded dynamic query cancels its job.
- **Placement:** `cursor` mode anchors at the cursor and flips above when there's
  no room below; `editor` mode centers.
- **Preview:** `"file"`/`"location"` render the target with treesitter; the docs
  sidebar shows resolved markdown; geometry differs correctly between cursor and
  editor placement.

## Open questions

- **Prompt position in editor mode** — top (telescope) vs. bottom (fzf)?
  Suggest a config default, top.
- **Filter-in-place for `select`** — should a long choice list grow a prompt
  automatically past N items, or stay promptless? (Lean: promptless; a caller
  that wants filtering uses a picker.)
- **Reusing the bottom `panel`** — is the loclist panel folded into this widget
  (editor-placement, no float) or kept as the separate dock it is today?
  (Lean: keep separate for now; revisit when `nx.tree` generalizes the dock.)
