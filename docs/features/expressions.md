# Expressions

There are places where a fixed rule is not enough and a whole plugin is too
much: how a substitution builds each replacement, what a collapsed fold says,
which filetype a `.h` file really is. For those, bemtvi takes a small **Lua
expression** and evaluates it where the answer is needed.

```lua
-- double every number on every line
:%s/\d+/\=tonumber(m[0]) * 2/g

-- a collapsed fold shows its first line and how much it hides
btv.fold.text([[ first:gsub("%s+$", "") .. "  (" .. lines .. " lines)" ]])

-- a .h file with templates in it is C++, not C
btv.filetype.detect([[ ext == "h" and head:find("template", 1, true) and "cpp" or nil ]])
```

Each one is an *expression*, not a function body: it evaluates to a value, and
that value is the answer. There is no `return`, no statements, no `end`. (The
one exception is the frame-time paint, which is a **block** — a per-line paint
loops over the line's matches, so it ends in its own `return`.)

## The ten surfaces

| Where | In scope | Returns |
| --- | --- | --- |
| `:s/…/\=…/` | `m`, `lnum` | the replacement text |
| `"=` / `<C-r>=` | `line`, `lnum`, `col` | text to insert or paste |
| `'foldexpr'` | `line`, `lnum` | a fold level (see [Fold levels](#fold-levels)) |
| `btv.fold.text` | `first`, `lines`, `lnum` | a closed fold's collapsed row |
| `btv.indent.expr` | `prev`, `line`, `lnum`, `sw`, `previndent` | indent columns, or `nil` |
| `btv.filetype.detect` | `name`, `ext`, `head` | a filetype, or `nil` |
| `btv.picker.scorer` | `label`, `query`, `score` | a sort key, higher first |
| `btv.complete.scorer` | `label`, `query`, `score`, `kind` | a sort key, higher first |
| `btv.decor.expr` | `line`, `lnum` | spans to highlight (a **block**) |
| `btv.qf.text` | `item`, `idx` | one quickfix row's text |

Everything an expression needs is handed to it. That is the whole design, and
the reason for most of what follows.

## Substitutions

A replacement that starts with `\=` is an expression, evaluated once per match.
`m` holds the submatches — `m[0]` is the whole match, `m[1]`, `m[2]`, … the
capture groups — and `lnum` is the line the match sits on.

```lua
:%s/(\w+)_(\w+)/\=m[2] .. "_" .. m[1]/     -- one_two      -> two_one
:%s/\w+/\=m[0]:upper()/g                   -- alpha beta   -> ALPHA BETA
:%s/^/\=lnum .. ": "/                      -- number every line
```

A group that did not participate in the match is `nil`, not `""`, so
`type(m[2])` distinguishes "matched nothing" from "did not match".

The expression is compiled once per command and called per match, and it works
the same in bulk `:s`, in the live `inccommand` preview, and in the `:s///c`
confirm walk. The delimiter still ends the replacement, so an expression
containing `/` needs it escaped — or a different delimiter:

```lua
:%s#(\w+)/(\w+)#\=m[2] .. "/" .. m[1]#
```

A number is accepted and rendered as Lua prints it, so `\=tonumber(m[0]) * 2`
gives `14`, not `14.0`.

## Fold levels

`'foldexpr'` decides a line's fold level under `foldmethod=expr`. It sees the
line's own text as `line` and its 1-based number as `lnum`.

Set it through `btv.bo` rather than `:set`, since an expression contains spaces:

```lua
btv.bo.foldmethod = "expr"
btv.bo.foldexpr = "line:find('{%s*$') and '>1' or line:find('^%s*}') and '<1' or '='"
```

The value is vim's fold-level grammar, and it is worth knowing because it is how
you express *nesting* without the expression having to remember anything:

| Value | Meaning |
| --- | --- |
| `0`, `1`, `2`, … | this line is at that level |
| `>N` | a fold of level N **starts** here |
| `<N` | a fold of level N **ends** here |
| `aN` | N deeper than the previous line |
| `sN` | N shallower than the previous line |
| `=` | same level as the previous line |
| `-1` | undefined — take the shallower of the levels either side |

The engine carries the running level from line to line, so `>1` / `<1` bracket a
block however deeply it is nested, and your expression only ever looks at one
line.

Two ready-made values need no expression at all: `btv.treesitter.foldexpr` folds
by the syntax tree and `btv.lsp.foldexpr` by the language server's
`foldingRange`. Both are native — they never enter the sandbox.

```lua
btv.bo.foldexpr = "v:lua.btv.treesitter.foldexpr()"
```

## Fold text

`btv.fold.text` replaces the `+--  12 lines: …` placeholder a closed fold shows.

```lua
btv.fold.text([[ first:gsub("%s+$", "") .. "  (" .. lines .. " lines)" ]])
btv.fold.text(nil)   -- back to the built-in
```

`first` is the fold's first line verbatim, `lines` how many it covers, `lnum`
where it starts. The result is memoized on the first line, so a screen that is
not changing costs nothing.

## Indentation

bemtvi indents from tree-sitter's `indents.scm` wherever a grammar has one.
`btv.indent.expr` is the escape hatch for a filetype that has no grammar, or
whose indent query gets a construct wrong.

```lua
btv.indent.expr([[
  line:match("^%s*end") and previndent - sw
    or prev:match("then%s*$") and previndent + sw
    or nil
]])
```

Return the target indent in **columns**, or `nil` to decline. Declining matters:
it hands the line to `smartindent`/`autoindent` exactly as if no expression were
installed, so you only have to describe the cases you care about.

The precedence is tree-sitter → your expression → `smartindent` → `autoindent`.
Structure beats a hand-written rule, so an expression never overrides a grammar
that has an opinion.

## Filetype from content

bemtvi resolves a filetype from a file's name, a path pattern, or its extension.
Extensions that can only be settled by looking *inside* the file — `.h` for
C-vs-C++, and `.r`, `.v`, `.m`, `.pl` — are deliberately left alone rather than
guessed. `btv.filetype.detect` is where you answer them.

```lua
btv.filetype.detect([[
  ext == "h" and (head:find("template", 1, true) or head:find("::", 1, true))
    and "cpp" or nil
]])
```

`name` is the basename, `ext` the extension without its dot, and `head` the
first few lines (bounded, about 2 KB). Return a filetype to decide it, or `nil`
to decline and leave the built-in tables to answer — a returned filetype wins
over them, which is what makes the `.h` case work.

It runs once per buffer, and its verdict becomes that buffer's filetype exactly
as `:setf` would set it.

## Picker and completion ranking

`btv.picker.scorer` reorders a picker's results. It is handed the native fuzzy
`score` alongside the row's `label` and the active `query`, so the natural shape
is to *nudge* the existing order rather than reinvent matching:

```lua
-- push test files down; leave everything else where the matcher put it
btv.picker.scorer([[ score - (label:find("/test") and 50 or 0) ]])
```

It re-ranks only rows that already matched, and only the top 1000 of them.
Matching stays native, which is what keeps a picker responsive while 100 000
candidates stream in.

`btv.complete.scorer` is the same thing for the completion popup, with the row's
`kind` also in scope (`"Snippet"`, an LSP kind name, or `""` for a buffer word):

```lua
-- keep snippets available, but below real code completions
btv.complete.scorer([[ score - (kind == "Snippet" and 20 or 0) ]])
```

Its `score` is the **blended** native key — the fuzzy score plus the source's own
bias (`lsp` 8 > snippets 5 > buffer 0) — so nudging it composes with source order
rather than replacing it. Both re-rankers run at most once per repaint, never once
per streamed batch, and the completion caret follows the row it was standing on:
a popup that reordered under the caret would accept a candidate nobody chose.

## The expression register

`"=` is the register whose contents are *computed*. It is reachable from three
places, and each prompts with `=` for one expression:

```
i<C-r>=6*7<CR>          in Insert — inserts `42` at the cursor
"=lnum*10<CR>p          in Normal — the following `p` pastes the result
:e log-<C-r>=lnum<CR>   in the command line — splices the result into the line
```

In scope are `line` (the cursor's line text), `lnum` and `col` (1-based) — vim's
expression register can reach the whole Vimscript environment, so the pure
equivalent is handed what a computed insert actually wants.

A result ending in a newline pastes **linewise**, anything else charwise — so
`"="a\nb\n"<CR>p` opens two new lines below.

Two consequences of it being an ordinary register once evaluated:

- **It is evaluated when you submit the prompt, not each time it is read.** The
  text is then stored, so `getreg("=")` shows it and `p` pastes it — but
  `"=lnum<CR>p` followed by `j.` pastes the *first* line number again. (vim
  re-evaluates on read; bemtvi cannot, because the paste path resolves registers
  through a read-only borrow while the sandbox needs a mutable one.)
- **A computed insert or paste is not `.`-repeatable**, for the same reason `:s`
  and `d/foo` are not: it transits the command line. `.` replays the last
  repeatable change instead.

`<Esc>` at the prompt computes nothing. From a *nested* prompt it abandons only
the expression — the command line you were typing comes back untouched, as it
does when the expression fails.

## Painting a line

`btv.decor.expr` runs over every visible line **during** the frame and returns the
spans to highlight on it:

```lua
btv.decor.expr([[
  local out, i = {}, 1
  while true do
    local s, e = line:find("TODO", i, true)
    if not s then break end
    out[#out + 1] = { s, e, "Todo" }
    i = e + 1
  end
  return out
]])
```

Each span is `{ first, last, group }` with **1-based inclusive** columns — what
`string.find` hands back, so a match drops straight in. An empty list declines the
line. This is the surface that takes a block rather than an expression, for the
loop above.

It is the pure sibling of `btv.decor.provider`, and the choice between them is
what each can see:

| | `btv.decor.provider` | `btv.decor.expr` |
| --- | --- | --- |
| power | full Lua, async, any editor state | `line` and `lnum`, nothing else |
| when | off the frame, one frame later | during the frame it belongs to |
| draws | virtual text, signs, line backgrounds, highlights | highlight spans |

Reach for the block when the paint is a pure function of the text — indent guides,
colour swatches, trailing whitespace, a keyword badge — and for the provider when
it needs git, an LSP reply, or anything the line alone cannot tell it.

The paint is evaluated over each window's visible rows and memoized on the
viewport, so a steady screen costs nothing and only scrolling, resizing or editing
re-evaluates.

## Quickfix rows

`btv.qf.text` renders the rows of the quickfix window — vim's
`'quickfixtextfunc'`. It is the one surface handed a **record**: `item` is the
entry, keyed exactly as `btv.qf.getqflist()` returns it, and `idx` is its 1-based
position.

```lua
-- the message first, the location trailing
btv.qf.text([[ item.text .. "  (" .. item.filename .. ":" .. item.lnum .. ")" ]])

btv.qf.text(nil)   -- back to vim's `file|lnum col N| message`
```

`item` carries `filename`, `bufnr`, `module`, `lnum`, `end_lnum`, `col`,
`end_col`, `vcol`, `nr`, `pattern`, `text`, `type` and `valid`.

One row per entry is a hard rule — the row index *is* the entry index for `<CR>`,
`:cc` and the severity colouring — so a newline inside a rendered row is flattened
to a space, exactly as the default flattens a multi-line message.

It applies to the quickfix list, every named list and every window's location
list, and installing or clearing it re-renders the ones that are open.

## What an expression can do

The expression runs in a second, deliberately tiny Lua VM. It has the
value-level standard library — `string`, `table`, `math`, `utf8`, plus
`tostring`, `tonumber`, `type`, `pairs`, `ipairs`, `next`, `select`, `assert`,
`error` — and nothing else.

There is no `io`, `os`, `package`, `require`, `load`, `dofile` or `debug`, no
coroutines, and no `btv.*`. An expression cannot read the buffer, touch the
filesystem, or reach the editor at all. Everything it is allowed to know is in
its arguments.

### Why source text, not a function

You pass a *string* of Lua rather than a function:

```lua
btv.fold.text([[ first .. " (" .. lines .. ")" ]])   -- yes
btv.fold.text(function(first, lines) end)            -- fails loudly
```

The expression runs in a different Lua VM from your config, and a closure cannot
cross between VMs — it would carry references to everything it captured. The
source crosses instead, and is compiled on the other side.

### Expressions are stateless

Nothing carries from one call to the next, and assigning a global raises an
error. That is deliberate, and the reason is that none of these surfaces calls
your expression once per item in order:

| Surface | Why a counter would lie |
| --- | --- |
| `:s` | the live preview re-runs it on every keystroke you type |
| `'foldexpr'` | only the rows an edit touched are re-evaluated |
| `btv.picker.scorer` | only the top survivors, re-run on each repaint |
| `btv.complete.scorer` | the same, over the popup's rows |
| `btv.decor.expr` | only the rows on screen, and only when the viewport moved |
| `btv.fold.text` | memoized, so calls are skipped outright |
| `btv.filetype.detect` | once per buffer |
| `"=` / `<C-r>=` | evaluated once per prompt, then stored |

A counter over `:%s/x/…/g` on a three-`x` line would read `16 17 18`, not
`1 2 3`, because the preview got there first. Statelessness turns that class of
bug into an error you see immediately.

Fold nesting, the one case that genuinely wants carried state, is served by the
relative fold values above — the engine does the accumulating.

## When an expression fails

Every failure is reported, and none of them silently produces an empty value.

A **compile error** is caught when you configure the expression, before it can
touch anything. A **runtime error**, a **wrong return type** (a table where a
string was wanted), or an expression that runs longer than its 50 ms budget
aborts that use.

Surfaces that run continuously — fold text, indentation, filetype, picker
ranking — report the failure *once* and then uninstall the expression, falling
back to the built-in behaviour. An expression evaluated on every repaint cannot
be allowed to print the same error on every frame.

`:s` and the expression register are the exceptions, because they are one-shot.
A failing `"=` echoes, computes nothing, and leaves whatever was stored alone. A
failing `:s` expression aborts the substitution and says so: a compile error
leaves the buffer untouched; a failure partway through leaves the substitutions
already made, which one `u` undoes, since the whole run is a single undo step.

An expression that loops forever is stopped at its deadline rather than hanging
the editor, and the VM has a hard memory ceiling, so `string.rep("x", 1e12)`
fails the call instead of the process.
