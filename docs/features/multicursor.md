# Multi-cursor mode

A Helix/kakoune/Sublime-style multi-cursor, built on top of bemtvi's vim grammar
rather than replacing it. You drop several cursors, then every motion, operator,
visual selection, and insert acts at all of them at once — `cw`, `x`, typing,
`o`, `p`, even per-cursor visual mode.

It is **single-buffer** multi-editing: cursors are per-buffer and shown only in
the focused window.

## The two phases

Multi-cursor is split into two phases with a mode boundary between them, so you
never have to think about "placement" and "editing" at the same time.

```
Normal ──<A-c>──▶ MultiCursor ──<Esc>──▶ Normal (cursors live) ──<Esc>──▶ Normal
                  │  motions move primary           │  motions/edits → all cursors
                  │  c / {n}c{motion} drop cursors  │
                  └─ /search keeps cursors          └─ /search or n clears them
```

1. **Placement** — enter with `<A-c>` (Alt+c). The status line reads
   `MULTICURSOR` and the active cursor is recolored **amber**. Motions move only
   that active cursor — you navigate around (including `/`-search) and drop
   *secondary* cursors at the spots you want.
2. **Editing** — press `<Esc>` to leave placement. The dropped cursors stay, and
   the status line returns to `NORMAL`. Now motions and operators apply at
   **every** cursor at once. A second `<Esc>` collapses back to a single cursor.

On leaving placement the primary cursor snaps onto the nearest placed cursor, so
the final set is exactly the cursors you dropped — never an extra one where you
happened to stop navigating.

## Placement grammar

While in `MULTICURSOR` mode:

| Keys | Effect |
| --- | --- |
| `<A-c>` / `<M-c>` | Enter placement and drop a cursor at the active position |
| `h` `j` `k` `l` `w` `b` `/` `n` … | Move **only** the active cursor (pure navigation) |
| `c` | **Toggle** a cursor at the active cell — drop if empty, clear if already there |
| `c{motion}` | Move by `{motion}` and drop a cursor there (`cj` = one line down) |
| `{count}c{motion}` | Drop cursors along the motion's span — `3cj` drops on relative lines 0–3 (4 cursors) |
| `cc` / `{count}cc` | Drop one cursor per line over `count` lines |
| `<Esc>` | Finish placement → Normal, cursors persist (first cancels a half-typed `{n}c…`) |

The spawn key is **Alt+c**, not Helix's bare `C` — that's already vim's
change-to-end-of-line. On macOS the terminal must send Option as Meta for
`<A-c>` to arrive: in Terminal.app enable *Use Option as Meta key*; in iTerm2 set
*Left Option = Esc+*.

## Editing with the cursor set

Back in Normal mode with cursors live, the whole normal grammar replays at every
cursor. A motion is re-resolved per cursor, so `w` lands at the right word for
each one independently. What's wired:

- **Motions** and the operators over them — `dw` `yw` `cw` `=w`, `dd` `yy` `cc`,
  text objects like `diw` / `ci"`.
- The standalone edits — `x` `X` `D` `C` `s` `J` `~` `r`.
- **Insert** — typing, `Enter`, and `Backspace` apply at every cursor; so do the
  insert-entry keys `a` `A` `i` `I` (each cursor moves to its own target column —
  line-end for `A`, first-non-blank for `I`) and `o` / `O`.
- **Paste** — `p` / `P` with **per-cursor registers**: a multi-cursor yank
  (`yy`, `yiw`, …) captures each cursor's own text, and a later paste gives each
  cursor back its own slice. When the captured count doesn't match the live
  cursor count (a single-source yank, or the set changed), every cursor pastes
  the active register instead — plain vim `p`, broadcast.

Cursors that converge onto the same cell after a motion (e.g. everyone hits `0`
or `gg`) automatically merge, so you never get a silent pile of cursors editing
one spot.

### Per-cursor visual mode

Each cursor carries its **own** selection. Press `v` or `V` over a placed set and
every cursor gets a one-wide selection; visual motions extend each independently.
Operators (`d`, `y`, `c`, …) bracket each cursor's own selection. Visual `o`
swaps to the other end of the selection at every cursor (there's no visual-block
mode yet, so `O` aliases `o`). `<Esc>` collapses the selections but keeps the
cursor heads; a second `<Esc>` collapses those too.

## Search clears the set (in Normal)

A committed search — `/` `?` `n` `N` `*` `#` — in **Normal** mode is treated as
navigating away: it abandons the multi-cursor session and collapses to a single
cursor. In **placement** mode the same search instead jumps to a match so you can
drop a cursor there, keeping the set. (Incsearch preview never clears anything —
only the committed search does.)

## Undo

Undo restores cursors to **where they were before** the undone edit, not where
the edit shifted them. Each multi-cursor edit undoes as a single step.

## Custom keymaps while placing

Placement mode has its own keymap bucket — mode code `'m'` — so a binding can
fire *only while placing*:

```lua
vim.keymap.set('m', '<Tab>', 'wc')  -- in MULTICURSOR: jump a word, drop a cursor
```

This is isolated the way vim isolates modes: a plain `'n'` (normal) map does
**not** fire while placing, and an `'m'` map does **not** fire in normal mode.
The all-mode `''` map (`:map`) still covers placement. Any key the `'m'` trie
doesn't bind passes straight through to the built-in placement grammar, so
`h`/`j`/`c`/`{count}c{motion}`/`<Esc>` keep working.

## How it works (in brief)

The load-bearing trick is that secondary cursors are stored as **extmarks** in a
reserved namespace, so the buffer's single edit choke point shifts them all for
free — an edit at one cursor keeps every other cursor's position correct with no
bespoke fix-up, and cursors ride the undo snapshot. The editing phase then just
*replays* an ordinary single-cursor command at each cursor in turn. The primary
cursor stays the existing `self.cursor`; secondary cursors are a purely additive
layer.

For the full design — the extmark model, the replay primitives
(`for_each_cursor` / `edit_each_cursor`), per-cursor visual anchors, undo cursor
baking, and rendering — see the
[multi-cursor design spec](../specs/2026-06-08-multicursor-design.md).
