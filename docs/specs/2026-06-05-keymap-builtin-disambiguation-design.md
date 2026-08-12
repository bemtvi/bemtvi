# Unified command disambiguation — make built-in commands instant under colliding maps — design & phased plan

**Date:** 2026-06-05
**Status:** **✅ Implemented (Phases 1–3, 2026-06-05).** Built on the keymap engine
([2026-06-04-keymap-design.md](2026-06-04-keymap-design.md), Phases 1–4) and the
`timeoutlen` idle flush (D4). Every multi-key built-in (`gg`, `dd`/`dw`,
`f{char}`/`;`/`,`, `r{char}`, `diw`/`ciw`, and the visual / visual-line variants)
now fires instantly under a colliding user-mapping prefix — no idle flush, no
next key. The pure parser lives in `bemtvi-core/src/editor.rs` (`parse_step` +
`command_status`); the matcher consults it as a read-only oracle in
`bemtvi-server/src/keymap.rs`. Coverage is the disambiguation + edge-audit blocks
in `crates/bemtvi-server/tests/keymaps.rs`. Per-phase notes are inline below.

**Sanity-checked against the code (2026-06-05).** The root cause, the cited line
refs (`keymap.rs:323`, `:328-340`; `keymaps.rs:267`/`:246`), and the "8 pending
fields" all verified accurate. Five refinements were folded in below, flagged
inline as **[refined]**: (1) the canonical fix-the-fold semantics of
`command_status` for multi-command runs; (2) the precise matcher-integration rule
(what "the released run" is, and the raw-replay gate); (3) the governing
release-precedence invariant that keeps user maps winning; (4) the real Phase-1
hazard — classification is *fused* with buffer-reading target computation in
`resolve_motion`/`find_motion`/`text_object_range`, so "zero drift" only holds if
`execute` dispatches on the typed `ResolvedCommand`, never on raw keys; (5) a
required test update the original plan missed (`idle_flush_completes_a_withheld_
prefix` asserts the very lag this removes) plus a doc-target correction for Phase 3.

This document is both a design and a phase-by-phase implementation plan. Each
phase is handoff-ready for a fresh context window: prerequisites, the files it
touches, the surface it adds, the **black-box tests** that prove it, and a hard
"done when" gate. Read *Problem* and *Design* first, then execute the phases in
order.

**Chosen architecture: unified.** There is exactly **one** normal-mode command
grammar, used by both the editor's executor and the keymap matcher. This is the
final design, deliberately favored over a cheaper separate-oracle approach — see
*Why unified* below.

---

## Problem

With a user `g`-prefix mapping in place, built-in `g` commands lag:

```lua
vim.keymap.set('n', 'gh', function() print('gh') end)
```

Typing `gg` does **not** jump to the top immediately — it resolves only on the
*next* keystroke or after the `timeoutlen` idle flush. neovim has no such lag: `gg`
is instant, and a key-hint popup shows `gg`, `gh`, … as peers under the `g` prefix.

### Why bemtvi lags (root cause)

The keymap engine (`crates/bemtvi-server/src/keymap.rs`) is, by design **D1**,
*editor-unaware*: its prefix trie holds only **user mappings** (`gh`). It does not
know `gg` is a complete built-in. So:

1. First `g` → live prefix of `gh` → **withheld** in `pending` (`keymap.rs:323`).
2. Second `g` → `[g,g]` matches nothing, **breaks** the prefix (`Classify::None`,
   `keymap.rs:328-340`): the engine replays the first `g` to the editor, then
   **re-feeds the trailing `g`**, which in isolation is *still* a live prefix of
   `gh` → **re-withheld**.

The editor receives only one `g`; the second waits for the next key or the flush.
The existing test even encodes the limitation —
`crates/bemtvi-server/tests/keymaps.rs:267` feeds `"gg0"` with a comment admitting
the `0` is there "to flush that buffered `g`."

neovim never re-examines that trailing `g` alone, because it disambiguates against
the **combined** {mappings ∪ built-in commands} space. A sequence that completes a
built-in fires immediately; nothing extends `gg`, so there is nothing to wait for.

### The fix in one sentence

