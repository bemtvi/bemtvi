# A bounded Lua sandbox, and `:s/…/\=…/` replacement expressions

*2026-08-16*

## The gap

Two gaps, one of which is the other's cause.

1. **`:s` cannot compute a replacement.** vim has `\=`: the replacement is an
   *expression*, evaluated per match with the submatches in scope
   (`:%s/(\w+)/\=submatch(1):upper()/`). bemtvi's replacement is a literal
   template only — `$1`, `&`, `\t` and friends, expanded by
   `search.rs::expand_replacement`. There is no escape hatch.
2. **bemtvi has no *safe synchronous* extension point at all.** Everything a user
   can supply today is either **async** (the `btv.*` promise world, which by
   construction cannot be awaited from a synchronous core path) or **native**
   (fast, but not customizable). Every sync-path expression vim offers —
   `indentexpr`, `foldtext`, `formatexpr`, content-based filetype detection — is
   blocked on the same missing primitive, and `'foldexpr'` only works today
   because `folds.rs` defers evaluation to *between* input and redraw and eats a
   stale frame for it.

`:s` is the cleanest instance: it is synchronous core code, the main Lua VM is
reached only through the server's async mirror, so there is no way to call user
Lua mid-substitution. The fix is a second, **bounded, pure, synchronous** Lua VM
the core can call under a deadline.

## The measurement that shaped this

Prototyped both candidate engines before committing (mlua 0.12 / PUC Lua 5.4 vs
rhai 1.25.1), same algorithms, checksum-verified identical results:

| scenario | native Rust | Lua 5.4 | Rhai 1.25 | rhai/lua |
| --- | --- | --- | --- | --- |
| call overhead (empty body) | — | 39ns | 223ns | 5.7x |
| substitute expression | 72ns | 420ns | 2074ns | 4.9x |
| fuzzy score | 13ns | 1665ns | 16938ns | 10.2x |
| indentexpr | 13ns | 723ns | 3155ns | 4.4x |

Three findings drove the design:

- **Lua wins 4.4–10x**, and the gap is inherent (rewriting the Rhai scorer to
  hoist `to_chars()` out of the loop changed nothing — it is tree-walking
  interpreter overhead, not a string-indexing artifact).
- **Lua's debug-hook penalty is flat and untunable** — ~+65% whether the hook
  fires every 200 or every 100,000 instructions, because any nonzero `hookmask`
  trips the `trap` flag in PUC `luaV_execute` and every instruction takes the
  slow dispatch path. You pay for *having* a hook, not for firing it. This is a
  first-class argument for a **separate VM**: the penalty stays contained in the
  sandbox and the main VM keeps its fast dispatch.
- **Rhai's `max_operations` is nearly free (+2%)** — architecturally the nicer
  sandbox — but it starts 10x behind, so *bounded* Lua (2.8us) still beats
  *unbounded* Rhai (16.9us) by 6x. Lua also keeps the config surface in one
  language.

Interruption was verified working in both: a `while true` loop stopped at exactly
5.00ms under a wall-clock budget.

Against this project's own accepted synchronous budget (`PARSE_DEADLINE = 50ms`,
`bemtvi-ts/src/engine.rs:35`), that makes **per-line / per-match** work viable and
**per-item-over-every-candidate** work not:

| workload | Lua | verdict |
| --- | --- | --- |
| `:%s/…/\=…/` over 10k matches | 4.2ms | fine |
| `=G` over 10k lines | 7.2ms | fine |
| picker rank, 100k candidates | 280ms | **too slow — must stay native** |
| picker re-rank, 200–1000 survivors | 0.6–2.8ms | fine |

## Non-goals

- **Not the main VM, and not reachable from it.** The sandbox has no `btv.*`, no
  editor state, no mirror, no `require` of plugin code, no I/O. It cannot mutate
  anything; it returns a value the core uses. This keeps the
  Lua-reads-go-through-the-mirror invariant intact by construction.
- **Not async.** Pure functions only. Anything that waits belongs in the main VM.
- **Not a replacement for `indents.scm`.** Treesitter indent stays the canonical
  structural source; a sandbox `indentexpr` would sit *below* it (a later phase).
- **Not a picker *scorer*.** Measured too slow over `all_items`; a later phase
  re-ranks the already-filtered survivor set only.
- **Not `submatch()`/Vimscript spelling.** The expression is Lua, and the
  submatches arrive as a plain table.

## The seam

Exactly the `SyntaxEngine` shape, which is the established way `bemtvi-core`
hosts an engine it cannot own (the core stays pure and synchronous, so it cannot
link Lua):

