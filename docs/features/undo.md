# Undo steps

bemtvi keeps vim's undo tree but not vim's undo steps. In vim, everything you
type between entering Insert and pressing `<Esc>` is one step, so `u` after a
paragraph takes the whole paragraph. bemtvi breaks typing into the steps a GUI
editor uses, roughly a word at a time. This is the same in every mode, Helix
included. There is no option to get vim's grouping back.

```
ifoo bar baz<Esc>   then  u  →  foo bar
                          u  →  foo
                          u  →  (empty)
```

## Where a step ends

You start a new step when you:

| You… | Example | `u` takes back |
| --- | --- | --- |
| type a separator after a word | `foo bar` | ` bar` (the space goes with the word after it) |
| type the first character of a new line | `foo<CR>bar` | `bar`, then `foo` and the line break |
| switch between typing and deleting | `foo<BS><BS>x` | `x`, then both `<BS>`s, then `foo` |
| pause for more than a second | `foo`, wait, `bar` | `bar` |
| move the cursor, then edit | `foo<Left>X` | `X` |

A run of separators stays together, so `foo   bar` undoes `   bar` in one step.
Punctuation counts as a separator: `foo.bar(x)` is four steps.

Moving the cursor counts however it happens: an arrow, `<Home>`/`<End>`, a mouse
click, a jump, or a plugin calling `btv.win.set_cursor`. An `i_CTRL-O` command is
a step of its own, even one that doesn't move the cursor.

## What stays one step

- A Normal-mode command, including the text a `c` command types. `cwnew<Esc>u`
  restores the old word in one `u`. With more than one word typed, the last ones
  come off first.
- A paste, a register insert (`<C-r>{reg}`, `<C-r><C-w>`), and an `<C-r>=`
  result. The typing after one of these is a new step.
- A mapping's output. One key pressed is one step, however much text its
  right-hand side types, for a string RHS, an `<expr>` RHS, or a Lua function
  that feeds keys. A mapping that changes nothing leaves the word you were typing
  in one piece.
- A snippet expansion. Typing over a placeholder, or into an empty tabstop, is a
  separate step, so `u` puts the placeholder back before it removes the snippet.
- An edit made with several cursors. Each step spans every cursor.
- A replay: `.`, a macro (`<F3>{reg}`), `:normal`, `:g`. `u` after `.` undoes
  the whole repeat.

## The tree is unchanged

Each step is a node in the same undo tree, so `g-`/`g+`, `:earlier`/`:later`,
`:undo {N}`, `:undolist` and `btv.undotree` all work on word-sized states.
`'undolevels'` still counts nodes, and with smaller steps the default of 1000
covers less editing than it did.

## Tests and fed keys

Keys a plugin feeds with `btv._feedkeys` are one step per run, like a mapping.
Keys fed as typed are the keyboard: the `btv.test` runner's `t:feed` uses them,
so a spec that types `ifoo bar<Esc>u` sees `foo`, the same as you would.
