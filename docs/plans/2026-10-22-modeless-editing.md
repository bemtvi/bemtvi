# Modeless editing: a GUI and web editing model without Normal mode

*2026-10-22*

Status: **done**. All seven phases landed; see [Outcome](#outcome) for where the
build departed from this sketch. Second of two plans. The first,
[standard undo grouping](2026-10-21-standard-undo-grouping.md), is done: typing
undoes about a word at a time, which this mode depends on.

## What this is

A session setting that makes bemtvi edit the way VS Code, Sublime or a browser
textarea do. You type and the text goes in. Shift with a motion selects. Typing
over a selection replaces it. Ctrl-C/X/V copy, cut and paste through the system
clipboard, and Ctrl-Z undoes. There is no Normal mode to fall into by accident
and no `<Esc>` to remember.

It is for the GUI and the web client only. The terminal client never gets it.

## Decisions (settled with the user)

- **A session setting.** A global boolean option, `'modeless'`, off by default,
  set from a config (`btv.o.modeless = true`) or `:set modeless`. It is not
  per-buffer or per-window, and it isn't persisted.
- **Not in the TUI.** A terminal can't report most of the chords this layout
  needs (Shift with arrows in every terminal, Ctrl-Shift-letter without the
  kitty protocol), and a TUI user is a vim user. `:set modeless` fails loud when
  a TUI is attached, and a TUI attaching to a session that has it on turns it off
  with a message. Nothing degrades silently.
- **Undo is already standard.** Word-sized steps shipped with the first plan, for
  every mode. Ctrl-Z here is the same `u`.

## Design: Insert is the resting mode

The core already has every piece of this mode except the place it rests. Insert
mode types. Select mode (`Mode::Select`, from the snippet work) holds a
charwise range that a printable, `<CR>` or `<BS>` replaces. So modeless adds no
new `Mode` variant. It changes where the editor *returns to*.

`Editor::base_normal_mode()` (`editor/helix.rs`) is the existing seam: Helix uses
it to rest in `HelixNormal` instead of `Normal`. It becomes
`resting_mode()`, and with `'modeless'` on it answers per window:

| Window shows | Rests in | Why |
| --- | --- | --- |
| a modifiable text buffer | `Insert` | typing edits |
| a `btv.view`, tree, quickfix, help or other `nomodifiable` buffer | `Normal` | their keys are buffer-local Normal maps (`j`, `<CR>`, `q`) |
| a terminal buffer | `Terminal` | unchanged |

Today 32 sites set `self.mode = Mode::Normal` directly instead of asking the
seam: leaving the cmdline, closing a picker, finishing an ex command, a window
switch, a buffer load and so on. Phase 1 routes each one through
`resting_mode()`. That audit is most of Phase 1's work, and a site it misses
strands a GUI user in Normal mode with no way out they know of. So Phase 1 also
adds a guard: in modeless editing, entering `Normal` over a modifiable buffer
outside an explicit command is a bug the tests look for.

`<Esc>` in modeless Insert never leaves Insert. It closes the completion popup,
a float or the signature help, ends a snippet session, and collapses a selection,
in that order, and otherwise does nothing.

Helix and modeless can't both be on. Turning one on while the other is on fails
loud.

## Selection

Select mode becomes the selection of this model. Its handler (`editor/select.rs`)
already replaces on printables, `<CR>`, `<BS>` and `<Del>`. What changes:

- **Shift with a motion selects.** In Insert, `<S-Left>`/`<S-Right>`/`<S-Up>`/
  `<S-Down>`, `<S-Home>`/`<S-End>`, `<C-S-Left>`/`<C-S-Right>` (word),
  `<S-PageUp>`/`<S-PageDown>` and `<C-S-Home>`/`<C-S-End>` enter Select with the
  anchor at the cursor. In Select they move the head and keep the anchor.
- **A plain motion collapses.** An arrow without Shift leaves Select for Insert,
  at the selection's start or end the way a GUI editor does (`<Left>` lands on the
  start, `<Right>` on the end).