Make the editor's command grammar a **shared, pure classifier** and let the matcher
consult it: when a withheld run forms a complete built-in command, release it to the
editor instead of re-holding it as a speculative mapping prefix.

---

## Why unified (and not a separate oracle)

A cheaper option is a second, standalone grammar function that mirrors the
dispatcher. It works, but it has one fatal property **in this codebase**: bemtvi
uses **black-box integration tests only** (architecture.md → *Testing philosophy*;
no unit tests, by rule). A standalone mirror is therefore never directly tested, so
any divergence from the real dispatcher is a **silent failure** — a future
normal-mode command added to the executor but not to the mirror would lag behind
every colliding mapping, with no test that fails. Unifying the grammar — one
parser, consumed by both the executor and the matcher — makes that entire class of
drift bug *structurally impossible*. Given the explicit goal of a final version, the
editor.rs refactor this requires is the correct cost to pay.

---

## Design

### Two non-negotiables (unchanged)

1. **`bemtvi-core` stays pure and synchronous.** The new parser is pure; no async,
   no I/O, no transport types. (Buffer mutation stays in the executor, as today.)
2. **The matcher stays a pure function of its inputs.** It consults a **read-only**
   classifier — the pure `command_status` free function — but mutates nothing,
   preserving D1's "the engine never touches the editor." (Whether that classifier is
   *injected* as `feed(mode, key, &oracle)` or *called directly* is an implementation
   choice; see Phase 2's `[refined]` note — direct is simpler here. The non-negotiable
   is read-only-ness, not the plumbing.)

### The core split: parse → execute

Today `Editor::handle_normal` (`crates/bemtvi-core/src/editor.rs`) interleaves three
things in one pass: it reads pending state (8 scattered fields — `count`,
`op_count`, `operator`, `gpending`, `pending_replace`, `pending_textobject`,
`pending_find`, plus search-operator state), *decides* whether a key extends /
completes / aborts a command, and *executes* buffer effects — all in the same match
arms. We separate the **decision** (pure, grammatical) from the **effect**
(buffer-touching):

```rust
/// The accumulated, not-yet-complete normal-mode command. Replaces the 8
/// scattered pending fields; one value, one place.
#[derive(Default, Clone)]
struct PendingCommand { /* count, op_count, operator, stage: Stage, … */ }

/// What the pure parser decides for (pending, key). No buffer, no mutation.
enum ParseStep {
    Prefix(PendingCommand),    // incomplete; keep accumulating
    Complete(ResolvedCommand), // a full command, ready to execute
    Invalid,                   // no command begins this way → reset/bell
}

/// THE grammar. The single source of truth. **Mode-aware**: the same dispatch
/// serves normal and visual, whose grammars differ (see *Mode coverage*).
fn parse_step(mode: Mode, pending: &PendingCommand, key: Key) -> ParseStep;

/// Applies a finished command to the buffer (the existing execution helpers:
/// apply_resolved_motion, begin/apply operator, replace_char, text objects, …).
fn execute(&mut self, cmd: ResolvedCommand);
```

`handle_normal` becomes a thin loop: `parse_step` → on `Prefix` stash the
`PendingCommand`, on `Complete` call `execute`, on `Invalid` reset. **Crucially,
classification is purely grammatical** — `dw` is `Complete` regardless of what `w`
selects; `fx` is `Complete` because `f` takes one char argument; `gg` is `Complete`
because nothing extends it. So `parse_step` needs **no buffer and no `&mut`**, which
is exactly what lets the matcher reuse it safely.

