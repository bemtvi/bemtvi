# Screen-reader access to the GUI editor

**Status:** parked (2026-10-27). Nothing is scheduled; this records the survey and the
shape of the work so it can be picked up later.

**Goal.** A screen-reader user can edit in `bemtvi-gui`, and in an app that embeds
`bemtvi-mitsuami`: read the text around the caret, hear the mode, the messages and the
diagnostics, and move through popups. The TUI is out of scope: anyone who needs this has
the GUI.

## What a code editor is expected to do

VS Code is the reference among mainstream editors:

- **The text as a native text control.** Monaco keeps a hidden textarea in step with the
  lines around the caret (`editor.accessibilityPageSize`), so a screen reader moves by
  character, word and line, and reads selections, natively.
- **Accessible View:** any transient popup (hover, completion, a notification) opened
  as plain text to read at leisure. **Accessibility Help** for the focused part.
- **Live announcements** of what happens out of focus: search results, errors, a task
  ending.
- **Accessibility signals:** a sound or a spoken cue when the caret's line has an
  error, a warning, a breakpoint or a fold, or a diff line is reached.
- **Keyboard reach** to everything, and jumps to the next problem or change with a
  spoken summary.
- Visual: high contrast, zoom, no blinking or smooth scrolling.

For a modal editor, add the **mode** (Insert, Visual, an operator pending) and the
**command line** as things to announce.

## What mitsuami gives today

Surveyed at mitsuami `4dbacb1`:

- **The core computes a semantic tree** (`mitsuami-core/src/a11y.rs`,
  `ui/accessibility.rs`). It has roles, including `TextArea`, `List`, `ListItem` and
  `StaticText`, and `A11yProps` (role, label, description, value, hidden).
  `Ui::a11y_tree(window)` returns it, and tests find controls through it.
- **The platform's accessibility comes from the native widgets themselves.**
  `Command::SetA11y` pushes only the label, the description and `hidden` onto the
  native element (each backend's `apply.rs`). Role and value are what the native
  control is.
- **A `GpuSurface` is an image:** `NSAccessibilityImageRole` on AppKit
  (`mitsuami-appkit/src/surface.rs`), `AccessibleRole::Img` on GTK
  (`mitsuami-gtk/src/surface.rs`), and `Role::Image` in the core tree. So the editor
  reads as a labelled picture: no text, no caret, no children. Overriding the role in
  `A11yProps` changes the core tree (tests) only, not the platform.
- **No live region or announcement** in the model.
- `hidden` takes a node off the screen *and* out of the accessibility tree; there is no
  "visually hidden, still accessible".

## Two ways to get there (mitsuami first)

1. **Surface semantics (recommended).** A `GpuSurface` carries a semantic subtree the
   app supplies. The core includes it in `a11y_tree`, so tests and an inspector see it
   for free. Each backend exposes it natively:
   - AppKit: `NSAccessibilityElement` children.
   - GTK: a `GtkAccessible` implementation, with `GtkAccessibleText` (GTK 4.14+) for
     the caret and the lines.
   - WinUI: a UIA automation peer with `TextPattern`.
   - Qt: a `QAccessibleInterface` with its text interface.

   The text area is the costly part, because screen readers query it by character,
   line and selection range. An **announce** primitive belongs with it (each platform
   has one; confirm the minimum versions).
2. **A native text widget, visually hidden.** The Monaco approach on native controls: a
   real `TextArea` node mirrors the viewport's lines and the caret, present to
   accessibility but not drawn. That gives the full text APIs on every platform without
   writing them, at the price of keeping caret and selection in step with a widget we
   don't own. It needs a "visually hidden" state in mitsuami.

Either way, a window that draws the semantic tree (`a11y_tree`) is worth building
alongside it. An invisible tree rots unless someone can look at it; Flutter's semantics
debugger and the browsers' accessibility inspectors are the precedent. mitsuami's
architecture already lists a devtools inspector as "later".

## bemtvi's side

The same whichever way mitsuami goes. `bemtvi_gpu::Client` holds the `View`, so it
builds the subtree, and the mitsuami adapter maps it onto mitsuami's nodes:

- **The focused window** as a multi-line text area, its value a window of lines around
  the viewport (never the whole buffer per frame: the never-freeze rule), the caret
  and selection as offsets into it. Other windows as their own nodes, with focus
  following the editor's.
- **The command line** as a text field while `:` or `/` is open.
- **The message line** as a live region: errors, `:echo`, `E37`.
- **The mode** as state on the editor node, announced when it changes.
- **Popups:** completion, picker and `btv.ui.select` as lists with a selected row;
  hover and doc floats as read-only text.
- **Diagnostics on the caret's line** as the text area's description, or a signal.

What the redraw doesn't carry yet (the text of lines outside the painted rows, the mode
as a name, per-line diagnostics) would ride the `View` from the server. The server is
the one that knows them, and every client could then use the same fields.

Tests: `Client::attach_headless` already gets real frames, so the subtree is built and
asserted headless in `crates/bemtvi-gpu/tests/client.rs` ("the caret is at offset 42",
"the live region says E37"). That's a better test target than pixels.

## Phases (when unparked)

1. **mitsuami:** surface semantics in the core tree, an announce primitive, and the
   inspector window. One backend native (AppKit or GTK).
2. **bemtvi:** the `Client` builds the subtree from the `View`; headless tests.
3. **mitsuami:** the other backends' native exposure.
4. **bemtvi:** what the `View` lacks (mode name, the lines around the viewport,
   diagnostics per line), and the signal-style cues.

## Open questions

- Surface semantics or the visually-hidden native widget: whether mitsuami wants to own
  text-range accessibility on four toolkits.
- How large the text area's window of lines should be, and how it updates
  incrementally.
- Whether an "Accessible View" (a popup's text in a read-only buffer) is better done in
  the editor itself, as a `btv.view`, which every client would get.