- **Select's fallback arm changes.** Today any key Select doesn't handle "ends
  Select and is handled as an ordinary Normal-mode key". In modeless that would
  drop into Normal, so the fallback becomes Insert there. The snippet path keeps
  its current behavior; this is the one branch on the setting inside the handler.
- **The mouse selects into Select, not Visual.** `editor/mouse.rs` enters Visual
  on a drag, a double-click (word) and a triple-click (line). In modeless these
  enter Select instead (charwise for a word, linewise for a line), and a plain
  click places the cursor in Insert.
- **Select all** is a named action over the whole buffer.

## Commands

The core gains named verbs, following Helix's named-action seam
(`btv._helix_action` -> `Editor::apply_helix_action`) so the key layout stays a
plugin and every key is rebindable by name:

| Action | Does |
| --- | --- |
| `copy` | selection to `"+`; no selection copies the line (VS Code) |
| `cut` | same, then deletes it as one undo step |
| `paste` | `"+` over the selection, or at the cursor; one undo step |
| `undo` / `redo` | `u` / `<C-r>` without leaving Insert; closes the pending typing step first |
| `select_all` | the whole buffer in Select |
| `delete_word_back` / `delete_word_forward` | `<C-BS>` / `<C-Del>` |
| `indent` / `outdent` | `<Tab>` / `<S-Tab>` over a multi-line selection |
| `duplicate_line`, `move_line_up` / `move_line_down` | the usual line verbs |

The copy, cut and paste verbs use the clipboard providers the GUI and web
already have (`server/clipboard.rs`, `verify-clipboard.mjs`); no new clipboard
path.

Ex commands stay reachable two ways. A **command palette** is a new
`btv.picker` source, `commands`, listing ex commands, user commands (with
their `desc`) and every described keymap. And a key opens the `:` line directly
for people who know the command. Both return to the resting mode when they
close.

## The key layout ships as a plugin

As with Helix, the core gives the semantics and a bundled prelude plugin
(`prelude/modeless.lua`) gives the keys. It binds them in a new keymap bucket
that is consulted only while modeless is on, before the Insert maps, so it never
touches a vim user's Insert mappings. Keys it doesn't bind fall through to the
Insert maps and then the native handler, so completion, snippet and auto-pair
maps keep working.

Default layout (Ctrl on every platform for now; see Phase 6):

| Key | Action | Key | Action |
| --- | --- | --- | --- |
| `<C-z>` | undo | `<C-S-z>`, `<C-y>` | redo |
| `<C-c>` | copy | `<C-x>` | cut |
| `<C-v>` | paste | `<C-a>` | select all |
| `<C-s>` | `:w` | `<C-f>` | search (`/` line) |
| `<C-g>` / `<C-S-g>` | next / previous match | `<C-h>` | replace (`:s` with preview) |
| `<C-p>` | files picker | `<C-S-p>` | command palette |
| `<C-;>` | the `:` line | `<C-w>` | close the buffer |
| `<C-Tab>` / `<C-S-Tab>` | next / previous buffer | `<C-BS>` / `<C-Del>` | delete word |

These shadow Insert built-ins on purpose: `i_CTRL-V` (literal), `i_CTRL-A`,
`i_CTRL-X` (completion submode), `i_CTRL-R` (register), `i_CTRL-W` and
`i_CTRL-O`. A modeless user reaches those through the palette.

Two conflicts to live with:

- **Browser-reserved chords.** Chrome on Windows and Linux takes `<C-w>`,
  `<C-t>`, `<C-n>`, `<C-Tab>` and `<C-1>`..`<C-9>` before the page sees them. The
  web client already accepts Alt in their place on those platforms
  (`verify-alt-ctrl.mjs`), so the layout keeps the Ctrl names and a web user
  presses Alt-W. The docs say so.
- **`<F2>`/`<F3>` are macros.** GUI editors use F3 for "find next", so the layout
  uses `<C-g>` (the Sublime and macOS convention) and leaves the macro keys alone.

