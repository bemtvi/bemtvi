# Keymap (`vim.keymap` / `:map`) — design & phased implementation plan

**Date:** 2026-06-04
**Status:** **Phases 1–3 implemented** on `main` (the matcher + normal-mode
`vim.keymap.set`, global; recursive `remap`, `<leader>`, mode-lists, and Visual
maps; insert/command-mode maps, buffer-local maps, `vim.keymap.del`, and the
low-level `nvim_set_keymap`/`nvim_buf_set_keymap`/`nvim_del_keymap`/`nvim_buf_del_keymap`
family); Phase 4 and the backport remain planned (`omap` deferred — see Phase 2).
Foundation work for **`main`**, deliberately independent of — and unaware of — the
LSP feature. **Implemented on `main` first** (it has no LSP
dependency), then **backported to `feature/lsp-integration`**, where it subsumes
that branch's hand-rolled `lsp_keymap` / `lsp_pending_g` recognizer and delivers the
LSP plan's promise that `gd`/`K`/… are rebindable. The LSP design doc
(`2026-06-02-lsp-support-design.md`, its Phase 7b — `vim.lsp.buf.*` bound via
`vim.keymap.set('n','gd',vim.lsp.buf.definition)` and `on_attach` buffer-local maps)
**lives on the `feature/lsp-integration` branch**, not on `main`. The
[*Backporting to `feature/lsp-integration`*](#backporting-to-featurelsp-integration)
section at the end is the integration contract that keeps this engine a clean
drop-in there.

This document is both the design for nxvim's key-mapping system **and** a
phase-by-phase implementation plan. Each phase is written to be handed off to a
fresh context window: prerequisites, the exact files it touches, the surface it
adds, the tests that prove it, and a hard "done when" gate. Read the *Design* half
first, then execute the phases in order — later phases assume earlier foundations.

The closest existing subsystems, and the templates for this work, are the
**autocmd lifecycle** ([2026-06-04-autocmd-lifecycle-design.md](2026-06-04-autocmd-lifecycle-design.md))
and **user commands**: a pure-Lua registry (`vim._autocmds` / `vim._user_commands`)
the server reads back, with callbacks invoked from Rust (`run_user_command` /
`run_panel_select`) whose effects drain through `apply_lua_effects`. Keymaps add
**two** twists those don't have: (a) the **LHS is matched against the live input
stream**, mid-keystroke — the interesting part; and (b) unlike autocmds, where
*matching* happens in Lua (`vim._fire`), keymap matching happens **in Rust** (the
trie lives in the server, design §1/D1), so the server must read the registry **as
data**, not just call into it.

---

## Goal

Make nxvim's keys user-mappable from Lua, the way real configs drive them:

```lua
vim.keymap.set('n', 'gd', vim.lsp.buf.definition, { buffer = bufnr })
vim.keymap.set('n', '<leader>w', '<cmd>write<cr>')
vim.keymap.set({ 'n', 'v' }, '<leader>y', '"+y')
vim.keymap.set('i', 'jk', '<Esc>')
```

while preserving nxvim's two non-negotiables:

1. **`nxvim-core` stays pure and synchronous.** No Lua, no user-mapping table, no
   callback types leak into core. The key state machine (`Editor::input(Key)`) is
   untouched; the mapping layer sits **in front of it**, in the server.
2. **One input path.** Every key still arrives as `nvim_input` notation, is parsed
   by `parse_keys`, and ends at `Editor::input`. Mappings interpose; they don't
   fork the path.

The compatibility target is the **Lua `vim.keymap.set` / `vim.keymap.del`** surface
(and the lower-level `nvim_set_keymap` / `nvim_buf_set_keymap` / `nvim_del_keymap`
it normalizes onto). Vimscript `:map`-family ex-commands are a late, optional phase;
legacy Vimscript configs are otherwise a non-goal (as in the LSP plan).

> **Why this is the same engine the LSP branch needs.** On
> `feature/lsp-integration`, `Server::input` opens each key with
> `if self.lsp_keymap(key) { continue; }` — a hand-rolled, single-key
> withhold/replay recognizer for `gd`/`gD`/`gr`/`K` plus the insert-mode completion
> triggers. That is exactly a *special case* of an arbitrary multi-key mapping
> table. This plan builds the general engine on `main`; the backport points the LSP
> keys at it as **default mappings** and deletes the bespoke recognizer. See the
> Backport section.

---

## How input works today (on `main`)

- **Transport.** A client sends `nvim_input("<notation>")`; the server's
  `nvim_input` arm calls `Server::input(keys)` (`crates/nxvim-server/src/lib.rs`).
- **Parse.** `Server::input` runs `parse_keys(keys)` (`crates/nxvim-core/src/input.rs`)
  → a `Vec<Key>`. `Key { code: KeyCode, ctrl, alt, shift }`; `parse_keys` understands
  literal chars and `<C-..>`/`<Esc>`/`<CR>`/… notation.
- **Per-key loop — *no interception today*.** The whole of `Server::input` is:

  ```rust
  fn input(&mut self, keys: &str) {
      for key in parse_keys(keys) {
          self.editor.input(key);
          // Per *key*, not per message: a batched `o…<Esc>` must still see the
          // transition into insert on the `o`.
          self.emit_lifecycle_events();
      }
      self.run_pending();
  }
  ```

  Every parsed key goes straight to `self.editor.input(key)`; `emit_lifecycle_events()`
  fires the buffer/mode autocmds the key implied (per-key, so a batched `o…<Esc>`
  still fires `InsertEnter` on the `o`). After the batch, `run_pending()` drives
  queued Lua/ex work to a fixpoint. **There is no mapping layer, no
  `feedkeys`/`nvim_replace_termcodes`, and nothing that withholds a key.** This plan
  adds the *first* interception layer to `main`.

> **The same loop on `feature/lsp-integration`.** There, the body is prefixed with
> `if self.lsp_keymap(key) { continue; }`, and the `Server` carries `lsp_pending_g`
> (the withheld-`g` prefix for `gd`/`gD`/`gr`) and `lsp_pending_ctrl_x` (the
> `<C-x><C-o>` prefix). `lsp_keymap`'s `g` handling is the kernel of the general
> problem: `g` is withheld, and the next key either completes a mapping (`gd`) or
> **replays** the withheld `g` and falls through (`gg`, `ge`, `dgg`). The engine in
> this plan is the **generalization of that withhold/replay to an arbitrary,
> user-populated set of multi-key LHSs across modes** — and on the backport
> `lsp_keymap` becomes its first client (its bindings move to *default mappings*).

---

## Design

### The model this plan establishes

**1. The keymap layer is server-side, in front of `Editor::input`.** Core stays
pure. The server owns a per-mode **prefix trie** of LHS → mapping, built from the
Lua registry, and a small **pending-key buffer** (the N-key withhold/replay buffer).
`Server::input`'s per-key loop consults the trie before `editor.input`. Recommended
home: a new **`crates/nxvim-server/src/keymap.rs`** module (sibling to the existing
`syntax.rs`) holding the trie, the pending buffer, and the match function — so
`Server::input` stays thin and the whole engine is isolated for the backport. The
matcher does **not** own the editor; it is a pure function of *(mode, incoming key)*
that returns an owned list of **steps** the server then executes (design §3) — which
also sidesteps borrow conflicts between the matcher state and `self.editor`.

**2. The Lua surface mirrors autocmds/user-commands.** `vim.keymap.set` stores an
entry in a pure-Lua registry `vim._keymaps`; a **function** RHS is kept in a
Lua-side table keyed by a stable id (`vim._keymap_fns[id]`), invoked from Rust by
`LuaRuntime::run_keymap(id)` (the `run_user_command` / `run_panel_select` analogue),
effects draining via `apply_lua_effects`. A **string** RHS is fed back into the input
path. Registration is pure Lua (like `nvim_create_autocmd`); the Rust surface is just
`run_keymap(id)` plus a **snapshot reader** — see point 6.

**3. Matching without an input timer (the crux).** nxvim processes keys
synchronously, in `nvim_input` batches, with **no idle timer**, so vim's
`timeoutlen` ambiguity ("wait T ms, then take the shorter map") cannot be
reproduced faithfully. The policy:

- Maintain a `pending: Vec<Key>` of keys that form a **live prefix** of at least one
  mapping in the active trie (for the current mode).
- On each key, extend `pending` and classify the sequence:
  - **No longer any prefix** → the buffered keys were not a mapping: **replay** them
    through `editor.input` in order, clear `pending`, then re-process the current key
    from scratch (it may itself start a new mapping). This is the withhold/replay
    pattern — which the LSP branch's `lsp_pending_g` implements ad-hoc for a single
    key; here it is general.
  - **A complete mapping and not a prefix of any longer one** → fire it (consume).
  - **A complete mapping *and* a prefix of a longer one** (ambiguous, e.g. `j` &
    `jk`) → keep buffering; if the next key continues to the longer map, fire that;
    if it doesn't, fire the **shorter** map, then re-process the next key. Within one
    `nvim_input` batch this is deterministic. A dangling prefix at the **batch
    boundary** is held in `pending` for the next batch.

  The single divergence from neovim — no real-time `timeoutlen`, so an ambiguous map
  resolves on the *next key* rather than after a timeout — is a documented gap
  (Phase 4 revisits a synthetic flush). For the mappings real configs and the LSP
  keys use (`gd`, `<leader>…`, `jk`), it is invisible.

  Concretely, the matcher's `feed(mode, key)` returns an owned `Vec<Step>`:
  `Step::Editor(Key)` (the server runs `editor.input` + `emit_lifecycle_events`) and
  `Step::Fire(MappingRhs)` (the server executes the RHS). A withhold returns an empty
  list; a replay returns the buffered keys as `Editor` steps followed by re-feeding
  the current key. All buffering/ambiguity state lives inside the matcher.

**4. `noremap` vs remap, and feeding.** A **function** RHS is just called. A
**string** RHS is parsed (`parse_keys`) and fed key-by-key:
- **noremap** (the `vim.keymap.set` default): fed straight to `editor.input`,
  bypassing the trie (no re-mapping). In step terms, the server emits `Step::Editor`
  for each RHS key.
- **remap** (`:map` default; `vim.keymap.set` with `remap = true`): fed back through
  the keymap layer (re-entrant `feed`), so RHS keys can themselves trigger mappings —
  bounded by a `maxmapdepth` recursion cap (vim's is 1000; a small cap suffices) to
  stop a self-referential map from looping. (Phase 2.)

**5. LHS/RHS normalization via `parse_keys`.** The LHS notation is normalized to a
`Vec<Key>` (the trie's key path), and a string RHS to a `Vec<Key>`, both via core's
existing `parse_keys` — so `<C-w>`, `<Esc>`, `<leader>`-expanded sequences, and
literal chars all canonicalize one way. `<leader>` is expanded from
`vim.g.mapleader` (default `\`) **at set-time**, before `parse_keys`, matching
neovim. No reverse (Key → notation) is needed: the trie is keyed by `Key`. This
requires one small **`nxvim-core` change**: derive `Hash` on `Key` and `KeyCode`
(they are `Eq` today but not `Hash`) so the trie can key children by `Key` in a
`HashMap`. That is the *only* core change the whole plan needs.

*Caveat — `<cmd>…<cr>` does not round-trip through `parse_keys`.* `parse_keys` only
knows single-char `<…>` names plus its fixed table (`<C-..>`, `<Esc>`, `<CR>`,
`<Space>`, …); a multi-char name like `<cmd>` falls through to the **literal** chars
`<`,`c`,`m`,`d`,`>`. So a **string** RHS such as `'<cmd>write<cr>'` (used in the Goal
example and the Phase 2 `<leader>w` test) can't simply be `parse_keys`-fed. When the
string-RHS feed lands (Phase 2), either special-case a leading `<cmd>…<cr>` and run
the inner text as an ex-command (like `vim.cmd` — neovim's `<Cmd>` semantics, no mode
change), or write examples/tests with the `:write<cr>` colon form (which parses
cleanly to `:`,`w`,`r`,`i`,`t`,`e`,Enter). The LSP **default** maps added on the
backport sidestep this entirely: they are **native** actions (point D7) that call
`request_lsp(kind)` directly rather than feeding keys.

**6. Reading the registry (the one structural difference from autocmds).** Autocmds
keep *matching* in Lua: Rust calls `vim._fire(event,…)` and Lua walks `vim._autocmds`.
Keymaps match **in Rust** (the trie), so `LuaRuntime` gains a **snapshot reader**,
`keymaps_snapshot() -> Vec<RawKeymap>` plus `keymaps_version() -> u64`, that pulls
`vim._keymaps` across the bridge as plain data:
`RawKeymap { modes: Vec<String>, lhs: String, rhs: RawRhs, noremap, buffer: Option<u64>, desc, default }`,
`RawRhs = Lua(fn_id) | Str(String)`. The server compiles this snapshot into per-mode
tries of `MappingRhs`. It checks `keymaps_version()` **once per `nvim_input` batch**
(not per key) and rebuilds only when it advanced — so per keystroke the server only
walks the cached trie. (A mapping whose function calls `vim.keymap.set` mid-batch
therefore takes effect on the *next* batch — an acceptable ordering, noted.)

### Key decisions

- **D1 — Server-side engine, core untouched.** Mappings invoke Lua and re-feed input;
  both are server concerns. Putting the whole layer in the server (a new `keymap.rs`)
  keeps `nxvim-core` pure (modulo the one `Hash` derive) and gives one place to
  reason about match ordering. `Editor::input(Key)` is unchanged.
- **D2 — One withhold/replay matcher, general from the start.** The pending-key
  buffer is built general (N-key, multi-mode, user-populated) rather than as a
  one-off. On the backport it **replaces** the bespoke `lsp_pending_g` /
  `lsp_pending_ctrl_x` recognizers; the LSP keys become ordinary default mappings
  (D6/D7). One matcher, not two.
- **D3 — Registry mirrors autocmds; rebuild-on-version, not per-key.** `vim._keymaps`
  is read into a cached trie via the snapshot reader (point 6); a version counter
  invalidates it on `set`/`del`, checked once per batch. Per keystroke the server
  only walks the trie.
- **D4 — Resolve ambiguity on the next key (no timer).** Documented divergence from
  `timeoutlen`; faithful for all non-ambiguous and batch-internal cases. Phase 4 may
  add a synthetic "flush pending" input the client can send on idle.
- **D5 — Default `noremap` depends on the entry point.** `vim.keymap.set` →
  `noremap=true`; `nvim_set_keymap`/`:map` → remapping. The engine takes a normalized
  `noremap` bool; the surfaces set the right default.
- **D6 — Built-in keymaps are overridable *defaults* (mechanism in `main`, populated
  on the backport).** The registry/trie carries a precedence ladder:
  **buffer-local > global**, and within a scope **user (non-default) > default**, and
  among the same kind **last-set wins**. A `default: bool` flag on an entry selects
  the default rung. `main` ships this ladder whole and exercises the *buffer-local >
  global* rung (Phase 3) and *last-set wins* (Phase 1); it installs **no** built-in
  defaults, so the *user > default* rung is structurally present but first **exercised
  on the backport**, where the four LSP maps land as defaults and a user
  `vim.keymap.set('n','gd',…)` shadows them. (Keeping the `default` rung in the
  comparison from day one is what makes the backport a data change, not an engine
  change — no dead code, since the flag is read by every precedence sort.)
- **D7 — RHS is an enum from day one; the backport adds one variant.** The fired
  mapping's RHS is `MappingRhs`, dispatched by a `match` when a map fires:
  - `main`: `MappingRhs::Lua(fn_id)` and `MappingRhs::Keys(Vec<Key>, noremap)`.
  - **backport adds** `MappingRhs::Native(BuiltinAction)` with
    `BuiltinAction::Lsp(LspReqKind)`, and one match arm that calls
    `self.request_lsp(kind)`. Because the *fire* dispatch is already a `match` over an
    enum, this is a localized addition — not an engine change, and it leaves no
    dead code in `main` (the variant simply doesn't exist there yet). This decision is
    the load-bearing one for a clean backport: the LSP keys are neither Lua functions
    nor string feeds, so without a native-action variant the backport would have to
    retrofit the RHS type.

### Files (touched across phases)

- `crates/nxvim-lua/src/prelude.lua` — `vim.keymap.set`/`del`, the `vim._keymaps`
  registry + `vim._keymap_fns` + `vim._run_keymap(id)` + `vim._keymaps_version`,
  `nvim_set_keymap`/`nvim_buf_set_keymap`/`nvim_del_keymap`, `<leader>` expansion,
  mode normalization. (Pure-Lua, like the autocmd registration helpers.)
- `crates/nxvim-lua/src/lib.rs` — `LuaRuntime::run_keymap(id)` (the
  `run_user_command` / `run_panel_select` analogue), and the **snapshot reader**
  `keymaps_snapshot()` + `keymaps_version()` (point 6 — new, because matching is
  Rust-side).
- `crates/nxvim-server/src/keymap.rs` *(new)* — the trie + `pending` matcher and its
  `feed(mode, key) -> Vec<Step>` surface; `MappingRhs`, `Step`, the per-mode trie
  build from a snapshot, and the precedence ladder (D6).
- `crates/nxvim-server/src/lib.rs` — a single new `Server` field (the `Keymaps`
  engine state: cached tries + version + pending buffer); `Server::input` drives the
  matcher's steps in place of the bare `editor.input` loop; RHS execution
  (Lua-fn → `run_keymap` + `apply_lua_effects`; string → feed); the per-batch
  version check/rebuild.
- `crates/nxvim-core/src/input.rs` — **add `#[derive(Hash)]`** to `Key` and `KeyCode`
  (the only core change). `parse_keys` is reused as-is.
- Tests: `crates/nxvim-server/tests/keymaps.rs` (new) — black-box via `nvim_input`,
  asserting observable effects (a mapping that runs a `:` command, edits the buffer,
  or `print`s a marker), per the no-unit-test rule. Carries its own
  `start`/`feed`/`lines` helpers (integration files don't share a module).

---

## Phases (the handoff plan)

Each phase ends with whole-workspace `cargo fmt --all -- --check`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace` green
(default features only — never `--all-features`, per CLAUDE.md). Each is sized to
one focused context window.

---

### Phase 1 — The matcher + normal-mode `vim.keymap.set` (global) — ✅ implemented

> **Landed.** The engine is in `crates/nxvim-server/src/keymap.rs` (trie +
> withhold/replay matcher), wired through `Server::input`; the Lua surface is in
> `prelude.lua` (`vim.keymap.set`, `vim._keymaps`/`_keymap_fns`/`_keymaps_version`,
> `vim._run_keymap`) with the snapshot reader / `run_keymap` in `nxvim-lua`'s
> `lib.rs`; the one core change (`#[derive(Hash)]` on `Key`/`KeyCode`) is in
> `input.rs`. Covered by `crates/nxvim-server/tests/keymaps.rs`. **One realized
> divergence:** with no input timer, a trailing live-prefix stays buffered until
> the next key flushes it (rather than firing at a `timeoutlen` boundary), so the
> `gg`-replay test sends a final motion to flush — exactly the D4 gap Phase 4's
> idle-flush closes.

**Goal / value.** Stand up the whole engine and the headline surface on `main`: a
user (or a config) can map a normal-mode key/sequence to a **Lua function** or a
**string** RHS, globally, `noremap`. This is the MVP the LSP plan's Phase 7b will
need after the backport (`vim.keymap.set('n','gd',vim.lsp.buf.definition)`), and it
proves the multi-key withhold/replay engine that the backport reuses to retire
`lsp_pending_g`.

**Prerequisites.** None. (The autocmd lifecycle is already in `main`; not required
here.)

**Scope (in).**
- The `nxvim-core` change: `#[derive(Hash)]` on `Key`/`KeyCode` (design §5).
- `vim.keymap.set(mode, lhs, rhs, opts)` for a **string** `mode` (normal: `'n'`),
  `rhs` a **function** or **string**, `opts` honoring `noremap` (default true) and
  `desc` (stored, unused). Stored in `vim._keymaps` with a stable id; a function rhs
  in `vim._keymap_fns[id]`; bump `vim._keymaps_version`.
- `vim._run_keymap(id)` (Lua) + `LuaRuntime::run_keymap(id)` (Rust), plus the
  `keymaps_snapshot()` / `keymaps_version()` reader (design §6).
- The server-side `keymap.rs` module: the **prefix trie** (per mode; Phase 1 only
  builds the Normal trie) + the `pending` withhold/replay matcher with the
  `feed(mode, key) -> Vec<Step>` surface (design §3), the precedence ladder shell
  (D6), and `MappingRhs::{Lua, Keys}` (D7). Wire it into `Server::input` as the
  **first** interception layer (`main` has none today), rebuilt when
  `vim._keymaps_version` advances (checked once per batch).
- RHS execution: a `Lua(id)` fire calls `run_keymap(id)` then `apply_lua_effects()`
  (which folds in the callback's highlights/commands/output — its direct `vim.cmd`s
  run there via `take_commands`); any *further* deferred ex-commands converge in the
  batch's trailing `run_pending()` — exactly how the autocmd path (`fire_lifecycle`)
  drains. A `Keys(keys, noremap=true)` fire emits `Step::Editor` per key (string RHS,
  noremap; no re-mapping yet).

**Scope (out → later phases / backport):** remap/recursive RHS, `<leader>`, modes
other than normal, multi-mode lists, buffer-local, `del`, insert/command mode,
ambiguity-timer, `expr`/`<Plug>`, `:map` ex-commands. **No built-in/LSP defaults are
installed in `main`** — the `Native` RHS variant and the four LSP default maps are
added on the backport (D6/D7).

**Tests** (`crates/nxvim-server/tests/keymaps.rs`).
- A function map (`vim.keymap.set('n','<Space>x', function() vim.cmd('…') end)`) fires
  on the sequence and its effect is observable; the keys don't also reach the editor.
- A string map (`'n','Y','y$'`, noremap) yanks to end-of-line.
- A multi-key map (`'n','gh',fn`) fires on `gh`; the **prefix replay** works — an
  unmapped `gj`/`gg` still reaches the editor intact (the withheld `g` is replayed,
  so core's `gg` = go-to-top still happens). *This is the exact engine the backport
  reuses for `gd` vs `gg`.*
- **Precedence:** re-`set`ting the same `('n', lhs)` replaces the prior mapping
  (last-set-wins; the *user > default* rung is tested on the backport, which is where
  defaults first exist).

**Done when.** The above pass; `main`'s `Server::input` runs every normal-mode key
through the matcher; `nxvim-core` still has no Lua deps (only the `Hash` derive); the
three gates are green.

---

### Phase 2 — remap feeding, `<leader>`, and the visual/operator modes — ✅ implemented

> **Landed.** Recursive (`remap`) RHS, `<leader>`/`<localleader>` expansion, mode
> **lists**, and Visual-mode (`v`/`x`) maps. `opts.remap = true` (or
> `opts.noremap = false`) marks a map recursive; its RHS keys are re-fed *through*
> the matcher (`Keymaps::fire` → `feed_key`), bounded by a **shared** re-feed budget
> (`MAX_MAP_DEPTH`, vim's `maxmapdepth`) reset once per real keystroke — a shared
> budget, not a per-branch depth, so a fan-out cycle (`a`→`bb`, `b`→`a`) stays
> linear instead of exploding; at the cap the remaining keys fall through as a
> literal feed. `<leader>` is expanded from `vim.g.mapleader` (default `\`) at
> set-time in `prelude.lua` (`keymap_expand_leader`). A declared map-mode now
> fans out to the editor-mode tries it covers via `keymap::mode_buckets`
> (`'v'`/`'x'` → Visual + Visual-Line, `''` → normal + visual). The matcher also
> now re-processes the ambiguous *remainder* after firing a shorter map (Phase 1
> replayed it raw). Covered by the Phase 2 block in `tests/keymaps.rs`.
>
> **`omap` deferred (the decision this phase owed).** nxvim still has no
> operator-pending *mode* — a pending operator lives in private `editor.operator`
> while `editor.mode == Normal`, so there is no trie the matcher could select by.
> `mode_buckets("o")` therefore maps to nothing and `''` expands to normal+visual
> only (vim's n+v+o minus the unreachable `o`). The normal-trie replay path already
> preserves `d{motion}`/`dgg` (the operator is core state the replayed keys feed),
> so no fidelity is lost for the mappings real configs use; a true `omap` trie
> awaits a small pure-core accessor for the pending operator and is left to a later
> phase / on-demand.

**Goal / value.** Complete normal-family mapping fidelity: recursive (`remap`) RHS,
`<leader>` expansion, mode **lists**, and the Visual / Operator-pending modes real
configs target.

**Prerequisites.** Phase 1.

**Scope (in).**
- **remap RHS:** when `noremap=false`, feed the RHS `Vec<Key>` back **through the
  matcher** (re-entrant `feed`), bounded by a `maxmapdepth` cap; a cycle hits the cap
  and stops cleanly.
- **`<leader>`** (and `<localleader>`) expansion from `vim.g.mapleader` at set-time,
  before `parse_keys`. (`vim.g` already exists in the prelude.) If `<leader>w` →
  `<cmd>write<cr>` is wanted here, resolve the `<cmd>…<cr>` caveat (design §5):
  special-case a leading `<cmd>…<cr>` to run the inner text as an ex-command.
- **mode lists** (`{'n','v'}`) and the mapping of nxvim's `Mode`
  (`Normal/Insert/Replace/Visual/VisualLine/Command`, codes `n/i/R/v/V/c`) → mode
  char(s): `n` Normal; `v`/`x` Visual + VisualLine (nxvim has no Select mode, so both
  map there); `o` operator-pending; `''` = n+v+o. Build a trie per resolved mode; the
  matcher selects the trie by `editor.mode`.
- Operator-pending interaction. **Note (verified):** nxvim has no operator-pending
  *mode* — while an operator is pending, `editor.mode == Normal` and the pending
  operator lives in a **private** `editor.operator: Option<char>` field (no public
  accessor today). So an `o`-specific trie can't be selected by `editor.mode` alone.
  The normal-trie replay path already preserves `d{motion}`/`dgg` (the operator is
  core-side state the replayed keys feed into); a *true* `omap` trie needs a small,
  pure core addition exposing the pending operator. **Decide in this phase whether to
  add that accessor or defer `omap`.**

**Scope (out):** insert/command mode (Phase 3); buffer-local (Phase 3); the
ambiguity timer and `expr` (Phase 4).

**Tests.**
- `vim.keymap.set('n','<leader>w','<cmd>write<cr>')` with `vim.g.mapleader=' '` fires
  on `<Space>w` (or the `:write<cr>` form if `<cmd>` handling is deferred).
- A remap chain (`'n','a','b'` + `'n','b',fn`) reaches `fn` via `a`; a self-cycle
  (`'n','x','x'`, remap) terminates at the depth cap without hanging.
- A `{'n','v'}` map works in both modes; an `x`-mode map fires in Visual.

**Done when.** The above pass; gates green.

---

### Phase 3 — insert & command mode, buffer-local maps, deletion — ✅ implemented

> **Landed.** Insert/command-mode maps, **buffer-local** maps (`opts.buffer`,
> `0` = current), `vim.keymap.del`, and the low-level
> `nvim_set_keymap`/`nvim_buf_set_keymap`/`nvim_del_keymap`/`nvim_buf_del_keymap`
> family. Insert and command maps needed **no new matcher code** — the engine
> already selects a per-mode trie by `editor.mode` (`mode_key`/`mode_buckets`
> covered `'i'`/`'c'` from Phase 1), so the work was the Lua surface plus tests.
> **Buffer-local** maps are where D6's *buffer-local > global* rung first does real
> work: the server now caches the registry **snapshot** and (re)compiles the tries
> via `build_for(buffer)`, dropping buffer-local entries scoped to *other* buffers
> and letting the surviving ones overwrite globals at the same LHS. The rebuild is
> gated on `(version, current_buffer)` rather than version alone (`needs_build`),
> so a buffer switch re-scopes the maps even with no registry change; like the
> version check it is once-per-batch, so a mid-batch switch takes effect next batch.
> A startup seed (`set_buf_snapshot` before `source_init`) makes `buffer = 0`
> resolve to the real startup buffer at config-time, matching neovim (the same
> `vim._cur_buf` snapshot `nvim_create_autocmd`'s `buffer = 0` already used).
> `vim.keymap.set`/`del` and the `nvim_*` family share two pure-Lua helpers
> (`keymap_register`/`keymap_remove`); the only behavioral split is the `noremap`
> default (D5 — `set` true, the `nvim_*`/`:map` family false). `keymap_remove`
> drops only the requested modes from a matched entry (surviving if it covered
> more), and frees a function RHS only when no modes remain — so a re-sourced
> config leaves exactly one mapping and can't double-fire. Covered by the Phase 3
> block in `tests/keymaps.rs`; a runnable playground is `examples/phase3-config`.

**Goal / value.** The remaining modes and scoping: insert-mode maps (the `jk`→`<Esc>`
class), command-line maps, **buffer-local** mappings (`opts.buffer`, the `on_attach`
use case), and `vim.keymap.del`.

**Prerequisites.** Phase 2.

**Scope (in).**
- **Insert/command-mode** maps. In `main` insert maps compose with the editor
  directly (there is no completion popup to coexist with — that interplay is a
  *backport* concern, see the Backport section). The matcher selects the Insert (or
  Command) trie by `editor.mode`.
- **Buffer-local** maps: `opts.buffer` (0 = current snapshot buffer) ties an entry to
  a `BufferId`; the matcher prefers a buffer-local map over a global one for the
  current buffer (the buffer-local > global rung of D6). `nvim_buf_set_keymap`.
- `vim.keymap.del(mode, lhs, opts)` / `nvim_del_keymap` / `nvim_buf_del_keymap`;
  re-running a config (augroup-`clear` style) doesn't duplicate maps.
- `nvim_set_keymap(mode, lhs, rhs, opts)` as the low-level entry `vim.keymap.set`
  normalizes onto (single-char mode codes, `noremap` default *false* here).

**Scope (out):** ambiguity timer, `expr`, `<Plug>`, `:map` ex-commands (Phase 4).
Migrating the LSP insert triggers (`<C-x><C-o>`, `<C-Space>`, `<C-k>`) and composing
with the completion popup is **backport** work, not this phase.

**Tests.**
- `vim.keymap.set('i','jk','<Esc>')` leaves insert mode; a lone `j` still inserts.
- A buffer-local map fires only in its buffer; switching buffers drops it.
- `vim.keymap.del` stops a map firing; re-sourcing a config that re-`set`s doesn't
  double-fire.

**Done when.** The above pass; gates green.

---

### Phase 4 — fidelity & the long tail

> **Idle-flush landed (the `timeoutlen` item).** The TUI now arms a `TIMEOUT_LEN`
> (1000ms, vim's default) timer after each keystroke and, on idle, notifies
> `nxvim_input_flush`; the server turns that into `Keymaps::flush(mode)`, which
> resolves a trailing live-prefix exactly as the next-key break path would —
> firing the longest complete (ambiguous *shorter*) map, else replaying the
> withheld keys raw. This closes the D4 gap: `gg` (with `gh` mapped) jumps to the
> top on idle without a following key, and an ambiguous `j`/`jk` resolves to `j`.
> The server stays timer-free (the timer lives in the client's existing
> `select!` render loop), so tests stay deterministic — they call the flush RPC
> directly rather than waiting on wall-clock. Covered by the Phase 4 block in
> `tests/keymaps.rs`. The remaining Phase 4 items (`expr`, `<Plug>`/`<nowait>`/
> `<silent>`, `:map` ex-commands) remain pick-per-demand.

**Goal / value.** The harder corners, added as real configs demand them: an
ambiguity-resolution policy better than "next key", `expr` maps, `<Plug>`, `nowait`,
`<silent>`/`desc` surfacing, and (optionally) the Vimscript `:map`-family.

**Prerequisites.** Phase 3.

**Scope (in, pick per demand).**
- **Ambiguity / `timeoutlen`:** ✅ a synthetic "flush pending" the client sends on
  input idle (the TUI already owns a render loop), letting an ambiguous shorter map
  resolve without the next key — closing the D4 gap as far as a timer-less core
  allows. *(Implemented — see the note above.)*
- **`expr` maps:** RHS function returns the keys to feed (then fed per `noremap`).
- **`<Plug>` / `<unique>` / `<nowait>` / `<silent>`** semantics.
- **`:map`-family ex-commands** (`nnoremap`, `inoremap`, `vmap`, …) parsed in the
  server and normalized onto the same registry — interactive parity for muscle memory,
  explicitly *not* a Vimscript-config goal.

**Scope (out):** full Vimscript; `<SID>`/script-local maps; `:map`'s `<buffer>`/`<expr>`
arg parsing beyond the common forms.

**Tests.** Per item: an ambiguous `j`/`jk` pair resolving on idle-flush; an `expr` map
feeding computed keys; `:nnoremap` parity with `vim.keymap.set`.

**Done when.** The chosen items pass; gates green.

---

## Backporting to `feature/lsp-integration`

This is the integration contract: how the engine — landed on `main` — drops into the
LSP branch and **subsumes** its hand-rolled recognizers. Because of D1/D2/D6/D7 the
backport is mostly **deletion plus a one-variant addition**, not an engine change.
Cross-referenced against the actual code on that branch (`lib.rs` / `lsp.rs`).

**B1 — One call-site, then identical.** On the branch, `Server::input`'s loop body is
`if self.lsp_keymap(key) { continue; } self.editor.input(key); self.emit_lifecycle_events();`.
The merge replaces the whole body with the matcher drive `main` already uses
(`for step in self.keymaps.feed(self.editor.mode, key) { self.apply_step(step); }`).
After this, `Server::input` is **byte-identical** on both branches — the only
remaining differences are the startup default-install (B2) and the `Native` fire arm
(B3).

**B2 — The LSP keys become default mappings.** At startup the server installs four
normal-mode defaults through the registry with `default = true` (D6):
`gd`→`Definition`, `gD`→`Declaration`, `gr`→`References`, `K`→`Hover`. A user
`vim.keymap.set('n','gd',…)` shadows them via the *user > default* rung. This is
where that rung is **first exercised** (and tested — see B6); `main` shipped it
unpopulated.

**B3 — `request_lsp` rides a native RHS.** Add the `MappingRhs::Native(BuiltinAction)`
variant (D7) with `BuiltinAction::Lsp(LspReqKind)`, and the one *fire* arm:
`MappingRhs::Native(BuiltinAction::Lsp(kind)) => self.request_lsp(kind)`. `request_lsp`
and `LspReqKind` (Definition/Declaration/References/Hover/SignatureHelp/Completion/…)
already exist on the branch (`lsp.rs`); the defaults in B2 carry
`Native(Lsp(LspReqKind::Definition))` etc. No key-feeding, so the `<cmd>` caveat never
applies to them.

**B4 — `lsp_pending_g` and the normal-mode `lsp_keymap` arm are deleted.** The general
`pending` buffer's withhold/replay covers `g`/`gd`/`gg`/`ge`/`dgg` exactly (it is the
generalization of `lsp_pending_g`), so:
- delete the `lsp_pending_g` field and its three sites,
- delete `lsp_keymap`'s normal-mode block (`g`→pending, `K`→hover, the `gd/gD/gr`
  resolution).
The `gd`-vs-`gg` behavior is now the default maps from B2 plus core's `gg` reached via
replay — the same observable behavior, one matcher.

**B5 — Insert mode: popup routing stays bespoke; triggers become maps.** The
completion popup is **modal, stateful UI routing** (`completion: Option<CompletionMenu>`;
while open, keys navigate/accept/dismiss), *not* a keymap — it must stay a separate,
higher-priority insert interception. The ordering contract in insert mode becomes:
1. **if a completion popup is open** → route the key through `completion_insert_key`
   (popup navigation/accept/dismiss) first;
2. **else** → the keymap matcher (`feed`);
3. **else** → `editor.input`.

The completion **triggers**, by contrast, *are* maps: migrate `<C-Space>`→`Completion`
and `<C-k>`→`SignatureHelp` to default insert maps (`Native` actions). `<C-x><C-o>` is
a two-key sequence the general matcher now handles, so `lsp_pending_ctrl_x` can also
retire as a default map (`<C-x><C-o>`→`Completion`); if the "only when no popup is
open" guard makes a default map awkward, keeping `<C-x><C-o>` bespoke is acceptable
and should be noted in a comment. Whichever way, the popup-open guard (step 1) is the
load-bearing rule, since a trigger should not fire while the menu is already steering
keys.

**B6 — Tests added on the branch.** Port `main`'s `keymaps.rs` as-is (it has no LSP
deps), and add: `gd`/`gD`/`gr`/`K` still issue their LSP requests via the defaults;
a user `vim.keymap.set('n','gd',fn)` shadows the default (the *user > default* rung);
`<C-Space>`/`<C-x><C-o>` still trigger completion; the popup-open guard (a mapped
insert key does not fire while the menu is open).

**Net diff on the branch:** one `Server::input` body swap (to the shared form), one
enum variant + one match arm, one startup install of four defaults, the insert
ordering guard, and the **deletion** of `lsp_keymap`'s normal arm + `lsp_pending_g`
(+ optionally `lsp_pending_ctrl_x`). No change to the matcher, the trie, the registry,
or the Lua surface.

---

## Cross-phase notes & follow-ups (not scheduled)

- **`timeoutlen` is not real-time.** Without a core input timer, ambiguous maps
  resolve on the next key (or, post-Phase-4, an idle flush), never a wall-clock
  timeout. Asserting wall-clock timing is out of scope, matching the coverage
  boundary the syntax/LSP designs set.
- **`feedkeys` / `nvim_feedkeys` / `nvim_replace_termcodes`** — a general
  programmatic-feed API; the remap engine (Phase 2) builds the internal feed path it
  would expose. Add when a plugin needs it.
- **Recorded macros (`q`/`@`)** — a separate feed source that would reuse the same
  input path; out of scope here.
- **`which-key`-style ambiguity UI** — surfacing pending-prefix hints in the panel; a
  UI follow-up once `pending` exists.

## Compared to neovim

- **Server-side matcher, core stays pure** — neovim resolves maps inside its input
  loop next to the editor; nxvim keeps the map trie in the server (a new `keymap.rs`)
  so `nxvim-core`'s key state machine never learns about user mappings (the same split
  as the autocmd emission). The one concession is a `Hash` derive on `Key`/`KeyCode`.
- **No real `timeoutlen`** — the single deliberate divergence (design §3 / D4); every
  non-ambiguous and within-batch case is faithful.
- **`vim.keymap.set` first, `:map` last** — the Lua surface modern configs use is the
  priority; Vimscript `:map` parity is an optional tail, mirroring how the LSP plan
  put `vim.lsp.*` first and Vimscript configs out of scope.
- **Built-in keys are defaults, not hard-coding** — on the backport the LSP keys (and
  any future built-ins) ride the same registry and are user-overridable (D6/D7),
  replacing the bespoke `lsp_keymap` recognizer rather than re-implementing it.
