# `btv.ui.input` readline + autocomplete (and the dap REPL)

Status: Phase 1 + 2 complete
Date: 2026-06-27

## Goal

Give `btv.ui.input` first-class **readline history** and **autocomplete**, as opt-in
options every plugin can use — then dogfood both in the `bemtvi-dap` debug REPL
(history recall of past expressions; `<Tab>` completion driven by the adapter's DAP
`completions` request).

The driving request was "readline-like capabilities + autocomplete in the debug
REPL", but the right home for it is the shared primitive, not the plugin: the
`CmdlineKind::Prompt` that backs `btv.ui.input` already reuses the *full command-line
editor* (`Editor::cmdline`), which has cursor editing, `<C-r>` register insert, and —
already wired but **hard-gated off** for prompts — `<Up>`/`<Down>` history recall and
the `<Tab>` wildmenu. So this is mostly *un-gating and parameterizing* existing
machinery per-prompt.

## API (the new `btv.ui.input` opts)

```lua
btv.ui.input({
  prompt  = "dap> ",
  default = "",
  history = "bemtvi-dap-repl",      -- string namespace: enables ↑/↓ recall + records submissions
  complete = function(line, col)   -- optional; returns a candidate list OR a promise of one
    -- candidate = { label = str, insert = str?, doc = str? }  (insert defaults to label)
    return { { label = "foo", insert = "foo", doc = "..." }, ... }
  end,
  complete_docs = true,            -- side docs pane for candidate `doc` (default true when complete set)
})
```

- `history` is a **namespace string** (per the user's request — "a specific namespace
  for history"). Each namespace is an independent recall ring. Empty/absent ⇒ no
  history (today's behavior). Session-only for now (see Deferred).
- `complete` is a per-call source. Sync (returns a list) **or** async (returns a
  promise — the DAP `completions` request is a network round-trip). Renders as the
  inline **wildmenu** above the prompt line (`<Tab>`/`<S-Tab>` cycle, `<CR>` accepts) —
  the same widget `:`-completion uses.

## Design decisions (locked)

- **History scope:** session-only first (in-memory in core), persist via shada later.
- **Completion UX:** inline wildmenu (readline feel), reusing `open_cmdline_menu` — not
  the full picker overlay.

---

## Phase 1 — namespaced history on `btv.ui.input` (+ dap REPL)

Core (`bemtvi-core`):
- `mod.rs`: add `prompt_history: HashMap<String, Vec<String>>` and
  `prompt_history_key: Option<String>`.
- `cmdline.rs::open_prompt(label, default, history_key)`: store the key; reset
  `hist_idx = None`.
- `active_history()`: for `CmdlineKind::Prompt`, return the namespaced ring (or `&[]`).
- `submit_cmdline` Prompt arm: record the submitted text under the active key
  (dedup-consecutive, skip empty — mirror `remember_ex`), via a new `remember_prompt`.

Wiring:
- `ops.rs::UiInputReq`: add `history: Option<String>`.
- `install.rs::_ui_input`: accept an optional 4th arg `history`.
- `effects.rs`: `open_prompt(req.prompt, req.default, req.history)`.
- `ui.lua::btv.ui.input`: pass `opts.history` through to `btv._ui_input`.

Dogfood:
- `bemtvi-dap/repl.lua::M.prompt()`: pass `history = "bemtvi-dap-repl"`.

Tests (`crates/bemtvi-server/tests/ui_prompt.rs`): submit a value under a namespace,
re-open the prompt, `<Up>` recalls it; a different namespace does not see it; `<Up>`
with empty history is a no-op; consecutive dup collapses.

## Phase 2 — autocomplete on `btv.ui.input` (+ dap REPL via DAP `completions`) ✅

Shipped as designed below, plus `complete_docs` (the side docs pane, default on when
`complete` is set) so candidates carrying a `doc` show it beside the list — the same
pane `:`-completion uses. The completed token is the trailing identifier run before
the cursor (breaks on `.`), so member completion (`os.get` → `os.getcwd`) replaces
only the part after the dot. The dap REPL maps DAP `CompletionItem`s
(`label`/`text`/`type`/`detail`) to the wildmenu shape, with `type` heading the docs.

Refresh debounce (`complete_debounce`, default 100ms) coalesces the live-narrowing
re-queries so an async source is one round-trip per quiet window, not per keystroke;
the initial `<Tab>` stays immediate. Built first-class on `btv.utils.debounce`, keyed
off a `refresh` flag core stamps on the request (initial open vs. narrowing edit).

Adapter-specified replace ranges ARE honored: a candidate may carry `start`/`length`
(0-based char offset + char count) and the wildmenu replaces exactly that span instead
of the trailing-identifier token. Plumbed as a per-row `replace: Option<(start,end)>` on
`MenuItem` (byte offsets converted from chars against the menu-build line); preview and
accept restore the original line first so per-row ranges stay valid across navigation.
The dap source maps DAP `CompletionItem.start` (1-based, `columnsStartAt1`) → 0-based.

Core:
- `open_prompt` gains a `complete: bool` flag → `prompt_complete_active`.
- Generalize `cmdline_complete_trigger`/`_refresh`/`cmdline_replace_arg` gating from
  "Ex only" to "Ex, or Prompt when `prompt_complete_active`".
- A prompt `<Tab>` stamps `prompt_complete_request: Option<CmdlineCompleteReq>` (reuse
  the token extractor `cmdline_complete_token`, which already operates on `cmdline`).
- `open_prompt_complete_menu(candidates)`: re-extract the token from the *current*
  line (handles async staleness) and call the shared `open_cmdline_menu`.

Async source bridge (Lua-owned, since the source may await a promise):
- `ui.lua`: stash the active prompt's `complete` fn in `btv._active_prompt_complete`
  (one prompt open at a time). Cleared on resolve/cancel.
- Server drains `prompt_complete_request` → calls `lua.run_prompt_complete(line, col)`,
  which invokes the fn, resolves a list-or-promise, then queues the candidates in a
  `Shared.prompt_complete_results` vec.
- Server drains `take_prompt_complete_results()` every effects pass → for each,
  `editor.open_prompt_complete_menu(cands)`. Sync sources land same-tick; async one
  tick later.

Dogfood (`bemtvi-dap`):
- `session.lua`: add `Session:completions(text, column, frame_id, cb)` →
  `completions` request, gated on `capabilities.supportsCompletionsRequest`.
- `repl.lua::M.prompt()`: pass `complete = function(line,col) ... end` that returns a
  promise resolving the adapter's completion targets mapped to `{label, insert, doc}`.
  Map DAP `CompletionItem` (`label`/`text`/`type`/`start`/`length`) to the wildmenu
  shape; respect `start`/`length` when the adapter specifies the replaced range.

Tests: a fake-adapter session asserts `<Tab>` in the REPL prompt issues `completions`
and the returned items become an accept-able wildmenu.

## Deferred

- **History persistence** across sessions via the shada plugin-namespace store
  (1 MiB budget; native + wasm parity — see the web-shada memory). Wire
  `prompt_history` per-namespace into `PersistState` behind an opt-in.
- Completion `doc` preview pane for the prompt wildmenu (the `:`-completion docs pane
  is Ex-only today).
