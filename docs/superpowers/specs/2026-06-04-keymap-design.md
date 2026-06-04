# Keymap (`vim.keymap` / `:map`) — design & phased implementation plan

**Date:** 2026-06-04
**Status:** Planned. Foundation work, independent of any feature branch. Unblocks
the LSP plan's [Phase 7b](2026-06-02-lsp-support-design.md) (`vim.lsp.buf.*` bound
via `vim.keymap.set('n','gd',vim.lsp.buf.definition)` and `on_attach` buffer-local
maps) and retires the built-in, hard-coded LSP key recognizer (`lsp_keymap`).

This document is both the design for nxvim's key-mapping system **and** a
phase-by-phase implementation plan. Each phase is written to be handed off to a
fresh context window: prerequisites, the exact files it touches, the surface it
adds, the tests that prove it, and a hard "done when" gate. Read the *Design* half
first, then execute the phases in order — later phases assume earlier foundations.

The closest existing subsystems, and the templates for this work, are the
**autocmd lifecycle** ([2026-06-04-autocmd-lifecycle-design.md](2026-06-04-autocmd-lifecycle-design.md))
and **user commands**: a Lua-side registry (`vim._autocmds` / `vim._user_commands`)
the server reads back, with callbacks invoked from Rust (`run_user_command` /
`run_panel_select`) whose effects drain through `apply_lua_effects`. Keymaps add
one twist those don't have: **the LHS is matched against the live input stream**,
mid-keystroke, which is the interesting part.

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
   untouched; the mapping layer sits **in front of it**, in the server — exactly
   where the built-in `lsp_keymap` recognizer already sits.
2. **One input path.** Every key still arrives as `nvim_input` notation, is parsed
   by `parse_keys`, and ends at `Editor::input`. Mappings interpose; they don't
   fork the path.

The compatibility target is the **Lua `vim.keymap.set` / `vim.keymap.del`** surface
(and the lower-level `nvim_set_keymap` / `nvim_buf_set_keymap` / `nvim_del_keymap`
it normalizes onto). Vimscript `:map`-family ex-commands are a late, optional phase;
legacy Vimscript configs are otherwise a non-goal (as in the LSP plan).

---

## How input works today

- **Transport.** A client sends `nvim_input("<notation>")`; the server's
  `nvim_input` arm calls `Server::input(keys)` (`crates/nxvim-server/src/lib.rs`).
- **Parse.** `Server::input` runs `parse_keys(keys)` (`crates/nxvim-core/src/input.rs`)
  → a `Vec<Key>`. `Key { code: KeyCode, ctrl, alt, shift }`; `parse_keys` understands
  literal chars and `<C-..>`/`<Esc>`/`<CR>`/… notation.
- **Per-key loop.** For each `Key`: `self.lsp_keymap(key)` gets first refusal — if it
  consumes the key (returns `true`) the editor never sees it; otherwise
  `self.editor.input(key)` applies it, and `emit_lifecycle_events()` fires the
  buffer/mode autocmds the key implied (per-key, so a batched `o…<Esc>` still fires
  `InsertEnter` on the `o`). After the batch, `run_pending()` drives queued Lua/ex
  work to a fixpoint.
- **The only interception today is `lsp_keymap`** — a **hand-rolled, single-key
  prefix matcher**: `gd`/`gD`/`gr`/`K` (normal), the completion triggers and `<C-k>`
  (insert). Its `g` handling is the kernel of the general problem: `g` is withheld
  (`lsp_pending_g`), and the next key either completes a mapping (`gd`) or **replays**
  the withheld `g` and falls through (`gg`, `ge`, `dgg`). There is **no** other
  user-facing mapping mechanism, and no `feedkeys`/`nvim_replace_termcodes`.

So the engine this plan builds is the **generalization of `lsp_pending_g`** to an
arbitrary, user-populated set of multi-key LHSs across modes — and `lsp_keymap`
becomes its first client (its bindings move to *default mappings*).

---

## Design

### The model this plan establishes

**1. The keymap layer is server-side, in front of `Editor::input`.** Core stays
pure. The server owns a per-mode **prefix trie** of LHS → mapping, built from the
Lua registry, and a small **pending-key buffer** (the N-key generalization of
`lsp_pending_g`). `Server::input`'s per-key loop consults the trie before
`editor.input`; the existing `lsp_keymap` call is subsumed by it.