**[refined] The actual hazard, and why the typed enum is load-bearing.** Today the
"is this a command?" decision is *fused* with buffer-reading target computation in
three `&self` functions: `resolve_motion` (`editor.rs:1302` — the **motion
alphabet** plus where each motion lands), `find_motion`/`find_char_target` (the find
arity plus the char search), and `text_object_range` (`editor.rs:1518` — the
**object-kind alphabet** plus the range search). `parse_step` is pure, so it
**cannot call them** — it must own those alphabets itself. That re-creates exactly
the drift seam the *Why unified* argument set out to kill, only moved one layer in:
if `parse_step` says `w` is a motion but `resolve_motion`'s arms disagree (one is
edited without the other), a built-in lags with no failing test. The unification's "zero drift" is
therefore **only real if `execute` dispatches on the typed `ResolvedCommand`
`parse_step` emits — never re-matching raw `Key`s.** Make `Motion`/object-kind/find
typed enums that `parse_step` produces and `resolve_motion`/`text_object_range`
consume via an exhaustive `match`; then a new built-in is a new variant the compiler
forces into *both* the parse arm and the effect arm. (Note `resolve_motion`'s
`Some`/`None` is already buffer-*independent* for every direct motion arm; it only
yields `None` from grammar/state — `gpending && key != g`, `;`/`,` with no
`last_find` — or from a find/`;`/`,` that doesn't match in the buffer. The latter is
an *execution* miss, not a classification one: `parse_step` calls these `Complete`
and `execute` no-ops on the buffer miss, exactly as `handle_normal` does today.)

### The oracle is a fold over the same grammar (zero drift)

```rust
pub enum CommandStatus { Complete, Prefix, Invalid }

/// Classify a key run against the command grammar for `mode` by folding
/// `parse_step` from a clean command boundary. SAME function the executor uses —
/// they cannot diverge.
///
/// [refined] Fold rule (the load-bearing detail): start from a default
/// `PendingCommand`; for each key call `parse_step`. On `Prefix(p)` carry `p`
/// forward; on `Complete(_)` **reset to a fresh default** and keep folding the
/// remainder; on `Invalid` short-circuit. The run's status is:
///   - `Complete`  iff the fold ends **exactly on a command boundary** (the last
///                 key produced `Complete` with nothing carried) — i.e. the run is
///                 a whole number of finished commands;
///   - `Prefix`    iff it ends mid-command (a non-empty `PendingCommand` carried);
///   - `Invalid`   iff any step was `Invalid`.
pub fn command_status(mode: Mode, keys: &[Key]) -> CommandStatus;
```

**[refined] Why the reset-on-`Complete` matters.** The break path can hand the
oracle a run of *more than one* built-in command, and the release rule needs
"ends clean," not "is one command." Worked cases (normal mode):

| run | fold | status | release? |
|-----|------|--------|----------|
| `[g]` | `Prefix(gpending)` | **Prefix** | no — lone `g` waits (correct) |
| `[g,g]` | gpending → `Complete(top)` | **Complete** | yes — `gg` instant |
| `[d,d]` | op=d → `Complete(dd)` | **Complete** | yes |
| `[x,x]` | `Complete(x)` → reset → `Complete(x)` | **Complete** | yes — a **single-key** built-in behind a colliding multi-key map (`xy` mapped) is released too |
| `[f]` / `[r]` | `Prefix(find/replace)` | **Prefix** | no — needs its argument |
| `[f,x]` | find → `Complete` | **Complete** | yes |

The `[x,x]` row is the reason single-key commands must live in `parse_step` even
though, taken alone, they never need disambiguation: behind a colliding map their
*second* occurrence is the key that would otherwise hang.

### Mode coverage (the fix applies to normal **and** visual)

The matcher already runs per mode (it selects the trie by `mode_key`), so the
oracle is mode-parameterized and the fix covers both modes that have multi-key
built-ins:

- **Normal** and **visual** share `parse_step` but with mode-conditioned arms,
  mirroring today's `mode.is_visual()` branches in `handle_normal`: in visual,
  `d`/`c`/`y` are `Complete` (they act on the selection immediately) and `i`/`a`
  are `Prefix` (text-object starts, `iw`/`a(`), whereas in normal `d`/`c`/`y` are
  `Prefix` (operators) and `i`/`a` are `Complete`. Motions (`gg`, `f{char}`, …) are
  identical in both and extend the selection in visual. So with `gh` mapped, a
  visual-mode `gg` extends to the top instantly, and visual `iw` under a colliding
  object map resolves instantly — same as normal.
