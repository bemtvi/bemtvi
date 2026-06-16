# Configurable widget keys — every action rebindable, no hardcoded widget grammar

**Status:** **proposed (2026-06-16).** Make every interactive widget's keys
rebindable through the *real* keymap engine — no bespoke per-widget config table,
no hardcoded `match key.code` grammar a user can't override. The fuzzy picker
(`nx.picker`) is the named offender and the Phase 1 reference; the same mechanism
then converts the other grabbing widgets one phase at a time.

## Why

The keymap engine (`crates/nxvim-server/src/keymap.rs`) already lets a user rebind
any **editing-mode** key (`vim.keymap.set('n'|'i'|'v'|'m', …)`) via per-mode prefix
tries with a precedence ladder (buffer-local > global, user > default). But the
**grabbing widgets** — picker, `nx.ui.select`, the message/quickfix panel, the file
explorer, the command line, the confirm dialog — intercept keys in
`Editor::input` *before* the per-mode dispatch and match them with hardcoded Rust
(`handle_picker_key`, `handle_select_key`, `handle_panel`, `handle_command`, …).
None of those keys reach a trie, so none are rebindable. `<C-n>`/`<C-p>` to move the
picker selection, `<CR>` to confirm, `<Esc>` to cancel are all frozen.

The fix is the pattern multi-cursor *placement* mode already proves: a widget gets
its **own keymap mode** (its own trie bucket, like `'m'`), its operations become
**named actions**, and the built-in keys ship as **overridable default maps** in
that bucket. `vim.keymap.set('picker', '<C-j>', nx.picker.actions.next)` then works
exactly like any other map, last-set-wins. As a bonus this is **source C** of the
`KeyPending` event (active-widget key tables) — once widget keys live in the trie,
which-key shows them for free.

## The mechanism

### 1. A keymap mode per widget (the trie bucket)

`mode_buckets` / `mode_key` (keymap.rs) gain a bucket char per grabbing widget,
addressed by a readable mode-code string in Lua:

| Lua mode code | bucket | widget                         |
| ------------- | ------ | ------------------------------ |
| `"picker"`    | `'P'`  | prompted fuzzy picker          |
| `"select"`    | `'S'`  | promptless `nx.ui.select` list |
| `"panel"`     | `'L'`  | message / quickfix panel       |
| `"explorer"`  | `'E'`  | file-explorer listing          |

(Editing buckets stay `n`/`i`/`c`/`v`/`V`/`m`; the new chars don't collide.) The
command line (`"cmdline"`) already *has* a `Mode::Command` + `'c'` bucket, so it is a
later refinement, not a new bucket — its hardcoded `handle_command` is the
conversion target, not the routing.

### 2. The matcher selects the bucket by *key context*, oracle off

Core exposes `Editor::key_context() -> KeyContext` — a pure read over the existing
grab state (`menu_grabs_input()`, `panel.is_some()`, `is_explorer_buffer()`, …):
`Editing` (the buffer; today's behavior) or a specific widget. The server's
`feed_matcher` picks the scope from it:

```rust
enum MatchScope { Editing(Mode), Widget(char) }   // bucket char for Widget
```

`Editing(mode)` is today's path verbatim — bucket `mode_key(mode)`, the
`command_status` disambiguation oracle, and the literal-arg bypass all apply.
`Widget(bucket)` uses `bucket` directly and **skips the oracle and the literal-arg
bypass** (a widget has no core command grammar — a withheld prefix that breaks just
replays raw to the widget). The withhold/replay trie machinery (classify /
longest_complete / resolve_buffered / fire) is shared unchanged; only bucket
selection + the two editing-only gates differ. Until a widget is *converted* its
context stays `Editing` (legacy core grab), so unconverted widgets are untouched.

### 3. Actions are named; defaults are Lua maps; text input is the fallthrough

Each widget's operations become a core action enum dispatched by one method, e.g.
`Editor::apply_picker_action(PickerAction)`. The defaults ship as **Lua `default`
maps** in the widget's bucket (dogfooding the keymap API, no new Rust native-default
machinery):

```lua
nx.picker.actions = {
  next    = function() nx._picker_action("next") end,
  prev    = function() nx._picker_action("prev") end,
  confirm = function() nx._picker_action("confirm") end,
  cancel  = function() nx._picker_action("cancel") end,
  preview_half_down = function() nx._picker_action("preview_half_down") end,
  -- … preview_half_up / preview_page_down / preview_page_up
  backspace = function() nx._picker_action("backspace") end,
  -- … delete / left / right / home / end
}
-- registered once, lowest precedence (default = true):
nx.keymap.set("picker", "<C-n>", nx.picker.actions.next,    { default = true, desc = "Next item" })
nx.keymap.set("picker", "<CR>",  nx.picker.actions.confirm, { default = true, desc = "Confirm" })
-- … etc.
```

`nx._picker_action(name)` queues the action onto a `Shared.picker_actions` vec; the
server drains it in `run_pending` and calls `editor.apply_picker_action` — the same
queue-effect-then-drain shape every other picker op uses (`picker_pushes`,
`picker_finishes`, `picker_query_changes`). A user override is an ordinary
non-default `picker` map and wins by the precedence ladder; rebinding to a built-in
is `set('picker', key, nx.picker.actions.next)`; disabling is `set('picker', key,
'<Nop>')`.

The **only** key that can't be a map is an arbitrary printable character (you can't
enumerate them), so the residual core handler shrinks to exactly that: an unmapped
printable key inserts into the query. Everything nameable (nav / confirm / cancel /
preview / the query-edit operations) is a map. The hot path — typing a search query
— stays core-direct and fast; only the control keys round-trip Lua (in-process, the
same cost any mapped key already pays).