## Events and plugins

Never leaving Insert changes which events fire. Phase 5 checks each:

- **`TextChanged` never fires**, only `TextChangedI`; and **`InsertLeave` never
  fires** after the first `InsertEnter`. Three bemtvi-line segments refresh only
  on `TextChanged` / `InsertLeave` (`components.lua:176`, `:526`, `:551`), so they
  would go stale. The fix goes in the plugins (listen to `TextChangedI` as well),
  not in a core rule that fakes a Normal-mode event.
- **Diagnostics** already apply while typing after a debounce
  (`update_in_insert`'s number form). `update_in_insert = false` means "hold
  until `InsertLeave`", which here means forever, so modeless treats `false` as
  the default debounce.
- **The pending undo step.** The last word typed stays an open group until
  something breaks it. Anything that reads history (`:w` with undo persistence,
  `btv.undotree`, `:undolist`) must see it committed. Phase 5 checks each reader
  and commits where it doesn't already.
- **`ModeChanged`** fires on Insert/Select flips, which is correct, and a
  statusline shows them as `INSERT`/`SELECT`. bemtvi-line may want a quieter
  label in this mode; that's a plugin choice.

## The client kind

The attach handshake carries capabilities but not *which* client attached. The
gate needs that, so `btv_ui_attach`'s capability map gains `client` (`"tui"`,
`"gui"`; the web edit host sets `"web"` where it sets its own caps). It is also
mirrored into `btv.ui.caps().client` for plugins. A client that sends no kind
(the test harness, `--test-plugin`, a third-party RPC client) is allowed; only
`"tui"` is refused.

Everything above lives in the core, so it works the same over a daemon session
and in the browser; the web build is verified with the `--no-default-features`
build and a `verify-modeless.mjs` Playwright check.

## Phases

Each lands as its own commit and pauses for review.

### Phase 1: the setting, the gate and the resting mode

`'modeless'` (in `options.rs::canonical()`, `apply_set_*`, and the
`every_known_option_is_wired_not_silent` list), the `client` capability in all
three clients, the refusal on a TUI, `resting_mode()`, the audit of the 32
`Mode::Normal` sites, `<Esc>` that stays in Insert, focus moves that pick the
mode per window, and the Helix exclusion. Tests feed the existing harness with a
`client = "gui"` attach and assert that no ex command, picker, window switch or
buffer load leaves a modifiable buffer in Normal.

### Phase 2: selection

Shift-motions into and inside Select, plain motions collapsing to Insert, the
fallback arm, mouse drag / double / triple click into Select, and select-all.

### Phase 3: the verbs

The named-action seam and the verbs in the table above, including copy/cut of
the line with no selection and undo/redo from Insert.

### Phase 4: the layout plugin and the palette

`prelude/modeless.lua` with its keymap bucket, the `commands` picker source, and
the search/replace keys. `btv.test` specs for the plugin.

### Phase 5: events and plugins

The event audit above: `update_in_insert = false`, the pending undo step's
readers, and patches to bemtvi-line (in `~/work/bemtvi-plugins`, its own repo
and commit).

### Phase 6: macOS Cmd (decision pending)

The core `Key` has no Cmd modifier: the web client's `<D-…>` folds into Alt
(`input.rs`), and the web client passes Cmd chords to the browser. A Mac layout
on Cmd needs a real modifier through the key parser, the keymap tries and both
clients. Until then a Mac user gets the Ctrl layout. Whether to do this, and
whether Cmd should then *replace* Ctrl on a Mac, is the user's call.

### Phase 7: docs and example

`docs/features/modeless.md`, the index row and book entry, an architecture
paragraph, and `examples/modeless/` with a spec that sets the option and drives
the layout's keys.

## Testing

Black-box through the harness, as usual, with every rule's test failing before
its phase and passing after it, mutation-checked. The web leg gets
`verify-modeless.mjs`, driving real keydown events (the Alt stand-ins included)
rather than `__bemtvi.feed`. Each phase also runs the example specs and the
plugin specs, since a resting-mode change touches every plugin that assumes
Normal after a command.