- **`bemtvi-core/src/sandbox.rs`** — the `SandboxEngine` trait plus its handle and
  error types. Core-side only; no Lua in sight.
- **`Editor::sandbox: Option<Box<dyn SandboxEngine>>`**, installed by the server
  through `set_sandbox_engine`, mirroring `set_syntax_engine`.
- **`crates/bemtvi-sandbox`** — the implementation: one `mlua` VM with a stripped
  global environment and a wall-clock deadline hook.

`None` means no sandbox is installed; every call site must degrade or fail loud
rather than fake a value.

## Phase 1 — the sandbox, and `:s/…/\=…/`

### The sandbox VM

A single `Lua` built with a **closed** environment: the pure stdlib only
(`string`, `table`, `math`, `tostring`/`tonumber`/`type`/`pairs`/`ipairs`/`select`,
`utf8`) and **nothing** that does I/O, loads code, or reaches the host — no `io`,
`os`, `package`, `require`, `dofile`, `load`/`loadstring`, `debug`, no
coroutines. Enforced by building an explicit environment table, not by deleting
globals after the fact.

Bounded by a **wall-clock deadline** per call, enforced by an instruction hook
that compares `Instant::now()` against a per-call budget and returns an error to
unwind — the same abandon-and-fall-back contract `deadline_budget` gives
tree-sitter parses.

### The `:s` surface

A replacement beginning `\=` is a **Lua expression** (compiled as `return (…)`),
evaluated once per match. In scope:

```lua
m        -- table of submatch strings: m[0] is the whole match, m[1..n] the groups
         -- (a group that did not participate is nil)
lnum     -- 1-based line number of the match
```

So `:%s/(\w+)_(\w+)/\=m[2] .. "_" .. m[1]/` swaps the halves, and
`:%s/\d+/\=tostring(tonumber(m[0]) * 2)/` doubles every number.

The expression is compiled **once** per `:s` invocation and called per match, so
compile cost (7.5us) is paid once, not per match.

The delimiter still terminates the replacement, so an expression containing the
delimiter must escape it — the same rule vim has.

### Threading it through

`search.rs` gains one canonical replacement abstraction rather than a parallel
code path:

```rust
pub(crate) enum Repl<'a> {
    /// The literal template, expanded against captures (`$1`, escapes).
    Template(&'a str),
    /// A compiled sandbox expression, called per match with the group texts.
    Expr(&'a mut dyn FnMut(&[Option<&str>]) -> Result<String, SandboxError>),
}
```

`substitute_line` and `match_replacement` take `&mut Repl` and return `Result`.
Both regex engines can produce the uniform `&[Option<&str>]` group slice (PCRE
from `Captures`, the vendored vim engine from `LineMatch::submatches`), so `\=`
works under **both** `'regexsyntax'` values rather than being PCRE-only.

All three substitute call sites route through it, so the expression form works
identically in bulk `:s`, in the `inccommand` live preview
(`refresh_subst_preview`), and in the `:s///c` confirm walk (`subst_confirm_seek`):

| site | file |
| --- | --- |
| bulk substitute | `ex.rs:1882` |
| live preview | `ex.rs:1628` |
| confirm walk | `ex.rs:2359` |

### Failure, loudly

Per the no-silent-stubs rule, every failure aborts the substitute with a
message — none of them silently yields empty text:

- compile error → `E:` with the Lua parse message, before any line is touched
- runtime error → aborts the command, reporting the failing line number
- deadline expiry → aborts, reporting the budget
- a non-string, non-number result → aborts (a table/nil is a bug, not `""`)
- **no sandbox installed** → aborts, loud

A *compile* error is caught before the edit pass begins, so the buffer is
genuinely untouched. A runtime, deadline or bad-return failure can only be found
mid-run; it aborts there, leaving whatever was already substituted — but the run
takes a single undo snapshot, so one `u` reverts the partial pass.

The live preview is the one place that must *not* echo per keystroke; it drops to
showing no preview for an erroring expression and lets the real `:s` report.

## Later phases (not this commit)

2. **Picker re-rank of survivors** — *shipped.* See below.
3. **The `*expr` family** — *shipped.* See below.

## Testing

Black-box through the harness, as always —
`crates/bemtvi-server/tests/editing/subst_expr.rs`:

- capture access, group reordering, arithmetic on `m[0]`, `lnum`
- a non-participating group arrives as `nil`
- the expression runs once per match, and per *match* not per line (`/g`)
- compile error, runtime error, wrong return type, and deadline expiry each abort
  loudly and leave the buffer unmodified
