# `nx.ui.input` / `nx.ui.confirm` — completing the `nx.ui.*` primitive set

**Status:** **done (2026-06-15).** Both Lua surfaces landed in
`crates/nxvim-lua/src/prelude/ui.lua` (tested black-box in
`crates/nxvim-server/tests/ui_prompt.rs`, 10 cases) plus the
`examples/ui-prompt/` config (loaded + driven end-to-end in the same suite).
Builds clean on both `native` and `--no-default-features` (wasm-eligible);
`cargo fmt` / `clippy -D warnings` clean.

This finishes the four small async UI primitives the
[native plugin API](../specs/2026-06-11-native-plugin-api.md) names —
`nx.ui.input` / `select` / `confirm` / `float`. `select` and `float` were already
complete; this phase added the two **command-line prompt** primitives, `input`
and `confirm`. (The earlier
[content-float plan](2026-06-15-nx-ui-float-content-float.md) asserted input and
confirm already existed — they did not at the Lua layer; this closes that gap.)

## What already existed (so this was pure Lua wiring)

The entire stack **below** the Lua surface was already built — for both, the
only missing piece was a Lua function calling the bridge:

- **`nx.ui.input`** — the `nx._ui_input(prompt, default, cb_id)` bridge
  (`install.rs`), the `UiInputReq` queue (`runtime.rs`), the server drain
  (`effects.rs` → `Editor::open_prompt`), the `CmdlineKind::Prompt` line-editing
  in core (`editor/cmdline.rs`), and the result delivery
  (`prompt_results` → `run_ui_input(id, result)` → `nx._run_cb(id, false, text)`)
  were all present and exercised. There was **no Lua function** calling
  `nx._ui_input` anywhere — a dangling Phase-8 bridge.
- **`nx.ui.confirm`** — likewise the `nx._confirm(label, accelerators, default,
  cb_id)` bridge, the `ConfirmReq` queue, `Editor::open_confirm` +
  `handle_confirm` (single-keypress resolution; the chosen 1-based index, `0` on
  `<Esc>`, arrives as a string through the **same** `prompt_results` channel as
  input — one prompt open at a time). Built for the removed blocking
  `vim.fn.confirm`; no `nx.ui`-shaped Lua surface called it.

## Design

Both follow the `nx.ui.select` shape exactly: allocate a one-shot callback id in
`nx._cb_fns`, queue the bridge request, and let the server fire `nx._run_cb` with
the result on a later tick. Non-blocking, callback-shaped (ADR 0002 rule 3).

- **`nx.ui.input(opts, on_confirm)`** [alias `vim.ui.input`, per the ADR 0002
  whitelist]. `opts = { prompt, default }`. `on_confirm(text)` gets the entered
  string (`""` on an empty `<CR>`) or `nil` on `<Esc>` (matching neovim).
- **`nx.ui.confirm(message, opts, on_choice)`** (also the 2-arg
  `nx.ui.confirm(message, on_choice)`). A **yes/no** confirmation: `on_choice`
  gets a **boolean** — `true` on Yes, `false` on No / cancel. `opts.default =
  true|false` picks the `<CR>` button (default Yes); the label renders a
  shell-style `[Y/n]` / `[y/N]` hint with accelerators `y`/`n`. The wrapper folds
  the bridge's 1-based index string to the boolean (`1` → true; `2`/`0` → false).
  Deliberately **not** aliased to `vim.*` (neovim's `vim.fn.confirm` is the
  blocking form the nx model omits; `vim.ui.confirm` is not a neovim function),
  and deliberately yes/no only — an arbitrary multi-choice menu is
  `nx.ui.select`'s job.

## wasm parity

The `input`/`confirm` requests drain in `apply_lua_effects` / `run_pending`
beside `take_ui_selects` / `take_ui_floats` (already verified working in the
wasm edit-host), so they inherit the same parity by construction — no new
`#[cfg(native)]` gate.

## Tests (`crates/nxvim-server/tests/ui_prompt.rs`, black-box)

`input`: typed text on `<CR>`; `nil` on `<Esc>`; prefilled default returned
unedited; empty `<CR>` is `""` not `nil`; `vim.ui.input` is the alias. `confirm`:
`y` → true; `n` → false; `<CR>` takes the default (both polarities); `<Esc>` →
false (cancel is never a silent true). Plus the shipped `examples/ui-prompt`
config loads and its `\d` confirm map deletes a line end-to-end.