- **Insert** and **command** modes are intentionally **unchanged**. Their trailing-
  prefix hold (`jk` mapped → `jj` waits for the flush) already *matches* neovim,
  because a literal inserted char is not a multi-key built-in that anything
  "completes." The divergence this doc fixes exists only where built-in multi-key
  commands do — normal and visual. `command_status` returns the conservative
  current behavior for these modes (the matcher simply never gets a `Complete` that
  would change a hold).

### Matcher integration (the behavioral change)

Confined to the **break path** and **idle-flush path** in `keymap.rs`. New rule:

> When the matcher is about to **re-withhold** a trailing key as a fresh
> user-mapping prefix, first ask the oracle whether the run already released to the
> editor in *this* resolution **plus** that key is a `Complete` built-in command. If
> so, **release** the key to the editor (`Step::Editor`) instead of withholding it.

**[refined] Made precise** (the rule above hides three details the implementation
must nail):

- **"The released run" =** the keys `resolve_buffered` just emitted as
  `Step::Editor` *raw* immediately before the re-feed — the `None` arm of
  `resolve_buffered` (`keymap.rs:388`), where the buffered prefix matched no
  mapping. Thread that slice into the recursive `feed_key` (or, equivalently, do the
  oracle check in the break path before re-feeding). It is **not** maintained across
  a `fire` — see the gate.
- **Raw-replay gate.** Only consult the oracle when the preceding buffered keys were
  replayed *raw*. If `resolve_buffered` instead *fired a shorter mapping*
  (`longest_complete` hit), the run is "a mapping fired, then leftover keys" — not a
  clean built-in command sequence — so the oracle must not splice the leftover onto
  it. (In the runs the break path actually produces this is the common case anyway:
  a colliding prefix like `g`/`d`/`f` has no mapping at the short node, so it
  replays raw.)
- **Governing invariant (why user maps still win).** The oracle is consulted *only*
  where the **mapping** trie yields `Classify::None` (break) or where `flush` finds
  **no complete mapping** prefix. A run that is itself a live mapping prefix or a
  complete mapping never reaches the oracle — it stays on the mapping path. So:
  release-to-editor (instant) happens **exactly when the run breaks every live
  mapping prefix**; while the run is still a live mapping prefix, holding it (and
  resolving via the idle flush) is correct and *matches neovim's `timeoutlen`*.
  Concretely, with `ggx` mapped, typing `gg` is a genuine live prefix of `ggx`, so
  it waits for the flush — exactly as neovim waits `timeoutlen` — and the oracle
  never fires (no break). User mapping ≻ built-in is preserved structurally.

Worked trace, `gh` mapped, input `gg`:

1. `feed g` → `pending=[g]`, mapping `Prefix` → held. (Oracle agrees `[g]` is
   `Prefix` — both say "wait"; matches neovim's g-menu.)
2. `feed g` → `pending=[g,g]`, mapping `None` → break path:
   - replay first `g` (`Step::Editor(g)`),
   - re-feed second `g`: would be held (`Prefix` of `gh`) → **oracle check**:
     `[g] + g = [g,g]` → `Complete` → **release** `g` (`Step::Editor(g)`).
3. `feed` returns `[Editor(g), Editor(g)]`; editor runs `gg` → **top, instantly. No
   flush, no next key, no dangling `gpending`** (both `g`s reached the editor in
   order).

Because the parser is the *whole* grammar, this fix applies to **every** built-in at
once: `dd` under a `dh` map, `f{char}` under `fh`/`ff`, `r{char}` under `rx`,
`diw`/`ciw` under colliding object maps — all instant.

`ggh` (with `gh` mapped) → `gg` then `h`; the `gh` map does **not** fire, matching
neovim. **[refined] Note this is a *fix*, not a preserved behavior:** today `ggh`
sends one lone `g` to the editor (arming `gpending`) and then *fires the `gh`
mapping* on `[g,h]`, leaving the editor in a dangling `gpending` — visibly wrong.
The disambiguation corrects it (the second `g` releases as a built-in before `h`
arrives), so its Phase 2 test must be confirmed **red on current `main`** like the
others. The genuinely ambiguous case is preserved: with `ggh` *mapped* (so `gg` is a
real mapping prefix **and** a complete command), `gg` is held and the idle flush
takes the shorter map — unchanged D4 / `timeoutlen` behavior.

### What does not change

- `gh` typed as `gh` still fires the mapping (the second key extends the prefix to
  `Complete`; the break path is never entered).
- `<nowait>`/`<silent>`/`<expr>`, remap re-feeding, precedence/last-wins,
  buffer-local filtering: untouched.
- The idle flush stays, now only for truly-ambiguous mapped prefixes and for
  releasing a lone prefix (e.g. a solitary `g`) to the editor after `timeoutlen`.

### Scope / known limitation

Operator-pending mapping (`omap`) stays deferred (`keymap.rs` mode buckets have no
`'o'`). A normal-mode map whose LHS collides with an operator's *argument* (mapping
`ip`, typing `dip`) interacts with that deferral, not this work — out of scope. The
oracle classifies from a clean command boundary, correct for the runs the break path
actually sees (they begin at a command boundary).