- the sandbox cannot reach `io`, `os`, `require`, `load`, or `btv`
- `\=` under both `'regexsyntax'` values
- the confirm walk (`:s///c`) and the live preview agree with bulk `:s`

## Outcome

Phase 1 shipped as described: 14 new black-box tests green, and the full suite at
**3919 passed / 0 failed**. Six notes where reality differed from the sketch.

- **`pcall`/`xpcall` had to be excluded from the sandbox environment.** A deadline
  unwinds as an ordinary Lua error, so an expression able to catch errors could
  swallow its own deadline and spin forever inside a `pcall` loop — the bound
  would have been decorative. There is a test asserting `pcall` reads `nil`.
- **A memory ceiling joined the time budget.** Bounding time alone still lets
  `string.rep("x", 1e12)` take the process down, so the VM carries a 16 MiB
  allocation limit as well.
- **One install site covered all three worlds.** The sandbox is installed in
  `EditHost::new` — documented as the single construction site shared by the
  native server and the wasm edit-host — so `:s/…/\=…/` works locally, over a
  daemon and in the browser from one line, rather than needing the usual
  native/wasm cfg-split. Both configurations were built to confirm it
  (`-p bemtvi-server` and `--no-default-features`).
- **`\=` works under both regex engines.** The plan expected this to be possible;
  it was, because `Repl` takes a uniform `&[Option<&str>]` that PCRE `Captures`
  and the vendored engine's `LineMatch::submatches` both produce. No PCRE-only
  carve-out was needed.
- **The engine has to be detached from the editor for the duration of a run.**
  A substitute loop must borrow the editor (to rewrite lines) and the engine (to
  evaluate) at once, which one `&mut self` cannot give. `Editor::with_sandbox`
  lifts the engine out and restores it centrally, so an early `return` on an
  erroring expression cannot lose it.
- **A control test caught a dialect ambiguity, not a regression.** Asserting the
  literal path still worked, `$2_$1` produced `one`: `$2_` reads as a group
  *named* `2_`. That is the documented behavior `${2}` exists to disambiguate —
  the test was wrong, the code was right, and it is now written with braces.


---

## Phase 2 — `btv.picker.scorer`

### The surface

`btv.picker.scorer(src | nil)` installs (or clears) a re-ranker over a picker's
surviving rows. `src` is a string of Lua **source**, not a function, because the
re-ranker runs in the sandbox — a separate VM, and a closure cannot cross
between VMs. Three names are in scope, and the expression returns the new sort
key, higher first:

```
label   the row's text
query   the active query
score   the native fuzzy score the row already earned
```

Handing in `score` is what makes this a *re-ranker* rather than a matcher
replacement — the archetypal use nudges the native order:

```lua
btv.picker.scorer([[ score - (label:find("/test") and 50 or 0) ]])
```

### The two bounds that make it safe

The measurements in phase 1 said scoring every candidate is not viable (280ms
for 100k rows). Two constraints keep this inside a frame:

- **Survivors only, capped.** The scorer sees the *filtered* set, never
  `all_items`, and at most `RERANK_LIMIT` (1000) of it. A loose query over a
  huge list can still leave tens of thousands of survivors; the tail keeps
  native order, which is invisible because nobody scrolls there. Worst case is
  ~3ms.
- **Once per repaint, not once per batch.** This is the subtle one. A streamed
  picker rebuilds its view per arriving batch, so re-ranking where the view is
  rebuilt would turn `extend_view`'s deliberate O(batch) into O(view)-per-batch
  — exactly the shape the never-freeze rule exists to prevent. Instead `Menu`
  carries a `rank_dirty` flag and the server settles it from `redraw`, right
  before projecting, next to where the generic Lua `'foldexpr'` is settled.

### Failure

A scorer runs every repaint, so a broken one must not echo every keystroke: the
first failure (raise, deadline, or non-number key) reports once and
**uninstalls** the scorer, degrading to native order. A compile error is caught
at configure time instead, where it belongs.

### Outcome

Shipped as described; 9 new black-box tests, full suite **3928 passed / 0
failed**. Notes:

- **The seam grew a second call shape rather than a generic one.** `compile_expr`
  now takes the parameter list (`["m", "lnum"]` vs `["label", "query", "score"]`)
  and each call shape gets its own typed `call_*`, keeping the marshalling
  explicit — as the phase-1 trait doc anticipated.
- **`settle_picker_rank` lives in `menu.rs`, not `editor/sandbox.rs`.** The view
  internals (`filtered`, `match_spans`, `rank_dirty`) are private to the menu
  module, and reaching them from a sibling would have meant widening them.