**2. The Lua surface mirrors autocmds/user-commands.** `vim.keymap.set` stores an
entry in a pure-Lua registry `vim._keymaps`; a **function** RHS is kept in a
Lua-side table keyed by a stable id (`vim._keymap_fns[id]`), invoked from Rust by
`LuaRuntime::run_keymap(id)` (the `run_user_command` analogue), effects draining via
`apply_lua_effects`. A **string** RHS is fed back into the input path. The server
caches the registry as a trie and rebuilds it when a registry **version** counter
(bumped by `set`/`del`) advances — never per keystroke.

**3. Matching without an input timer (the crux).** nxvim processes keys
synchronously, in `nvim_input` batches, with **no idle timer**, so vim's
`timeoutlen` ambiguity ("wait T ms, then take the shorter map") cannot be
reproduced faithfully. The policy:

- Maintain a `pending: Vec<Key>` of keys that form a **live prefix** of at least one
  mapping in the active trie (for the current mode).
- On each key, extend `pending` and classify the sequence:
  - **No longer any prefix** → the buffered keys were not a mapping: **replay** them
    through `editor.input` in order, clear `pending`, then re-process the current key
    from scratch (it may itself start a new mapping). This is `lsp_pending_g`'s
    replay path, generalized.
  - **A complete mapping and not a prefix of any longer one** → fire it (consume).
  - **A complete mapping *and* a prefix of a longer one** (ambiguous, e.g. `j` &
    `jk`) → keep buffering; if the next key continues to the longer map, fire that;
    if it doesn't, fire the **shorter** map, then re-process the next key. Within one
    `nvim_input` batch this is deterministic. A dangling prefix at the **batch
    boundary** is held in `pending` for the next batch (exactly as `lsp_pending_g`
    persists across keys today).

  The single divergence from neovim — no real-time `timeoutlen`, so an ambiguous map
  resolves on the *next key* rather than after a timeout — is a documented gap
  (Phase 4 revisits a synthetic flush). For the mappings real configs and the LSP
  keys use (`gd`, `<leader>…`, `jk`), it is invisible.

**4. `noremap` vs remap, and feeding.** A **function** RHS is just called. A
**string** RHS is parsed (`parse_keys`) and fed key-by-key:
- **noremap** (the `vim.keymap.set` default): fed straight to `editor.input`,
  bypassing the trie (no re-mapping).