---

## Phases

Execute in order. Each ends green on `cargo test --workspace` +
`cargo clippy --all-targets -- -D warnings` (**default features only** — never
`--all-features`, per CLAUDE.md).

### Phase 1 — Extract the normal-mode parser (behavior-preserving, the big one) — ✅ implemented

> **Landed (commit `refactor(core): Phase 1`).** `parse_step`, `PendingCommand`,
> `Stage`, `ResolvedCommand`/`ParseStep`, and the typed `Motion`/`ObjectKind`/
> `FindKind`/`NormalCmd` sub-enums are in `bemtvi-core/src/editor.rs`. The 8
> scattered pending fields are gone — one `PendingCommand` on `Editor`, with
> `last_find` kept as cross-command memory. `handle_normal` is now a thin
> `parse_step → execute` loop; `execute` dispatches on the typed `ResolvedCommand`
> (not raw keys), and `resolve_motion`/`text_object_range` match on the sub-enums,
> so parse↔execute can't drift. Behavior-preserving: the whole existing suite
> stayed green unchanged.

**Prereq:** none beyond current `main`.

**Do:**
- Introduce `PendingCommand`, `Stage`, `ResolvedCommand`, `ParseStep`, and the pure
  **mode-aware** `parse_step(Mode, &PendingCommand, Key) -> ParseStep` in `editor.rs`.
  Cover the **entire implemented multi-key grammar for normal AND visual** (the same
  `handle_normal` dispatch already serves both): counts (`count`/`op_count`),
  `g`-family (`gg`/`g*`/`g#`), operators `d`/`c`/`y` (normal: + motion; visual: act on
  selection), doubled `dd`/`cc`/`yy`, find-char `f`/`t`/`F`/`T` + `;`/`,`, `r{char}`,
  `i`/`a` text objects (object-prefix in visual / op-pending), and the search-operator
  (`d/`, `c?`, …) hand-off to command-line mode. Mirror today's `mode.is_visual()`
  branches exactly.
- Replace the 8 scattered pending fields with a single `PendingCommand` on `Editor`
  (keep `last_find` — it is cross-command *memory*, not pending state).
- Re-route `handle_normal` through `parse_step` → `execute`, where `execute`
  delegates to the **existing** helpers (`apply_resolved_motion`, `begin_operator`,
  `replace_char`, `text_object_range`/`apply_text_object`, search entry). The
  *effects* are unchanged; what is genuinely new is **lifting the grammar out of**
  `resolve_motion`/`find_motion`/`text_object_range`, which currently fuse "is this a
  command?" with the buffer math. Concretely: `parse_step` emits a typed
  `ResolvedCommand` (with `Motion`, object-kind, and find-target sub-enums), and
  `resolve_motion`/`text_object_range` are refactored to **`match` on those enums**
  rather than on raw `Key`s — so the buffer math stays put but the alphabet lives in
  exactly one place and the compiler enforces parse↔execute agreement (the
  anti-drift mechanism the *Design* section calls load-bearing). Treat this — not the
  field consolidation — as the real work and risk of Phase 1.