- **A scorer set inside an `<expr>` mapping is discarded**, like every other
  queued effect: `Shared::discard_effects` destructures exhaustively, so the new
  field failed to compile until it was classified.
- **Mutation-tested.** With the `settle_picker_rank` call commented out, 3 of the
  9 tests fail (reorder, failure-report, bad-return) and the ones asserting
  native order stay green — confirming they measure the feature rather than
  passing incidentally.

---

## Phase 3 — the `*expr` family, and retiring the deferred foldexpr

Four surfaces, each the same shape: a Lua expression, compiled at configure time,
evaluated in the sandbox, failing loud exactly once.

| surface | in scope | returns |
| --- | --- | --- |
| `btv.fold.text` | `first`, `lines`, `lnum` | the collapsed row's text |
| `btv.filetype.detect` | `name`, `ext`, `head` | a filetype, or `nil` to decline |
| `btv.indent.expr` | `prev`, `line`, `lnum`, `sw`, `previndent` | indent columns, or `nil` |
| `'foldexpr'` (the option) | `line`, `lnum` | vim's fold-level value |

Each sits where the core previously had to either refuse or defer:

- **`foldtext`** replaces a hardcoded default whose doc comment said "Customizable
  `foldtext` is a later phase". Building the view holds only `&Editor` and `Fold`
  is `Copy`, so the text is memoized on the editor — keyed by the fold's
  `(start, count)` *plus the first line it was rendered from* — and filled by
  `settle_fold_text` before projection. Keying on the first line is what makes a
  steady screen free.
- **Content-based filetype** answers what `mod.rs` documents as "omitted rather
  than guessed". It runs once per buffer and writes its verdict as the buffer's
  explicit filetype, so `buffer_filetype` (which is `&self`, on hot paths) never
  reaches the sandbox.
- **`indentexpr`** slots into `indent_for` below the treesitter verdict and above
  `smartindent`; `nil` declines.

### Retiring the main-VM foldexpr

The generic `'foldexpr'` used to run in the **main** VM, which core cannot call,
so it went out through a deferred round-trip: `pending_foldexpr` → the server's
`folds.rs` → `LuaRuntime::eval_foldexpr_lines` → `set_foldexpr_values`, costing a
frame of stale folds. All of that is **deleted**. `compute_generic_expr_folds`
now fills its own unevaluated rows synchronously, so the levels are ready in the
frame the edit landed in.

This is a deliberate compatibility break, taken on the user's instruction: a
foldexpr can no longer call `vim.fn.getline` or reference a main-VM function via
`v:lua`. It does not need to — the row's own text is passed in as `line`, which
is precisely what lets it be pure.

### Outcome

Shipped; full suite **3947 passed / 0 failed**. Notes:

- **Two bugs the tests caught in the sniffer.** It was keyed on buffer id alone,
  but `:e` *reuses* an empty unnamed buffer — so the startup buffer's verdict
  stuck to whatever file was opened into it. And `head` could never be empty,
  because building it appended a newline per line. Both were invisible from the
  code; only a probe showed `ext=""` and a `head` with the wrong content.
- **An empty `'foldexpr'` must mean flat, not an error.** `:set foldmethod=expr`
  before `:set foldexpr=…` is the ordinary intermediate state, and compiling `""`
  raised a syntax error on a perfectly normal config.
- **The perf guard had to change shape.** `fold_perf.rs` counted evaluations with
  a main-VM counter the expression incremented; a pure sandbox cannot increment
  anything. It now measures the same typing *against a baseline with no foldexpr*,
  which is what isolates the foldexpr from the per-keystroke frame cost — a first
  attempt that compared against one full pass failed, because redraw over 5000
  lines dominates. Mutation-tested: forcing whole-buffer re-evaluation makes it
  14.3s against a 496ms baseline, a 29x separation.
- **The `fold_incremental` suite was passing vacuously** after the switch — its
  `v:lua` expression could not run, so every fold was flat and every
  spliced-equals-fresh assertion held trivially. Ported to the sandbox model; it
  is load-bearing again (one test asserts two expressions produce *different*
  folds, which flat levels cannot satisfy).

---

## Follow-up — the sandbox is stateless, and now enforced to be

The environment table was shared by every compiled chunk and writable, so an
expression could carry state — `rawset(_G, …)`, or a global assignment inside an
immediately-invoked closure — and a later, separately-compiled expression could
read it. The docs called the sandbox "pure"; it was not.

