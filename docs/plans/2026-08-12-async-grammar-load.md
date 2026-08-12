# Getting the grammar load off the tick

`d1700547` put a hard 50ms budget back around all of a buffer's injection work and
made an over-budget region resume across frames instead of being dropped. What that
budget still cannot bound is the *first* cold grammar load on a frame: it runs to
completion, uninterruptible, and the editor is frozen for its duration. Same cost
the root language already paid — the deferred first highlight moves it off the
first paint, but the frame *after* the paint still blocks.

This plan removes it. It is worth doing in two independent halves, because the
measurement says most of the cost should not be paid at all, not merely paid
elsewhere.

## What was measured

Fixture: the rust grammar compiled from the cargo registry into a temp data dir
(`crates/nxvim-ts/tests/fixture/mod.rs`), debug build, this machine.

| step | cost |
| --- | --- |
| `dlopen` + ABI probe (`LoadedLanguage::load`) | **0.06 – 0.4 ms** |
| compiling one `highlights.scm` (3.5 KB, 161 lines) | **73 ms** |
| compiling *half* of that same file | **62 ms** |
| whole `Grammar::load` (this fixture ships one query) | **59 ms** |

Two things follow, and both contradict how the cost has been described in commit
messages so far (including mine — "a cold `dlopen` plus a compile", with the dlopen
named first):

1. **The dynamic library load is free.** It is ~0.1% of the total. Every millisecond
   is `Query::new`.
2. **A query's compile cost is dominated by the grammar, not the query.** Halving the
   source removed 15% of the time. `ts_query_new` analyzes each pattern against the
   language's parse table, so the floor is set by how big the *grammar* is. Compiling
   the same query twice costs the same both times — there is no shared per-language
   analysis to amortize.

So the cost is roughly **(number of query files) × (a per-grammar constant)**. And
`Grammar::load` compiles all five engine-relevant queries eagerly — `highlights`,
`indents`, `injections`, `folds`, `textobjects`. Real languages ship all five:

```
python:  folds highlights indents injections locals textobjects   (5 compiled, locals unused)
lua:     folds highlights indents injections locals textobjects   (5 compiled)
markdown:folds highlights indents injections                      (4 compiled)
vimdoc:  highlights injections                                    (2 compiled)
```

Five compiles at a rust-sized constant is ~300ms, which is the right order for the
~512ms figure measured for typescript earlier. **Painting a buffer needs one of
those five** (`highlights`), or two for a host language that injects
(`+ injections`). `indents` is needed on `=` / `o` with `smartindent`, `folds` on
`foldmethod=expr`, `textobjects` on `vif` — all user-initiated, all long after open.

## Phase 1 — compile a query when something asks for it

Entirely inside `nxvim-ts`; no seam, no async, no other crate. Expected to remove
~60% of a real language's load cost by not doing that work at open.

`Grammar`'s four optional query fields (`indents`, `injections`, `folds`,
`textobjects`) become lazily-compiled slots holding one of: not-looked-at-yet,
`None` (no such file), `Some(Query)`, or a load error to report. `highlights` stays
eager — it is required, its absence is already a load failure, and every caller of a
grammar wants it immediately.

- `Grammar::load` (`loader.rs`) reads and compiles `highlights` only. It still
  *stats* the four optional paths so "does this language have folds" stays answerable
  without a compile (`engine.rs:2129/2143/2162` probe exactly that today via
  `g.folds.is_some()`).
- Each use site (`engine.rs:1698` indents, `:1833` folds, `:1887` textobjects, and
  the injection-region walk) goes through an accessor that compiles on first ask and
  caches. This needs `&mut Grammar` at those sites — they hold `&Grammar` today, so
  expect the borrow rework to be the bulk of the phase.
- A compile failure on first use must still be loud (no silent stubs): it is reported
  once, then remembered as failed so a fold keypress doesn't re-echo per frame.
- `set_query` (`engine.rs:517`) installs a resolved override into a live grammar; on
  a slot that was never compiled it just fills the slot, which is strictly less work
  than today.
- **Injections keep a subtlety**: `injections` is compiled the first time a buffer of
  that language is highlighted, i.e. still on the paint path. That is one compile, not
  four, and phase 2 takes it off the tick with everything else.

Tests: extend `crates/nxvim-ts/tests/` — a grammar whose `folds.scm` is broken loads
fine and highlights, and only errors when a fold asks for it; a language with all
five queries costs measurably less to load than it does today (calibrated against a
measured compile like `injection_budget.rs` does, not a hardcoded ms).

## Phase 2 — the load itself moves off-tick