**No new behavior.** The proof is the existing suite: **every** test in
`crates/bemtvi-server/tests/editing.rs` (and the rest of the workspace) stays green,
unchanged. If the phase proves too large for one window, split operators into a
follow-up via a strangler step (`parse_step` returns a `Fallthrough` for
not-yet-migrated arms; `handle_normal` keeps the old path for those), then remove the
fallback once total — but the **preferred** path is one clean extraction.

**Done when:** the full workspace suite passes with zero behavior change; the 8
pending fields are gone; `parse_step` is the sole normal-mode decision point.

---

### Phase 2 — Expose `command_status`, unify the matcher, ship the fix — ✅ implemented

> **Landed (commit `feat(keymap): Phase 2`).** `pub enum CommandStatus` +
> `pub fn command_status(mode, &[Key])` (a fold over `parse_step`, reset-on-
> `Complete`) are exported from the crate root. The matcher calls
> `bemtvi_core::command_status` **directly** (no injected closure — D1 holds because
> the call is pure). The break-path release rule is implemented as pinned in
> *Matcher integration → Made precise*: `resolve_buffered` returns the raw-replayed
> run (the raw-replay gate), and `feed_key` releases the trailing key to the editor
> when it would only re-withhold *and* `raw_run + key` is `Complete` (a `would_hold`
> guard keeps a key that completes/breaks a mapping on the normal path). Tests
> written failing-first and confirmed red on pre-fix `main`; the `"gg0"` crutch and
> the `idle_flush_completes_a_withheld_prefix` lag-assertion were updated as the
> plan required.

**Prereq:** Phase 1 (the parser is total).

**Do (test-first — bug-fix workflow):**
- Add `pub enum CommandStatus` + `pub fn command_status(mode, keys) -> CommandStatus`
  as a thin fold over `parse_step`; export from the crate root.
- Give the matcher access to `command_status`. **[refined] Simpler than the original
  plan:** `command_status` is a *pure free function* and `keymap.rs` already depends
  on `bemtvi_core` (it imports `parse_keys`, `Key`, `Mode`), so the matcher can call
  `bemtvi_core::command_status(mode, &keys)` **directly** — no `&dyn Fn`/trait
  threaded through `feed`/`flush`/`feed_key`/`resolve_buffered`. A pure call touches
  no editor state, so D1 ("the engine never touches the editor") is intact; the
  injection the original sketched buys nothing here (there are no unit tests to mock
  it for, by rule). Inject a closure only if a real seam later wants it.
- Implement the break-path release rule **exactly as pinned down in *Matcher
  integration → Made precise*** (the released-run slice, the raw-replay gate); apply
  the same oracle check in `flush`'s `None`-prefix arm. Matcher stays mutation-free.

**Tests (write failing first, confirm red on current `main`, then green):**
- `gh` mapped → `feed(&rpc, "gg")` with **no** trailing key / **no** flush → cursor
  at line 1 immediately. *(This is the canonical failing test — confirm it fails
  before the fix.)*
- `ggh` with `gh` mapped → top, then column moves left; assert `gh` did **not** fire.
- A family sweep proving the unification is total: map `dh` → `dd`/`dw` instant; map
  `fh`+`ff` → `f{char}` + `;`/`,` instant; map `rx` → `r{char}` instant; map a
  colliding object prefix → `diw`/`ciw` instant.
- **Visual-mode** coverage proving mode-awareness: with `gh` mapped, `v`-select then
  `gg` extends to the top instantly; visual `iw`/`a(` under a colliding object map
  resolve instantly. (A normal-vs-visual pair on the same LHS, e.g. `gh`, confirms
  the mode-conditioned grammar.)
- **Update** `unmapped_prefix_sequence_reaches_the_editor` (`keymaps.rs:246`) to drop
  the `"gg0"` crutch — `"gg"` alone must reach the editor.