- **remap** (`:map` default; `vim.keymap.set` with `remap = true`): fed back through
  the keymap layer, so RHS keys can themselves trigger mappings — bounded by a
  `maxmapdepth` recursion cap (vim's is 1000; a small cap suffices) to stop a
  self-referential map from looping.

**5. LHS/RHS normalization via `parse_keys`.** The LHS notation is normalized to a
`Vec<Key>` (the trie's key path), and a string RHS to a `Vec<Key>`, both via core's
existing `parse_keys` — so `<C-w>`, `<Esc>`, `<leader>`-expanded sequences, and
literal chars all canonicalize one way. `<leader>` is expanded from
`vim.g.mapleader` (default `\`) **at set-time**, before `parse_keys`, matching
neovim. No reverse (Key → notation) is needed: the trie is keyed by `Key`.

### Key decisions

- **D1 — Server-side engine, core untouched.** Mappings invoke Lua and re-feed input;
  both are server concerns. Putting the whole layer in the server (where `lsp_keymap`
  already lives) keeps `nxvim-core` pure and gives one place to reason about match
  ordering. `Editor::input(Key)` is unchanged.
- **D2 — Generalize `lsp_pending_g`, don't special-case.** The pending-key
  withhold/replay buffer **replaces** the bespoke `lsp_pending_g`/`lsp_pending_ctrl_x`
  recognizers; the LSP keys become ordinary default mappings (D6). One matcher, not
  two.
- **D3 — Registry mirrors autocmds; rebuild-on-version, not per-key.** `vim._keymaps`
  is read into a cached trie; a version counter invalidates it on `set`/`del`. Per
  keystroke the server only walks the trie.
- **D4 — Resolve ambiguity on the next key (no timer).** Documented divergence from
  `timeoutlen`; faithful for all non-ambiguous and batch-internal cases. Phase 4 may
  add a synthetic "flush pending" input the client can send on idle.
- **D5 — Default `noremap` depends on the entry point.** `vim.keymap.set` →
  `noremap=true`; `nvim_set_keymap`/`:map` → remapping. The engine takes a normalized
  `noremap` bool; the surfaces set the right default.
- **D6 — Built-in keymaps become overridable defaults.** The server installs the LSP
  (and any future built-in) bindings through the same registry at startup, marked as
  defaults so a user `vim.keymap.set` for the same `(mode, lhs)` shadows them. This
  validates the engine on the real `g`-prefix multi-key case and delivers the LSP
  plan's promise that `gd`/`K`/… are rebindable.

### Files (touched across phases)

- `crates/nxvim-lua/src/prelude.lua` — `vim.keymap.set`/`del`, the `vim._keymaps`
  registry + `vim._keymap_fns` + `vim._run_keymap(id)`, `nvim_set_keymap`/
  `nvim_buf_set_keymap`/`nvim_del_keymap`, `<leader>` expansion, mode normalization.
- `crates/nxvim-lua/src/lib.rs` — `LuaRuntime::run_keymap(id)` (the
  `run_user_command` analogue) and a reader for the `vim._keymaps` snapshot +
  version; possibly a Rust-backed low-level `nvim_set_keymap` if a pure-Lua registry
  proves awkward.
- `crates/nxvim-server/src/lib.rs` — the trie + `pending` matcher in `Server::input`,
  subsuming `lsp_keymap`; new `Server` fields (the pending buffer, the cached trie +
  version); RHS feeding (raw vs re-mapped, depth cap); default-keymap install.
- `crates/nxvim-core/src/input.rs` — `parse_keys` reused as-is (no change expected; a
  `Key` ordering/`Hash` derive may be added for trie keys).
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

### Phase 1 — The matcher + normal-mode `vim.keymap.set` (global)

**Goal / value.** Stand up the whole engine and the headline surface: a user (or a
config) can map a normal-mode key/sequence to a **Lua function** or a **string** RHS,
globally, `noremap`. This is the MVP the LSP plan's Phase 7b needs
(`vim.keymap.set('n','gd',vim.lsp.buf.definition)`), and it retires the hand-rolled
normal-mode `lsp_keymap` path by re-expressing `gd`/`gD`/`gr`/`K` as **default
mappings** through the new engine.

**Prerequisites.** None. (The autocmd lifecycle is already in; not required here.)

**Scope (in).**
- `vim.keymap.set(mode, lhs, rhs, opts)` for a **string** `mode` (normal: `'n'`),
  `rhs` a **function** or **string**, `opts` honoring `noremap` (default true) and
  `desc` (stored, unused). Stored in `vim._keymaps` with a stable id; a function rhs
  in `vim._keymap_fns[id]`; bump `vim._keymaps_version`.
- `vim._run_keymap(id)` (Lua) + `LuaRuntime::run_keymap(id)` (Rust) — invoke a
  function rhs; effects drain via `apply_lua_effects`.
- The server-side **prefix trie** (per mode; Phase 1 only builds the Normal trie) +
  the `pending` withhold/replay matcher (design §3), rebuilt when
  `vim._keymaps_version` advances. Wire it into `Server::input` **in place of** the
  `lsp_keymap` normal-mode arm.
- **String RHS feeding (noremap):** `parse_keys(rhs)` → fed straight to
  `editor.input` (no re-mapping yet).
- **Default keymaps:** install `gd`/`gD`/`gr`/`K` as defaults routed to the existing
  `request_lsp(...)` paths; drop `lsp_pending_g`. A user `vim.keymap.set('n','gd',…)`
  overrides. (Insert-mode LSP keys stay on the existing `completion_insert_key` path
  until Phase 3.)

**Scope (out → later phases):** remap/recursive RHS, `<leader>`, modes other than
normal, multi-mode lists, buffer-local, `del`, insert/command mode, ambiguity-timer,
`expr`/`<Plug>`, `:map` ex-commands.

**Tests** (`crates/nxvim-server/tests/keymaps.rs`).
- A function map (`vim.keymap.set('n','<Space>x', function() vim.cmd('…') end)`) fires
  on the sequence and its effect is observable; the keys don't also reach the editor.
- A string map (`'n','Y','y$'`, noremap) yanks to end-of-line.
- A multi-key map (`'n','gh',fn`) fires on `gh`; the **prefix replay** works — an
  unmapped `gj`/`gg` still reaches the editor intact (the withheld `g` is replayed).
- `gd` (now a default) still issues go-to-definition; a user override of `gd` wins.

**Done when.** The above pass; `lsp_pending_g` is gone; `nxvim-core` still has no Lua
deps; the three gates are green.

---

### Phase 2 — remap feeding, `<leader>`, and the visual/operator modes

**Goal / value.** Complete normal-family mapping fidelity: recursive (`remap`) RHS,
`<leader>` expansion, mode **lists**, and the Visual / Operator-pending modes real
configs target.

**Prerequisites.** Phase 1.

**Scope (in).**
- **remap RHS:** when `noremap=false`, feed the RHS `Vec<Key>` back **through the
  trie**, bounded by a `maxmapdepth` cap; a cycle hits the cap and stops cleanly.
- **`<leader>`** (and `<localleader>`) expansion from `vim.g.mapleader` at set-time,
  before `parse_keys`.
- **mode lists** (`{'n','v'}`) and the mapping of `Mode` → mode char(s): `n` Normal,
  `v` Visual+VisualLine, `x` Visual, `o` operator-pending, `''` = n+v+o. Build a trie
  per resolved mode; the matcher selects the trie by `editor.mode`.
- Operator-pending interaction: a mapping fires while an operator is pending (e.g.
  `omap`), composing with the count/operator the core tracks.

**Scope (out):** insert/command mode (Phase 3); buffer-local (Phase 3); the
ambiguity timer and `expr` (Phase 4).

**Tests.**
- `vim.keymap.set('n','<leader>w','<cmd>write<cr>')` with `vim.g.mapleader=' '` fires
  on `<Space>w`.
- A remap chain (`'n','a','b'` + `'n','b',fn`) reaches `fn` via `a`; a self-cycle
  (`'n','x','x'`, remap) terminates at the depth cap without hanging.
- A `{'n','v'}` map works in both modes; an `x`-mode map fires in Visual.

**Done when.** The above pass; gates green.

---

### Phase 3 — insert & command mode, buffer-local maps, deletion

**Goal / value.** The remaining modes and scoping: insert-mode maps (the `jk`→`<Esc>`
class), command-line maps, **buffer-local** mappings (`opts.buffer`, the `on_attach`
use case), and `vim.keymap.del`.

**Prerequisites.** Phase 2.

**Scope (in).**
- **Insert/command-mode** maps, composed with the existing insert-mode interception:
  the completion-popup keys (`completion_insert_key`) keep priority; a user insert map
  is consulted around it. Migrate the insert-mode LSP triggers (`<C-x><C-o>`,
  `<C-Space>`, `<C-k>`) to default insert-mode mappings where they compose cleanly
  (or record why one stays bespoke).
- **Buffer-local** maps: `opts.buffer` (0 = current snapshot buffer) ties an entry to
  a `BufferId`; the matcher prefers a buffer-local map over a global one for the
  current buffer. `nvim_buf_set_keymap`.
- `vim.keymap.del(mode, lhs, opts)` / `nvim_del_keymap` / `nvim_buf_del_keymap`;
  re-running a config (augroup-`clear` style) doesn't duplicate maps.
- `nvim_set_keymap(mode, lhs, rhs, opts)` as the low-level entry `vim.keymap.set`
  normalizes onto (single-char mode codes, `noremap` default *false* here).

**Scope (out):** ambiguity timer, `expr`, `<Plug>`, `:map` ex-commands (Phase 4).

**Tests.**
- `vim.keymap.set('i','jk','<Esc>')` leaves insert mode; a lone `j` still inserts.
- A buffer-local map fires only in its buffer; switching buffers drops it.
- `vim.keymap.del` stops a map firing; re-sourcing a config that re-`set`s doesn't
  double-fire.

**Done when.** The above pass; gates green.

---

### Phase 4 — fidelity & the long tail

**Goal / value.** The harder corners, added as real configs demand them: an
ambiguity-resolution policy better than "next key", `expr` maps, `<Plug>`, `nowait`,
`<silent>`/`desc` surfacing, and (optionally) the Vimscript `:map`-family.

**Prerequisites.** Phase 3.

**Scope (in, pick per demand).**
- **Ambiguity / `timeoutlen`:** a synthetic "flush pending" the client sends on input
  idle (the TUI already owns a render loop), letting an ambiguous shorter map resolve
  without the next key — closing the D4 gap as far as a timer-less core allows.
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
  loop next to the editor; nxvim keeps the map trie in the server so `nxvim-core`'s
  key state machine never learns about user mappings (the same split as `lsp_keymap`
  and the autocmd emission).
- **No real `timeoutlen`** — the single deliberate divergence (design §3 / D4); every
  non-ambiguous and within-batch case is faithful.
- **`vim.keymap.set` first, `:map` last** — the Lua surface modern configs use is the
  priority; Vimscript `:map` parity is an optional tail, mirroring how the LSP plan
  put `vim.lsp.*` first and Vimscript configs out of scope.
- **Built-in keys are defaults, not hard-coding** — the LSP keys (and future
  built-ins) ride the same registry and are user-overridable, replacing the bespoke
  `lsp_keymap` recognizer.