The precedent to copy is `:TSInstall`, which already does exactly this shape:
`HostEffects::ts_install` (`edithost.rs:406/630`) → `spawn_blocking` →
`UnboundedSender<InstallOutcome>` → drained in the run loop → `on_install_done`
(`excmd.rs:637`) → echo + repaint. The load is the same kind of work with a smaller
payload.

`Grammar` is `Send` (`Language`, `Query` and `libloading::Library` are all
`unsafe impl Send`), so the loaded grammar itself can cross the thread — no rebuild
on arrival.

- **Engine side.** `Engine::grammar()` stops loading inline. A miss records the
  language as *wanted* and returns not-loaded, which every caller already handles
  (the buffer paints plain; an injected region becomes a
  `PendingInjection { parser: None }`). New `Slot::Loading` prevents a second request
  for a language already in flight. Two new methods drain and fill:
  `take_wanted_grammars() -> Vec<String>` and an install that takes the loaded
  grammar back.
- **Crossing the core.** The editor holds `Box<dyn SyntaxEngine>` and `nxvim-core`
  must not learn tree-sitter types, so the trait gains
  `fn wanted_grammars(&mut self) -> Vec<String>` (plain) and
  `fn install_grammar(&mut self, lang: &str, loaded: Box<dyn Any + Send>)`, with the
  engine downcasting. Default impls keep every other `SyntaxEngine` untouched.
- **Server side.** After a highlight refresh (`treesitter.rs::refresh_buffer_highlights`)
  the server drains the wanted list and dispatches each through a new
  `HostEffects::ts_load_grammar(lang, overrides_snapshot)`; the outcome channel lands
  in the run loop next to `install_events`, installs the grammar, and repaints the
  buffer. `QueryOverrides` is a `HashMap<(String,String),String>` — cloned per
  request, so the worker needs nothing back from the engine.
- **Waking.** The load's *completion* is the repaint trigger. Deliberately not folded
  into `parse_pending`: that would spin the 5ms `PARSE_RESUME_TIMER` for the whole
  duration of a load to discover nothing changed.
- **Failure reporting moves with it.** `open` returns `OpenOutcome::LoadFailed`
  synchronously today and the editor echoes it. Asynchronously, a broken parser is
  echoed from the completion handler instead — same message, later frame. This is the
  one user-visible behavior change in the phase and needs its own test.
- **Ordering.** Requests are FIFO and deduplicated; the focused buffer's own language
  is naturally requested before the injections it discovers. `spawn_blocking`'s pool
  bounds concurrency; no extra cap unless a many-language document shows CPU churn.

Tests: a harness test that opens a file whose grammar is cold and asserts the server
answers an RPC barrier promptly *while* the load is in flight (today that request
waits out the whole compile), then that the highlights arrive without a keystroke.
Engine-level round-trip of wanted → install. A broken installed parser still echoes
its error exactly once.

## Phase 3 — the stateless surfaces

`highlight_text` / `highlight_fragment` / `preview_highlights` (picker preview, LSP
doc floats, `nx.treesitter.highlight`) are one-shot calls that must return spans
*now*, so a cold language returns unpainted text and needs a later repaint.

- The picker preview already owns a `PreviewCache` and a repaint path — invalidate
  the cached entry when a grammar it wanted lands.
- `nx.treesitter.highlight` is already promise-shaped; it can resolve after the load
  rather than resolving unpainted.
- Doc floats repaint on the next frame like a buffer.

## Not in scope

- **Preloading every installed parser at startup.** Costs the sum of all of them, at
  the moment the user is least willing to wait, for languages the session may never
  open. The reason to load lazily is unchanged.
- **A persistent on-disk cache of compiled queries.** tree-sitter has no serialized
  query form; this would mean owning one across grammar-ABI changes.
- **wasm / serverless.** `mod treesitter` is native-gated and the browser build
  highlights JS-side with no engine to load into, so the seam's wasm twin is a no-op
  and no language is ever requested there. A daemon session runs the engine
  server-side (native), so it is on the native path already and needs nothing extra —
  but per the tier-1 rule, verify with `--no-default-features` that the new trait
  methods compile out cleanly.

## Order and risk

Phase 1 is independent and lands the larger share of the win. Phase 2 is the bigger
change (three crates, a new effect, a behavior change in how a load failure is
reported) but is a copy of a shape that already works. Phase 3 is small and can trail.

The main risk in phase 1 is the `&Grammar` → `&mut Grammar` borrow churn at the use
sites; if it turns out to reach further than expected, interior mutability
(`OnceCell` per slot) keeps the signatures and is worth taking. The main risk in
phase 2 is a load that never completes leaving a language wedged in `Slot::Loading`
forever — the outcome channel must deliver on failure too, and the panic path of a
`spawn_blocking` task counts as a failure.