What made this worth closing rather than documenting is *how* it misbehaved. A
counter over `:%s/x/…/g` on `x x x` returned **16 17 18**, not 1 2 3: the
`inccommand` live preview had already evaluated the expression fifteen times
while the command was being typed. That is the general shape of the problem —
**no call shape here is a clean once-per-item traversal**:

| shape | why an accumulator is wrong |
| --- | --- |
| `:s` | the live preview re-runs it on every keystroke |
| `'foldexpr'` | the splice evaluates only the rows an edit touched |
| picker scorer | only the top `RERANK_LIMIT` survivors, re-run per repaint |
| `foldtext` | memoized, so calls are *skipped* |
| `filetype.detect` | once per buffer |

So: the environment chunks run in is an **empty** table whose reads fall through
to the allow-list and whose writes raise. Empty is load-bearing — `__newindex`
only fires for absent keys, so exposing the allow-list directly would have left
every one of its names quietly assignable. The stdlib tables are frozen the same
way (they are shared, so a writable `string` is a channel between expressions),
the metatable is hidden, and `rawset`/`rawget` are withheld because they bypass
metatables entirely.

Fold *nesting* — the one case that genuinely wants carried state — is already
served without it: the relative fold values (`>N`/`<N`/`aN`/`sN`/`=`) let the
expression stay per-line-pure while `ranges_from_foldexpr_values` accumulates the
running level in Rust. Per-line purity is what buys the incremental splice.

### Future: a whole-collection call shape

If a real need for carried state turns up, the shape to build is **not** per-call
state but a call that receives the whole collection at once — e.g. a
`btv.fold.levels(src)` handed every line and returning every level, or a picker
scorer handed the whole survivor set. That is stateful *within* one deterministic
call, so it has none of the traversal hazards above.

It is not being built now because it trades away the incremental splice: a
whole-buffer foldexpr is O(buffer) per edit, which is exactly what
`fold_perf.rs` guards against. It would want to be opt-in per buffer rather than
the default, and to carry its own perf guard.

---

## Phase 4 — the expression register (`"=` / `<C-r>=`)

*2026-08-17*

### The gap

`registers.rs:10` names it: *"only the expression register `"=` is not yet
wired."* Every other special resolves (`"%` the file name, `".` the last insert,
`"/` the last pattern); `"=` is the one that needs to *compute*, which needed a
synchronous VM. `is_readonly_register` already lists `'='`, so the write side is
guarded; only the read side is missing.

It is the `:s/…/\=…/` call shape reached from three places instead of one:

| entry | what it does |
| --- | --- |
| `<C-r>=` in Insert | prompt, evaluate, insert the result at every cursor |
| `"=` at a command boundary | prompt, evaluate, store — the following `p`/`P` pastes it |
| `<C-r>=` in the command line | prompt, evaluate, insert the result into the line being typed |

### In scope

`line`, `lnum`, `col` — the cursor's line text, its 1-based line number, and its
1-based column. Vim's expression register has the whole Vimscript environment;
ours is pure, so what a computed insert actually reaches for is passed in
instead. Three names cover the real uses (`lnum * 2`, a padded index, munging the
line you are standing on) and each is a scalar the core already has.

Returns the text to insert, through the same `text_result` conversion every other
surface uses — so a number is fine (`<C-r>=6*7` inserts `42`).

### Evaluated once, at confirm — not per read

Vim re-evaluates `@=` on every read. bemtvi evaluates when the prompt is
confirmed and stores the resulting text in the `=` cell, because
`Editor::register_text` is `&self` (it is on paste-hot paths) and evaluating
needs `&mut` for the engine. The consequences, all stated rather than discovered:

- `"=lnum<CR>p` then `j.` pastes the *first* line number again. Vim would
  re-evaluate and give the second.
- `@=` from Lua (`getreg("=")`) reads the stored result, and the register mirror
  sees the write like any other, so the value is introspectable — which
  re-evaluation-on-read could not offer.
- `<C-r>=` in the command line therefore has a defined meaning even before the
  nested prompt lands: the stored result.

### The prompt

A new `CmdlineKind::Expr(ExprTarget)`, opened exactly the way
`enter_helix_regex` opens its prompt: `cmdline_return_mode` remembers where to go
back to, the kind routes `<CR>` in `submit_cmdline`, and the per-target payload
lives in an `Editor` field rather than in the enum (`CmdlineKind` is `Copy`, as
`let kind = self.cmdline_kind` in `submit_cmdline` requires). `cmdline_type()`
reports `=`, matching vim's prompt character.

`ExprTarget` is the `Copy` tag — `Insert`, `Register`, `Cmdline`:

- **Insert** resumes the insert flavour it was opened from and routes the result
  through `insert_text_session`, the same primitive `<C-r>{register}` uses, so a
  multi-cursor insert and the `".` accumulator behave identically.
- **Register** restores the count and `register = Some('=')` onto
  `Editor::pending` at confirm, because entering the command line calls
  `reset_pending`. Restoring is what makes the *following* `p` see the register —
  vim's `"=…<CR>p` ordering.
- **Cmdline** saves the whole outer command-line state and restores it with the
  result spliced in at its cursor. A stack, not a single slot, so an expression
  prompt opened from an expression prompt costs nothing extra.

### Failure, loudly

Compile error, runtime error, deadline expiry and a bad return type each echo and
insert nothing, leaving the `=` cell as it was. There is no partial write: the
evaluation happens before anything is inserted or stored.

### Phases

- **4A** — the seam's `call_eval`, the prompt, the Insert and Register targets.
- **4B** — the Cmdline target (the nested prompt).

### Testing

`crates/bemtvi-server/tests/editing/expr_register.rs`, black-box as always:
arithmetic and string results, `line`/`lnum`/`col` in scope, the linewise rule,
`"=…<CR>p` and `"=…<CR>P`, the count surviving the prompt, insertion at every
cursor with multi-cursors, each failure mode echoing and inserting nothing, the
register mirror seeing the stored result, `<Esc>` cancelling, and (4B) `<C-r>=`
splicing into a `:` line and a `/` line without disturbing what was already
typed.

---

## Phase 5 — `btv.complete.scorer`

### The gap

`settle_picker_rank` bails on `menu.kind != MenuKind::Picker` (`menu.rs:1057`),
so the completion popup — the same `Menu`, the same `filtered` / `match_spans`,
the same fuzzy pass — cannot be re-ranked. A config can bias whole *sources*
(`MenuItem::priority`, `lsp` 8 > snippets 5 > buffer 0) but cannot say "demote
snippets while I am typing a call" or "prefer the shorter candidate".

### In scope

`label`, `query`, `score`, `kind` — and `score` is the **blended** native key
(`fuzzy + priority`), the one `sort_complete_view` sorts on, so nudging it
composes with the source bias rather than fighting it. `kind` is the row's kind
label (`"Snippet"`, an LSP `CompletionItemKind` name, `""` for a buffer word).

The source *name* is deliberately not in scope: `MenuItem` does not carry one,
and inferring the source from `kind` is exactly the heuristic the "fix at the
canonical layer" rule warns about. If source-level ranking is wanted, the fix is
to thread the real name onto `MenuItem`, as its own change.

### Where it runs

Once per frame, from `redraw`, beside `settle_picker_rank` — not in
`sort_complete_view`, which runs per arriving batch and would turn its O(batch)
into O(view)-per-batch. Capped at `RERANK_LIMIT`. `rank_dirty` already covers the
"nothing changed" case.

One difference from the picker: the re-rank must **follow the selected row**, the
hazard `sort_complete_view` documents at length — `self.cursor` is a position in
the view, so reordering under a caret would make `<CR>` accept a candidate nobody
chose. The permutation helper both paths share preserves the selected item's
identity for a `Complete` menu.

### Failure

Identical to the picker's: a scorer runs every repaint, so the first failure
reports once and **uninstalls** itself, degrading to native order. A compile
error is caught at configure time.

### Testing

`crates/bemtvi-server/tests/complete_scorer.rs`: a scorer reorders the popup, the
caret follows the row it was on, `kind` and the blended `score` are really in
scope, a broken scorer reports once and uninstalls, `nil` clears it, and the
order without a scorer is unchanged.

### Outcome

Shipped as planned, both sub-phases in one commit: 34 new black-box tests
(`editing/expr_register.rs`), 3 new example specs, full suite **3990 passed / 0
failed**. Mutation-tested — dropping the insert and restoring the wrong parse
stage fails 14 of the 34. Six notes where reality differed from the sketch.

- **The restored pending needs `Stage::Start`, not the stage `"` left behind.**
  Cloning `Editor::pending` verbatim carried `Stage::RegisterPending`, so the key
  after the prompt was read as a *register name* — and `p` is a valid one, which
  made `"=…<CR>p` silently select register `p` and paste nothing. The saved state
  is now built explicitly (count + `register = Some('=')` + a clean stage).