- **[refined] Update `idle_flush_completes_a_withheld_prefix` (`keymaps.rs:626`) —
  the original plan missed this and it will go red.** It currently asserts the cursor
  is *still on line 3 after `feed("gg")`* ("go-to-top hasn't fired") and only moves
  to line 1 after the flush — i.e. it encodes the exact lag this work deletes. Two
  moves, do both: (1) the unambiguous `gg`-instant assertion moves to the canonical
  new test above (no flush); (2) repurpose this test so its withheld prefix is one
  that is *genuinely still held* under the fix — map **`ggh`** (so `gg` is a live
  prefix of a longer **mapping**, not a broken one), type `gg`, assert no movement
  (held), then `flush` and assert go-to-top fires via the raw replay. That keeps the
  test's name and intent ("the flush completes a withheld prefix") honest against the
  new disambiguation instead of contradicting it.

**Done when:** every built-in multi-key command fires instantly under a colliding
user prefix; no flush needed for the unambiguous cases; suite green.

---

### Phase 3 — Edge audits, idle-flush consistency, docs — ✅ implemented

> **Landed (commit `docs(keymap): Phase 3`).** Edge-audit tests added: remap RHS
> resolving to a built-in is instant, visual-line `V`+`gg`, count+selection
> (`v3gg`), the search-operator hand-off (`d/world`), and the inverse (a full
> mapped prefix `ggh` fires the map, not the built-in). **No engine change was
> needed for oracle uniformity** — the single oracle in `feed_key`'s break path
> already covers every release path, because both remap RHS re-feeds (`fire`) and
> flush re-feeds (`resolve_buffered`'s fire-arm) route back through `feed_key`; the
> remap test confirms it. Docs updated (the keymap-design "Idle-flush landed"
> blockquote, `architecture.md`, the `keymap-trailing-prefix-lag` memory note +
> `MEMORY.md`); `examples/keymap-builtin/` added and verified end-to-end; the stale
> section 1 of `examples/phase4-config` corrected.

**Prereq:** Phase 2.

**Do:**
- Deeper **visual-mode** edge cases beyond Phase 2's coverage (visual-line `V`,
  operator-on-selection vs. pending text object, count + selection), and the
  **search-operator** (`d/`) hand-off to command-line mode under colliding maps.
- Ensure the oracle is consulted **uniformly** on every release path —
  `resolve_buffered`, `longest_complete`, the `flush` loop, and **remap** RHS
  re-feeding — so a remapped sequence that resolves to a built-in is also instant.
- Confirm a genuinely-ambiguous *mapped* prefix (`ggh` mapped) still resolves via the
  `timeoutlen` idle flush (shorter-map-wins), unchanged.
- **Docs.** **[refined] The substantive target is
  [2026-06-04-keymap-design.md](2026-06-04-keymap-design.md), the "Idle-flush landed"
  blockquote (lines ~495-506)**, which today claims the flush *"closes the D4 gap:
  `gg` (with `gh` mapped) jumps to the top on idle without a following key."* Revise
  it: `gg`-class sequences are now **instant** via the shared grammar; the idle flush
  is relegated to truly-ambiguous *mapped* prefixes (`j`/`jk`) and lone-prefix
  release. Likewise the worked example near line ~307 ("the `gg`-replay test sends a
  final motion to flush"). `architecture.md` needs only a light touch — it has **no**
  disambiguation narrative to amend; it still lists `vim.keymap` under *Not yet
  implemented* (line ~497), which is itself stale, so at most move that out and add
  one sentence. Update the `keymap-trailing-prefix-lag` memory note **and its
  one-line entry in `MEMORY.md`** (both currently say the lag is "FIXED (Phase 4)"
  via the flush — the new truth is "instant via unified disambiguation; flush only
  for ambiguous mapped prefixes").
- **Example config:** per project convention, add `examples/keymap-builtin/`
  demonstrating a `g`-prefix map with instant `gg`, verified end-to-end.

**Done when:** no built-in multi-key command lags behind any colliding user prefix in
any mode; docs and the memory note reflect the unified disambiguation; suite green.

---

## Risk & rollback

- **Phase 1 is the risk surface** (it touches the editor's hot path). It is
  *behavior-preserving by construction* and gated entirely by the existing black-box
  suite — if a test moves, the refactor is wrong. No new behavior lands until Phase 2,
  so Phase 1 is safely revertable on its own.
- Phases 2–3 only *add* the new release rule on the break/flush paths; runs that are
  not complete built-in commands behave exactly as before, bounding the blast radius.