## Non-goals

- The TUI, in any form.
- Multi-cursor gestures (Ctrl-D add next match, Alt-click). The engine exists;
  binding it is a follow-up once the base mode settles.
- Restoring the selection on undo, which VS Code does. Useful here and noted in
  the undo plan; a separate change. (Done after the phases: each undo state
  records the modeless selection it was left with, as byte offsets, and a
  modeless undo or redo selects it again.)
- Making modeless the default anywhere, or a per-buffer setting.

## Outcome

- **One settle point, not 32 edited sites.** `settle_modeless` moves a clean Normal
  over a modifiable buffer into Insert (and an Insert the buffer can't take back to
  Normal). The server calls it after every key and command, the core when a mapping
  or fed span closes. Nothing that drops into Normal had to learn about the option,
  including paths added later.
- **`:normal` from Insert runs from Normal**, then resumes Insert, as vim does. It
  used to type its keys. Plugins call `btv.cmd("normal! …")` all the time, and a
  session resting in Insert would have broken every one.
- **The selection keeps its own caret anchor and head** (`CaretSel`). Select's
  inclusive representation can't express a caret moving left or a selection ending
  on a line break; it is used only to draw. Type-over deletes the exact span and
  writes no register.
- **The key layout's tries are overlays**: the Insert (or Select) maps with the
  `modeless` maps laid on top, so a layout key wins and every other key still finds
  its Insert map. `<C-h>` goes in only with the keyboard protocol, and outdent has no
  Control chord (`<C-[>` is `<Esc>`).
- **General pieces came out of it**: `btv.cmdline.open`,
  `btv.cmdline_complete.catalog()` and the `commands` picker, the `BufModifiedSet`
  event (rather than having plugins watch `TextChangedI`), and `btv.ui.caps()` gained
  `client` and `platform`.
- **Command is a real modifier** (`Key::cmd`, `<D-…>`) through the core, the shared
  encoder, the GUI and the web page; the layout installs from `UIEnter`, once per
  platform.
- **The plugin specs of Phase 4** became the example's spec in Phase 7, which drives
  every note's keys under `--test-plugin`; the Rust suite covers the rest.
- **Bugs fixed along the way**, each with a test that failed first:
  - `'mouse'` gated Select on an `s` flag vim doesn't have, so a click in Select was
    ignored; vim's `v` covers Select.
  - Any dirty undo group became a node on commit, even one that changed nothing
    (`i<Esc>` left an empty state). The tree now remembers the `changedtick` a group
    began at.
  - `snapshot_taken` is editor-wide, so after a save, a reload or a buffer switch an
    insert session could type into a buffer whose tree had no open group: those edits
    belonged to no step, and an undo could lose them. `push_undo` now opens the
    buffer's group when the flag is stale, and typed edits always go through it.
  - The web page sent Ctrl/Alt+Shift+letter as an uppercase letter (`<C-P>`), which
    the editor reads as unshifted. It now sends `<C-S-p>`, as the GUI does.
- **Found and left alone** (they predate this work and sit outside it): an unhandled
  Ctrl chord in vim-mode Insert types its letter (`<C-z>` types `z`); `build.sh` stops
  on its last step when `web/vendor/msgpack` can't be copied over, after the wasm is
  already linked.
- **Fixed afterwards:** `<C-o>:{cmd}<CR>` never resumed Insert. The command line's
  `<CR>` and `<Esc>` are keymap actions, not keys through `Editor::input`, so the
  one-shot stayed armed: `<C-o>:w` left you in Normal, and the next key ran as a
  Normal command (a space moved the cursor instead of typing). Submitting or
  cancelling the line now resumes Insert, and a `:normal` run from it keeps the
  one-shot disarmed until its last key.
- **Not done**, as planned: multi-cursor gestures. Restoring the selection on undo
  landed after all (`5d727100`).