- **A computed insert is not `.`-repeatable, and that fell out of an existing
  rule** rather than needing one: any command that transits `Mode::Command` is
  already marked non-repeatable centrally (the same rule that excludes `:s` and
  `d/foo`), so `.` replays the last repeatable change. vim *does* repeat
  `i<C-r>=…<CR>` by replaying the prompt keys, which bemtvi cannot: the submit
  key is a `cmdline`-bucket keymap resolved in the server, and dot-repeat re-feeds
  through `Editor::input`, where a Command-mode `<CR>` is inert. Pinned by tests
  in both directions.
- **The nested prompt must restore its outer line on *every* path.** The first
  cut returned early on an empty or failing expression, which threw the suspended
  command line away — a typo at the `=` prompt cost the whole `:s` you were
  typing. The result is threaded as an `Option` instead, and `<Esc>` restores too,
  so the line always comes back.
- **Visual `"=…<CR>p` does not replace the selection**, and that is a
  pre-existing gap unrelated to this: bemtvi's `p` pastes at the cursor in every
  mode. What this phase owes Visual mode is that the prompt *returns* to it with
  the selection intact, which is what the test asserts.
- **`SavedCmdline` had to be a struct saved wholesale.** The prompt is a full
  command line, so twelve fields — text, cursor, kind, prompt label, return mode,
  visual origin, history index, the `btv.ui.input` history/completion opt-ins, and
  a `/` search's count and origin — all have to come back. Field-by-field at the
  call site would rot the moment a thirteenth is added.
- **The example turned up two spec-hygiene facts, neither about this feature.**
  A test that edits the sample leaves that buffer modified and the per-test
  baseline does not restore it, so `open()` needs `:e` *then* a bare `:e!` (`:e
  <path>` switches to an existing buffer without re-reading). And an unterminated
  `:s` trims its trailing whitespace, so the demo's replacement is terminated.

### Outcome

Shipped as planned: 13 new black-box tests (`tests/complete_scorer.rs`), 2 new
example specs, full suite **4003 passed / 0 failed**. Mutation-tested — dropping
the `settle_complete_rank` call fails 9 of the 13. Four notes.

- **The two re-rankers now share their reorder.** Both needed the same
  permutation over the parallel `filtered` / `match_spans`, and the completion one
  additionally needed the selection-identity handling `sort_complete_view` already
  had. Extracted as three `Menu` methods (`chosen_identity`, `follow_identity`,
  `reorder_head_by_keys`) that all three callers use, so the popup's
  don't-move-the-caret rule has one implementation rather than two.
- **`kind` is in scope; the source *name* deliberately is not.** `MenuItem`
  carries no source name, and inferring the source from the kind label is exactly
  the heuristic the "fix at the canonical layer" rule warns against. If
  source-level ranking is wanted, the fix is to thread the real name onto
  `MenuItem` as its own change.
- **Two test fixtures had to be built around the fuzzy scores rather than
  assumed.** `-score` does not reverse two candidates that *tie* fuzzily (`zoom`
  and `zombie` against `zo`), so the inversion test uses two sources whose only
  difference is the source bias — which also proves `score` carries the blended
  key. And "the query is in scope" needs a promotion that *fights* the native
  order at the longer query, else the assertion passes vacuously.
- **"Reported once" is not directly observable, so the test asserts the
  consequence.** The message line holds the last echo either way; what proves the
  uninstall is that the popup returns to native order, which also makes a repeat
  impossible.

---

## Phase 6 — a table-returning seam, and frame-time paint (`btv.decor.expr`)

*2026-08-18*

### The gap

`decor.rs:10` states it: *"neovim's frame-time `on_win`/`on_line` model can't host
that on the PUC backend (ADR 0002 rule 4), so the trigger is detached from
rendering."* `btv.decor` therefore wakes **off** the frame, hands the provider a
viewport snapshot, and takes a generation-stamped publish back — machinery a
decoration needs when it draws from git, an LSP reply, or any state only the main
VM can reach.

A decoration that is a **pure function of one line** needs none of it, and pays
for all of it: indent guides, `#rrggbb` swatches, trailing whitespace, a TODO
badge all land a frame late, which is visible as flicker while scrolling. The
sandbox can run *during* projection. What it cannot do is return a decoration:
every `call_*` on the seam returns a scalar.

### The seam

Two additions, both mechanical:

- **`compile_block(src, params)`** — the source is a function *body* rather than a
  single expression, because a per-line paint naturally loops over matches. Same
  closed environment, same deadline, same memory ceiling; only the wrapper differs
  (`function(…) <src> end` rather than `function(…) return (<src>) end`). Scoped to
  this surface — the other six stay expressions, where a `return` would be noise.
