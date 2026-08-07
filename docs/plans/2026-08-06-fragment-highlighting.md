# Highlighting code that isn't a whole program — LSP hover blocks and other fragments

> **Status: COMPLETE.** Both phases landed.

## Why this document exists

nxvim highlights *everything* with tree-sitter — including the fenced code blocks
inside LSP-generated documentation (hover, completion docs). Those blocks are not
programs. They are:

1. **fragments of the real language** — a struct field (`field: Vec<String>`), a
   statement, a signature with no body; and
2. **not the language at all** — an annotation dialect the server invents for
   display. `lua_ls` emits `function nx.tbl.get(t: table, ...: string)` in a
   ` ```lua ` fence; `tsserver` prefixes `(method) `; neither is valid source.

Both are handed to `Engine::highlight_text` (`crates/nxvim-core/src/editor/float.rs`
→ `Editor::preview_highlights`) as if they were a whole file.

### What actually happens today (measured)

Probed against the rust fixture grammar and a real installed `lua.so`:

| snippet | ERROR bytes | painted |
|---|---|---|
| `pub fn frob(x: &str) -> bool` (rust-analyzer, no body) | 0% | correct |
| `let x = some_call(a, b)` | 0% | correct |
| `field: Vec<String>` | 94% | `Vec` / `String` → `constructor` — **wrong** |
| `(method) Foo::bar(x: u32) -> bool` | 97% | `x`, `u32`, `bool` unpainted |
| `function nx.tbl.get(t: table, ...: string)` (lua_ls) | 98% | `function` **loses its keyword colour**; `table` / `string` → `variable.parameter`; `get` → `variable.member` |
| `local x: string` (lua_ls) | 50% | `string` → `module.builtin` |

Three conclusions, and they set the whole design:

- **Tree-sitter is already good at truncated-but-real code.** The "partial syntax"
  half of the problem barely exists — error recovery handles a body-less signature
  or a bare statement. Nothing to fix there.
- **The dialect half isn't degraded, it's confidently wrong.** A structural query
  matched a construct that isn't there, so the fragment gets *plausible, incorrect*
  colour. That reads worse than plain text, because the reader trusts it.
- **`ERROR`-node coverage separates the two cleanly** (0% vs 50–98%), and it is
  *local*: the untrustworthy captures are exactly the ones inside an `ERROR`
  subtree. We already have this signal in the tree and throw it away.

## The shape of the fix

A **fragment mode** on the stateless off-buffer highlighter. Whole-file surfaces
(the picker preview, the `:help` preview) keep the current path untouched — they
*are* whole files. Only the doc-float code blocks opt in.

### Phase 1 — trust structure only where the parse is sound

Language-agnostic, no configuration, no threshold. Inside every `ERROR` region of
the host tree:

- **drop the host layer's *structural* captures** that intersect it (they are the
  wrong ones) — but keep the ones that only classify a **token**. The lexer still
  worked where the parser didn't, so `@string` / `@number` / `@comment` /
  `@keyword` / `@operator` / `@punctuation` / `@constant` name something that really
  is in the text, while `@type` / `@function` / `@variable.parameter` /
  `@constructor` / `@property` name a *role* in a construct — precisely what error
  recovery guessed at. (`is_lexical_group`, matched on the capture's major segment.)
  Mutation testing caught this: a blanket drop also threw away rust's own
  `constant.builtin` on an integer literal and replaced it with a coarser guess.
- **repaint the rest** from the leaves' own token kinds, at a fill precedence
  *below* every real capture, so the surviving lexical captures (and any genuinely
  injected layer) paint over it and the repaint only shows through where the parse
  left nothing. Probing confirms the leaves inside an `ERROR` still carry real kinds —
  `"function"` as an anonymous token, `string_content`, `integer_literal`,
  `identifier` — so the mapping needs **no per-language tables**:

  | leaf | group |
  |---|---|
  | node kind contains `comment` | `comment` |
  | node kind contains `string` / `char` | `string` |
  | node kind contains `number` / `integer` / `float` | `number` |
  | node kind contains `keyword` | `keyword` |
  | anonymous token, text all `[A-Za-z_]` | `keyword` |
  | anonymous token `(` `)` `[` `]` `{` `}` | `punctuation.bracket` |
  | anonymous token `,` `;` `:` `.` | `punctuation.delimiter` |
  | anonymous token, anything else | `operator` |
  | everything else (identifiers) | *unpainted* |

  The walk stops descending at the first string / comment / number node and paints
  it whole, so a string's quote delimiters never leak out as `operator`.

Because the repaint is **local to the `ERROR` range**, a fragment that parses
cleanly (most rust-analyzer hovers, every `:help` example) produces byte-identical
output to today. The result for the `lua_ls` hover is `function` coloured as a
keyword, strings and numbers coloured, and *nothing confidently wrong*.

### Phase 2 — the framing ladder

Recovers the structure Phase 1 conservatively refuses to guess at. A snippet that
doesn't parse on its own is tried inside each of its language's framings in turn,
and the **first that parses cleanly** wins; its spans are mapped back onto the
snippet's own lines and columns. Measured on `field: Vec<String>` inside
`struct __nx { … }`: 94% → 0% error, `Vec` / `String` go `constructor` → **`type`**,
and `field` gains `property`.

**Only a clean parse is accepted** — no "lowest error coverage wins". The original
sketch ranked candidates by `ERROR` bytes, but a framing that merely fails
*differently* just relocates the guesswork, and the whole point of Phase 1 is that
a confident wrong answer is worse than none. Clean is a sharp, explicable bar: the
ladder fires exactly when it can turn a broken parse into a whole one, and
otherwise costs one throwaway parse and falls through to the repaint. "Clean" is
`Node::has_error()` on the root, which covers `MISSING` as well as `ERROR` (both
carry error cost), so a framing that only parses by having a token invented for it
doesn't count either. This also made the confidence *metric* unnecessary — a
boolean is all the selection rule needs.

The framings are per-language data, so they live in Lua (dogfood `nx.*`), with
shipped defaults for rust, lua, javascript, typescript, tsx, go, c, cpp and java:

```lua
nx.treesitter.fragment_context("rust", { "struct __nx {\n%s\n}", "fn __nx() {\n%s\n}" })
```

A same-line framing (`"fn __nx() { return %s }"` — the shape an *expression* needs)
works too: the mapping shifts the line index by the prefix's newline count and the
*first* line's columns by the prefix's trailing width, then clips anything the
framing owns (including a suffix sharing the fragment's last line). A template
with no `%s` is dropped rather than silently wrapping nothing; an empty list turns
the ladder off for the language.

#### Indentation-sensitive languages

A `%s` that follows **only whitespace** on its line means something different from
a same-line opener: the whitespace is the block level the whole fragment sits at,
not a prefix its first line continues. So an indenting framing repeats the indent
on *every* fragment line and takes that width back off *every* line's columns.
Without it, `"class __nx:\n    %s"` over a two-line fragment produces a header, one
indented line and then a dedent — a syntax error rather than a block.

The rule needs no new template syntax: whether the opener is pure whitespace
decides it. A blank line is left un-indented rather than given trailing whitespace.

The wrapped text also always ends in a newline. tree-sitter-go reports a `MISSING`
terminator after a final declaration, which is a defect under the clean-parse bar,
so without this the Go struct-field framing could never win despite producing a
perfect tree — measured, not theorized.

#### What the shipped framings actually recover (measured)

Against the real grammars, on the hover shapes servers actually send:

| language | fragment | outcome |
|---|---|---|
| python | `def foo(a: int) -> bool` | framed by `%s:\n    pass` → `def` keyword, `foo` function, `a` parameter, `int`/`bool` types |
| python | `class Foo(Base)`, `if x > 1`, `for i in items`, `@property`+`def …` | same rung — a body-less *header* is the commonest python hover there is |
| go | `Name string` | framed as a struct body → `Name` member, `string` type |
| go | `Read(p []byte) (n int, err error)` | framed as an interface body → `Read` method, parameters typed |
| javascript | `get name()`, `async fetchAll(ids)` | framed by `class __nx {\n%s {}\n}` → `get`/`async` keywords, `name`/`fetchAll` methods |
| json | `"key": 1` | framed by `{%s}` → `key` property, `1` number |
| lua | `field = 5,` | framed by `local __nx = {\n%s\n}` → `field` property |
| rust | `field: Vec<String>` | framed as a struct body → `Vec`/`String` types, `field` property |

Two honest notes from the same measurements:

- **Python's win comes from the colon-and-`pass` rung, not from indentation.**
  tree-sitter-python accepts `return`, `yield`, `await` and bare annotations at top
  level, so most python fragments parse *as-is* and never reach the ladder. The
  indenting rungs ship last and fire only for block-only content that arrives
  flush-left. The indentation *mechanism* is still load-bearing — it is what makes
  an indenting template correct at all, for the shipped rungs and for user-written
  ones.
- **A fragment with several body-less members** (`def a(self) -> int` / `def b(self)
  -> str` on consecutive lines) has no framing: each line would need its own colon
  and body. It falls through to the repaint, which is the right answer.

Extending `nx.treesitter.highlight` with a `fragment = true` option was left out:
no caller wants it yet, and the surface it exists for (the help window's `>lua`
blocks) is whole-file content.

### Considered and rejected

- **Dialect grammars.** `tree-sitter-luadoc` parses `---@param` comments, not
  `function f(a: string)`. Nothing upstream parses what `lua_ls` puts in its hover
  fence.
- **Per-server fence-language remap** (`lua` from `lua_ls` → some other grammar):
  needs a grammar that doesn't exist. Reconsider only if Phases 1–2 leave a real gap.
- **Per-line transforms beyond indentation** (rewriting a fragment to make it
  parse): indentation is a *framing* concern — it is part of where the snippet
  sits — but editing the snippet's own tokens would mean highlighting text the user
  isn't looking at. The ladder either finds a context the fragment fits in
  unchanged, or declines.
- **Asking the server.** LSP has no semantic tokens for hover markup.

---

## Phase 1 — implementation

**Touchpoints**

| file | change |
|---|---|
| `crates/nxvim-ts/src/engine.rs` | `highlight_fragment` (public); the shared snippet path grows a fragment mode; `extract_spans` takes the suppress ranges + fallback tokens |
| `crates/nxvim-core/src/syntax.rs` | `SyntaxEngine::highlight_fragment`, defaulting to `highlight_text` so the wasm JS-side engine is unaffected |
| `crates/nxvim-core/src/editor/syntax.rs` | `Editor::preview_highlights_fragment` |
| `crates/nxvim-core/src/editor/float.rs` | `render_markdown_into` highlights each fenced block through the fragment path |

**A separate bug the end-to-end test flushed out.** `render_markdown_into` joined a
block's lines with `\n` and **no trailing newline**, but the highlighter treats a
rope's last line as the phantom one (`len_lines - 1`) — so a **one-line** block
resolved to zero visible lines and *every* span was dropped. A bare signature on
one line is the commonest hover there is, so this was most of "hover isn't
highlighted", independent of any fragment reasoning. `redraw.rs`'s preview
projection already normalized this; `float.rs` now does too.

**Untouched on purpose:** the picker/`:help` preview (`redraw.rs`) and
`nx.treesitter.highlight` (`effects.rs`) — whole-file surfaces.

**Tests**

- `crates/nxvim-ts/tests/fragment_highlight.rs` — the engine behavior: the wrong
  structural capture is gone, a literal's own capture survives, a *structurally*
  captured keyword (the `lua_ls` shape, reproduced with a one-pattern query over the
  same compiled parser) is recovered, and a clean fragment is byte-identical to
  `highlight_text`.
- `crates/nxvim/tests/syntax.rs` — end-to-end through the completion-docs float (a
  Lua completion source whose `doc` is a fenced fragment), against the compiled
  fixture grammar, asserting the float's painted groups; plus the one-line-block
  regression above.

Both e2e tests were mutation-checked in isolation: reverting the fragment call site
resurfaces `constructor` in the float, and reverting the trailing newline leaves the
one-line block with no spans at all.

---

## Phase 2 — implementation

**Touchpoints**

| file | change |
|---|---|
| `crates/nxvim-ts/src/engine.rs` | `FragmentContext` (a template split at its `%s`), the `fragment_contexts` registry, `set_fragment_context`, `parses_cleanly`, the ladder in `highlight_fragment`, `unwrap_spans` |
| `crates/nxvim-core/src/syntax.rs` | `SyntaxEngine::set_fragment_context` (default: ignore — an engine that does no off-buffer highlighting has no ladder to configure) |
| `crates/nxvim-core/src/editor/syntax.rs` | `Editor::set_ts_fragment_context` |
| `crates/nxvim-lua/src/ops.rs` + `install.rs` | `TsOp::SetFragmentContext`, `nx._ts_fragment_context` |
| `crates/nxvim-server/src/effects.rs` | the op → editor. **No cfg split**, unlike `TsOp::SetQuery`: the prelude ships defaults for nine languages, so a loud wasm arm would greet every browser session with a wall of errors about a surface that isn't there |
| `crates/nxvim-lua/src/prelude/nx.lua` | `nx.treesitter.fragment_context` + the shipped defaults (22 languages) |
| `examples/fragment-highlighting/` | a completion source that fakes the four hover shapes (fragment / statement / body-less signature / dialect), plus `:FragmentLadderOff` |

**Tests**

- `crates/nxvim-ts/tests/fragment_highlight.rs` — the ladder: a clean framing
  recovers real structure, ordering (first clean framing wins), same-line column
  mapping, a dialect falling through to the repaint, and inert handling of an empty
  or `%s`-less list.
- `crates/nxvim/tests/syntax.rs` — the shipped framings reaching the engine from the
  prelude (mutation-checked by dropping the op), against the ladder-off run that
  isolates Phase 1's repaint.

The example was verified end-to-end with a throwaway harness test (removed before
commit, per the examples convention) — which is how a bad `nx.command.create` call
in it was caught: the real API is `nx.command(name, fn, opts)`.

---

## Follow-up spotted while measuring (not part of this work)

Probing the shipped javascript framings through a bare `Engine` returned almost no
spans, which looked like a mapping bug. It isn't: nvim-treesitter's javascript
query is `; inherits: ecma,jsx` plus ~56 lines of its own, and the inherit chain is
merged by `crates/nxvim-server/src/treesitter.rs` at startup — a bare `Engine` never
sees it. With the chain merged by hand the framings paint correctly (`get` keyword,
`name` method, `async` coroutine keyword). Worth knowing when reading engine-level
output: **`Engine` alone under-highlights every inherits-based grammar**, and any
future engine test for those languages has to merge the chain itself.

---

## Phase 3 — the two shapes the ladder can't take as written (2026-08-07)

Testing against real python servers turned up two hover shapes that fell all the
way to the repaint, each for a reason the ladder cannot fix by adding rungs: what
needs framing isn't the text as sent.

**A display label in front of the code.** `pyright` writes `(class) Asdfd`,
`(method) def join(self, x: str) -> str`, `(variable) count: int` — and `tsserver`
`(property) Foo.bar: number`. The label is the server's own annotation, not source;
its presence is what makes an otherwise framable signature unparseable, so every
one of these lost its structure. `annotation_prefix` takes it off — a parenthesised
run of words at the start of line 1, followed by a space and then code — the ladder
runs again on the remainder, the spans shift back over the label's width, and the
label itself paints `comment`, the non-code text it is.

**A block that is a list, not a fragment.** `ty` answers a hover on an overloaded
function with *every* signature, one per line (blank-separated or not). Each line
frames cleanly on its own; together they are a fragment of nothing, and the whole
block dropped to the repaint — so a two-overload hover was *less* highlighted than
a one-overload hover. `split_fragment` resolves line by line instead, each through
its own ladder and its own peel, possibly landing on different rungs (the test pins
a statement and a struct field in one block).

Both are all-or-nothing, which is the same rule Phase 2 already lives by: a peel
whose remainder doesn't frame leaves no trace (no label span over text nothing
explained), and one line that isn't a whole item drops the whole split. Otherwise a
"list" would be whatever lines happened to parse out of a context the parse says
isn't there. `MAX_SPLIT_ITEMS` (64) bounds the split: past it a block is far likelier
to be real source that failed to parse than a list of overloads, and the repaint is
the cheap answer.

One rung was added on the way: python's `def %s:\n    pass\n`, for a **bare**
signature (`join(self, x: str) -> str`) — which is what a `(method)` hover is once
its label comes off, in servers that don't repeat the `def`.

Measured on the real python grammar, all six shapes now come back fully framed
(`def` keyword, `join` function, `self` builtin, `x` parameter, `str` type); before,
four of them painted nothing but brackets and operators. tsserver's dotted
`(property) Foo.bar: number` still doesn't frame after the peel — `Foo.bar` is not a
member name in any TS framing — and correctly leaves no label span.

**Tests** — `crates/nxvim-ts/tests/fragment_highlight.rs`: the peel (structure +
columns), a peel whose body still fails leaving no trace, an item split across two
different rungs, blank lines and per-item labels riding it, and one unresolvable
line dropping the whole split. `crates/nxvim/tests/syntax.rs`: both shapes in one
doc float through the shipped framings (mutation-checked by disabling the peel and
the split).