## Phasing — each phase independently shippable, commit + pause between

### Phase 1 — the mechanism + the picker  *(the reference)*

- keymap.rs: `MatchScope`, the `'P'` bucket, `mode_buckets("picker")`, oracle/literal
  gating in the `Widget` scope; `feed_matcher` selects the scope from `key_context()`.
- core: `KeyContext`, `Editor::key_context()`, `PickerAction` + `apply_picker_action`,
  and a slimmed picker-text fallthrough (printable → query) replacing the nav/confirm/
  cancel/preview arms of `handle_picker_key` / the picker branch of `handle_menu`.
- Lua/bridge: `nx._picker_action`, `Shared.picker_actions`, the `nx.picker.actions`
  table + the default `picker` maps (in `prelude/picker.lua`); server drain.
- Tests (`tests/picker.rs` / a new `tests/widget_keys.rs`): the default keys still
  navigate/confirm/cancel/scroll-preview; a user `set('picker', …)` rebinds and the
  default no longer fires; `<Nop>` disables; an unmapped printable still edits the
  query; a normal-mode `<C-n>` map does **not** leak into the picker.

### Phase 2 — `nx.ui.select` (the promptless list, `'S'` bucket)

Convert `handle_select_key` (`j`/`k`/`gg`/`G`/`<C-n>`/`<C-p>`/`<CR>`/`<Esc>`/`q`) to
`select` default maps + `SelectAction`. `gg` becomes a two-key default map (proving
multi-key widget maps through the same trie).

### Phase 3 — the panel (`'L'`) and explorer (`'E'`)

`handle_panel` and `handle_explorer` → `panel` / `explorer` default maps + actions.
The explorer's `:`/`/`/`?` fall-through-to-cmdline stays the unmapped-key behavior.

### Phase 4 — the command line (`"cmdline"`, the existing `'c'` bucket)

Convert `handle_command`'s history / cursor / `<C-r>` register keys to `cmdline`
default maps + actions, over the bucket `Mode::Command` already selects. The confirm
dialog (`handle_confirm`) and `:s///c` single-key answers come with it or stay as a
documented non-goal (their grammar is a fixed prompt alphabet, not a keymap).

## Non-goals

- **The modal core keys stay core.** `<Esc>` to leave insert, `i`/`a`/`o` to enter
  it, the motion/operator grammar — these are already rebindable as `n`/`i` maps and
  are not widgets. Untouched.
- **Terminal escape hatches stay hardcoded.** `<C-\><C-n>` and triple-`<Esc>` are
  safety exits, not rebindable widget keys (you must always be able to escape a
  runaway PTY). Terminal *job* mode is a later, careful consideration if at all.
- **Literal-argument reads** (`r{char}`, `f{char}`, `"{reg}`) and the `:s///c` answer
  alphabet are fixed grammars, not key tables — out of scope.
- **No parallel config table.** There is deliberately no `nx.picker.setup{ mappings
  = … }`; the keymap engine *is* the configuration surface (the whole point).

## Testing (black-box, per the no-unit-test rule)

Drive keys against the running server with a picker / widget open and assert on the
`redraw` (selection moved, preview scrolled, menu closed) and on confirm/cancel
outcomes; assert a user `vim.keymap.set('<widget>', …)` overrides the default and a
`<Nop>` disables it; assert no editing-mode map leaks into a widget bucket. No
`#[test]` units in the crates.