- **`call_paint(f, line, lnum) -> Vec<PaintSpan>`**, with
  `PaintSpan { start, end, group }` in **0-based, end-exclusive byte** offsets —
  what an extmark takes.

Lua returns a list of `{ first, last, group }` with **1-based inclusive** columns,
because that is exactly what `string.find` gives back:

```lua
local out, i = {}, 1
while true do
  local s, e = line:find("TODO", i, true)
  if not s then break end
  out[#out + 1] = { s, e, "Todo" }
  i = e + 1
end
return out
```

The engine validates every element (a 3-tuple of two integers and a string) and
the core clamps each span to the line and snaps it to character boundaries. A
malformed element is a `BadReturn`, never a silently dropped span.

### The surface

`btv.decor.expr(src | nil)` — a sibling of `btv.decor.provider`, not a
replacement. The choice between them is what each can see:

| | `btv.decor.provider` | `btv.decor.expr` |
| --- | --- | --- |
| power | full Lua, async, any editor state | pure: `line`, `lnum`, nothing else |
| when | off-frame, generation-stamped | during projection, same frame |
| draws | extmarks — text, virt lines, signs | highlight spans |
| costs | a round trip, and one frame of lag | ~1us per visible row |

### Where it runs

`settle_decor_expr()` from `redraw()`, beside `settle_picker_rank` and
`settle_fold_text`. For every visible window it evaluates each visible row and
writes range extmarks into a new reserved `PAINT_NS`, replacing that buffer's
previous ones — the ephemeral-mark pattern the `:s` diff preview already uses, so
the paint rides `highlights_for` to every client with no client change at all.

Two bounds keep it inside a frame. The work is **screen-bounded by construction**
(the visible rows of the visible windows, ~50 each), and it is **memoized on
`(buffer, top, bot, changedtick)` per window** — the same key
`recompute_decor_dirty` uses — so a steady screen costs nothing and only scroll,
resize or an edit re-evaluates.

### Failure

Loud once, then uninstall, like every surface that runs continuously: a paint
expression that errors, exceeds its deadline, or returns a malformed span reports
once and is removed, and the buffer paints as it did before. A compile error is
caught at configure time.

### Testing

`crates/bemtvi-server/tests/decor_expr.rs`: spans reach the projected highlights,
the columns are the 1-based inclusive ones `string.find` returns, several spans on
one line, a line the expression declines (an empty list), scrolling repaints the
new rows, an edit re-evaluates, the marks do **not** leak into the user-facing
extmark mirror, each failure mode reports once and uninstalls, and the paint
survives (moves with) an edit on the same line.

### Later, on the same seam

A table *param* gives `quickfixtextfunc` (an entry in, a rendered line out), and
the same table return gives an `errorformat` alternative that parses one line of
build output into an entry. A `PaintSpan` that grows optional `virt_text` /
`sign_text` extends the paint without touching the seam again.

### Outcome

Shipped as planned: 18 new black-box tests (`tests/decor_expr.rs`), 2 new example
specs, full suite **4034 passed / 0 failed**. Mutation-tested twice — dropping the
`settle_decor_expr` call fails 15 of the 18, and dropping the *memo* takes the
steady-screen guard from 0.26s to 4.95s against a 55ms baseline. Five notes.

- **The paint had nowhere to be seen from a plugin test, so the framework grew a
  third view.** `t:lines()` is buffer text and `t:screen()` is the glyphs drawn; a
  highlight changes neither, so no spec could observe a decoration at all — not
  this one, and not a `btv.decor.provider`'s either. `t:highlights([row])` mirrors
  the focused window's spans, read back out of the window value already built
  rather than re-projected, so a spec sees exactly what the client was sent.
- **The memo is the feature, not an optimization.** Without it, in-viewport cursor
  motion re-evaluates every visible row on every keystroke. The guard measures the
  same keystrokes against a no-block baseline (the shape `fold_perf.rs` settled
  on), with the per-call cost kept to a few milliseconds so a loaded machine
  cannot trip the 50ms deadline and pass it *vacuously* by uninstalling the paint.
- **The whole buffer's paint is rebuilt when any window showing it moves.** Marks
  are per buffer while viewports are per window, so clearing only the stale
  window's rows would strand the other window's spans.
- **A block, not an expression** — and the docs' opening claim ("no `return`, no
  statements") needed the exception stated rather than quietly broken.
- **`set_ui_mirror` outgrew clippy's argument limit** at eight, which was the
  nudge to give it a `UiMirror` struct: every field is a different projection of
  the same frame, and the test framework's accessors map onto them one for one.
