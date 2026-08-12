# Helix mode — selection-first editing

An opt-in **selection-first** editing model, alongside — not replacing — bemtvi's
vim grammar. Where vim is *verb→noun* on a point cursor (`d` waits for a motion),
Helix is *noun→verb* on a persistent `anchor..head` **range**: a motion re-selects
on every keystroke, and a verb acts on the current selection immediately, with no
operator-pending wait. Every cursor **is** a selection; a bare cursor is a
width-1 selection, and multi-selection is the always-on default.

This is the [Helix](https://helix-editor.com/) *editing model* ported natively —
not its keybindings bolted on. The hard part isn't rebinding keys; it's that a
motion *means something different*: `w` moves **and re-selects** the next word on
every selection, so `wd` deletes a word with no motion-wait in between. That
semantic change is why the model is native Rust (a genuine core-grammar change),
while the **key layout** ships as the bundled `btv.helix` plugin.

Vim behavior is untouched. Helix is per-session and off by default.

## Turning it on

```lua
-- ~/.config/bemtvi/init.lua
btv.helix.enable()          -- enter Helix normal mode at startup
```

Or interactively: `:helix` toggles it (and `btv.helix.disable()` turns it off).
Both are idempotent. The status line reads `HELIX` (normal) or `HELIX-SEL`
(select); the mode codes are `hn` / `hs`, distinct from vim's `n`/`v` so plugins
and `ModeChanged` can tell them apart.

Search defaults to **smart-case** in Helix without touching the global
`ignorecase`/`smartcase`; opt out with `btv.helix.enable{ smart_case = false }`.

## The two modes

| Mode | Status | What motions do |
| --- | --- | --- |
| **Helix normal** (`hn`) | `HELIX` | word/find motions *move and re-select*; plain char/line motions collapse to a point at the target |
| **Helix select** (`hs`) | `HELIX-SEL` | every motion moves only the **head**, extending the selection (anchor held) |

`v` toggles between them (keeping the selection and its shape). `<Esc>` in select
drops to normal; in normal it collapses to a point and drops secondary
selections.

## Motions

Motions return a range, so the selection follows the cursor as you move.

| Keys | Selection |
| --- | --- |
| `w` / `W` | the next word / WORD **plus trailing whitespace**, stopping before the next word (vim's `w` lands *on* it) |
| `b` / `B`, `e` / `E` | back to the previous word start / forward to the next word end |
| `h` `j` `k` `l`, `0` `^` `$`, `G`, arrows | plain moves — collapse to a point at the target (in normal), extend (in select) |
| `f{c}` / `t{c}` / `F{c}` / `T{c}` | select forward/backward to (or till) `{c}` — and, unlike vim, they scan the **whole document**, matching the next occurrence on any line |
| `;` `,` (after a find) | repeat / reverse the last find |

Word motions are Helix-semantic (a line's leading indentation is its own word;
blank lines are skipped; a selection never spans a newline). `W`/`B`/`E` collapse
the word/punct classes so a run of any non-blank chars is one WORD.

## Verbs act now — no operator-pending

A verb operates on the **current selection** immediately, across every selection,
as one undo group:

| Key | Effect |
| --- | --- |
| `d` | delete the selection |
| `c` | delete and enter (multi-cursor) Insert — `<Esc>` resumes Helix normal |
| `y` | yank (the selection stays highlighted, Helix-style) |
| `>` / `<` | indent / unindent |
| `=` | reindent |
| `~` | switch case (keeps the selection) |
| `r{c}` | replace every selected character with `{c}` |
| `R` | replace the selection with the yank |
| `J` | join the lines the selection spans |
| `p` / `P` | paste **after** / **before** the selection (not the cursor char); with multiple selections each pastes its own slice of the last multi-yank |

So `wd` = select a word, delete it. `wcXyz<Esc>` = change a word to "Xyz". No
motion-wait ever.

## Selection verbs

Transforms on the selection **set** itself — these have no direct vim analog:

| Key | Effect |
| --- | --- |
| `x` / `X` | extend the selection to whole lines; repeat grows one line down / up |
| `%` | select the whole file |
| `_` | trim each selection to its non-whitespace content |
| `;` | collapse each selection to its head (a caret) |
| `,` / `Alt-,` | keep only the primary selection / drop the primary, keep the rest |
| `Alt-;` | flip anchor and head |
| `(` / `)` | rotate which selection is **primary** through document order |
| `Alt-(` / `Alt-)` | rotate the **contents** (text moves between selections; ranges stay put) |
| `&` | align every selection's start onto the same column ("align the `=` signs") |

## Multiple selections

Multi-selection is the default, so most verbs already fan out. To *grow* a
selection set:

- `C` / `Alt-C` — copy the primary selection onto the next / previous line(s),
  each copy becoming the new primary (walk it down/up with repeats).
- The regex spawners below (`s` / `S`).

Insert entry fans out too: `i` `a` `I` `A` collapse each selection to its insert
point and open a **multi-cursor** Insert at all of them; `o` / `O` open a fresh
line at every selection.

## Match mode (`m`) — brackets, text objects, surround

`m` opens a sub-grammar:

| Keys | Effect |
| --- | --- |
| `mm` | jump to the matching bracket (like vim's `%`) |
| `mi{o}` / `ma{o}` | select the **inner** / **around** text object `{o}` at each selection |
| `ms{c}` | surround each selection with the `{c}` delimiter pair |
| `md{c}` | delete the `{c}` pair surrounding each selection |
| `mr{from}{to}` | replace the `{from}` surrounding pair with `{to}` (two-stage, with a live preview) |

The text-object alphabet is the full shared set — vim objects (`w` `W` `p` `s`,
pairs `(` `{` `[` `<`, quotes `"` `'` `` ` ``) **and the tree-sitter captures**
`f` function / `a` argument / `c` comment / `t` class — so `mif` selects the
enclosing function body, `maf` the whole function, `mia` the argument. (Requires a
grammar with `textobjects.scm`; `btv.textobject.map` keys work here too.)

## Regex selection (`s` / `S` / `K` / `Alt-K`)

Each opens a `/`-style prompt that transforms the selection set; the would-be
result previews live as you type:

| Key | Effect |
| --- | --- |
| `s` | select each regex match **within** the selection (one selection per match) |
| `S` | split the selection **on** the regex |
| `K` / `Alt-K` | keep / remove selections that contain a match |

`s` over `%` (whole file) then a verb is the "change every occurrence" workflow —
e.g. `%sfoo<CR>c…`.

## Registers, view, and more

- **Register select** `"{reg}` — target register `{reg}` for the next
  `y`/`d`/`c`/`p`/`P`/`R` (one-shot), reusing vim's registers.
- **View menu** `z` — `zt` / `zz` / `zb` reposition the viewport around the cursor
  line, leaving the selection put.
- **Search** `/` `?` `n` `N` — a match becomes the selection in normal mode; in
  **select** mode each search / `n` *adds* the match as a new selection (growing a
  multi-selection).
- **Undo / redo** — `u` / `U`.

## Menus (the plugin layer)

The bundled `btv.helix` plugin binds the keys a bare grammar key can't reach:

- **Insert entry** — `i` `a` `I` `A` `o` `O` (all per-selection).
- **Goto menu** `g` — `gg` file start, `ge` last line, `gh` / `gl` line
  start/end, `gs` first non-blank, plus LSP `gd`/`gy`/`gr`/`gi` (definition / type
  / references / implementation).
- **Space menu** `<Space>` — the picker + LSP leader: find files, global search,
  buffers, document/workspace symbols, diagnostics, hover, rename, code action,
  jumplist, marks.

### which-key

The native which-key (`btv.on_key_pending`, see [UI primitives](ui-primitives.md))
lights up for Helix too — both the plugin menus (`g`, `<Space>`) and the native
sub-grammars: `m` match mode, `mi`/`ma` (the object alphabet, tree-sitter captures
included), `z` view, `f`/`t` find, `r` replace, and `"` register.

## Rebinding

Both Helix modes share one keymap bucket, `helix`, which **falls through** to the
native grammar on no match (so Helix stays fully usable with no config — the
plugin only *adds* keys). Every verb is exposed by name through
`btv.helix.actions.<name>`, so any key is rebindable:

```lua
-- alias X to "extend line" (x); user maps win over defaults
btv.keymap.set("helix", "X", btv.helix.actions.extend_line_below)

-- add a goto entry: gm -> last line (a count typed before it still applies)
btv.keymap.set("helix", "gm", btv.helix.actions.goto_last_line, { desc = "Last line" })
```

The action names mirror Helix's own command names (`extend_line_below`,
`select_regex`, `flip_selections`, `rotate_selection_contents_forward`, …), so a
Helix user's muscle memory carries over. An unknown name fails loud.

## Try it

The [`examples/helix`](../../examples/helix/init.lua) config is a runnable,
annotated walkthrough:

```sh
BEMTVI_CONFIG=examples/helix cargo run -p bemtvi -- examples/helix/sample.txt
```

## How it works (in brief)

Helix reuses vim's machinery rather than forking it. A `Range { anchor, head }` /
`Selections` vocabulary projects over the **existing** stores — the primary lives
in the vim cursor + visual anchor, secondaries in the same `CURSOR_NS` /
`ANCHOR_NS` extmarks the [multi-cursor mode](multicursor.md) uses — so selections
ride the buffer's edit choke point and undo for free. `Mode::HelixNormal` /
`HelixSelect` have their own parse step (`handle_helix`) but share the operator /
multi-cursor engine; the one predicate `Mode::shows_selection()` (visual **or**
Helix) lets Helix reuse the visual selection's rendering and multi-cursor sweep
wholesale.

For the full design — the projection seam, the grammar, the named-action registry,
and every verb — see the
[Helix editing model design record](../plans/2026-07-21-helix-editing-model.md).
