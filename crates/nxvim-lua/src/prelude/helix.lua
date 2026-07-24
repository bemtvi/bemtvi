-- nxvim Lua prelude — the Helix selection-first keymap plugin (opt-in).
--
-- Phase 5 of the Helix editing model (`docs/plans/2026-07-21-helix-editing-model.md`).
-- The *engine* — the persistent `anchor..head` selection set and the selection-first
-- grammar — lives natively in `crates/nxvim-core/src/editor/helix.rs`; the native
-- `handle_helix` handler hardwires the single-key motions and verbs so Helix mode is
-- usable and testable without any plugin. This module is the **bindings**: it turns
-- the model on (`nx.helix.enable`) and layers the goto (`g`) and space (`<space>`)
-- menus, insert-entry keys, and undo/redo onto the native grammar through the
-- rebindable `helix`-mode keymap bucket. Everything routes through the named-action
-- seam `nx._helix_action` -> `Editor::apply_helix_action`, so a user can rebind any
-- verb by name (`nx.helix.actions.<name>`) exactly like `nx.ui.select_actions`.
--
-- Opt-in: nothing here fires until `nx.helix.enable()` (or `:helix`) enters
-- `Mode::HelixNormal`. Registering the default maps at load is inert — the `helix`
-- bucket (`'h'`) is only ever consulted while a Helix mode owns input, and an
-- unmapped key still falls through to the native `handle_helix` grammar.

nx.helix = nx.helix or {}

-- ----- named-action table ----------------------------------------------------
-- `nx.helix.actions.<name>(count?)` fires the named Helix verb through the engine
-- (`nx._helix_action(name, count)` -> `Editor::apply_helix_action`). `count` is
-- optional — omit it and the verb reads the digits typed before the key (the native
-- `helix_count`). Bind these with `nx.keymap.set("helix", "<key>", nx.helix.actions.<name>)`.
-- The names mirror Helix's own command names so a Helix user's muscle memory (and
-- config) carries over. Unknown names fail loud at the engine.
nx.helix.actions = nx.helix.actions or {}
for _, name in ipairs({
  -- mode / insert entry
  "normal_mode",
  "select_mode",
  "insert_mode",
  "append_mode",
  "insert_at_line_start",
  "insert_at_line_end",
  "open_below",
  "open_above",
  -- goto (`g` menu)
  "goto_file_start",
  "goto_last_line",
  "goto_line_start",
  "goto_line_end",
  "goto_first_nonwhitespace",
  -- selection-set verbs
  "flip_selections",
  "extend_line_below",
  "extend_line_above",
  "select_all",
  "collapse_selection",
  "keep_primary_selection",
  "remove_primary_selection",
  "trim_selections",
  "copy_selection_on_next_line",
  "copy_selection_on_prev_line",
  "rotate_selections_forward",
  "rotate_selections_backward",
  "rotate_selection_contents_forward",
  "rotate_selection_contents_backward",
  "align_selections",
  "join_selections",
  "replace_selections_with_yanked",
  -- selection-regex prompts
  "select_regex",
  "split_selection",
  "keep_selections",
  "remove_selections",
  -- immediate-apply operators
  "delete_selection",
  "change_selection",
  "yank",
  "indent",
  "unindent",
  "format_selections",
  "switch_case",
  -- paste / undo
  "paste_after",
  "paste_before",
  "undo",
  "redo",
}) do
  nx.helix.actions[name] = function(count)
    nx._helix_action(name, count)
  end
end

-- ----- enable / disable ------------------------------------------------------
-- `nx.helix.enable()` turns the selection-first model on (enters `Mode::HelixNormal`),
-- `nx.helix.disable()` turns it off (back to vim's Normal). Both are idempotent — the
-- engine no-ops a redundant toggle — so calling them from a keymap or `init.lua` is safe.
--
-- `nx.helix.enable{ smart_case = false }` also sets the search-case default (see
-- `nx.helix.smart_case` below); omit the field to keep the current setting.

function nx.helix.enable(opts)
  opts = opts or {}
  if opts.smart_case ~= nil then
    nx.helix.smart_case(opts.smart_case)
  end
  nx._helix_action("enable_helix")
end

function nx.helix.disable()
  nx._helix_action("disable_helix")
end

-- `nx.helix.smart_case(on)` sets whether Helix document search (`/`/`?`/`n`/`N`)
-- defaults to smart-case — case-insensitive unless the pattern carries an uppercase
-- character — Helix's own default (`on == nil` means `true`). This is **self-contained**:
-- it never touches the global `:set ignorecase`/`smartcase` that vim-mode search reads,
-- so toggling it leaves your vim-mode search behavior untouched. Turn it off for
-- case-sensitive Helix search (which then follows the global options like vim mode does).
function nx.helix.smart_case(on)
  if on == nil then
    on = true
  end
  nx._helix_action(on and "smart_case_on" or "smart_case_off")
end

-- ----- default `helix`-bucket keymaps ----------------------------------------
-- The bindings the native `handle_helix` grammar does NOT already provide: insert
-- entry, the goto/space menus, and undo/redo. The core motions (`hjkl`/`w`/`b`/`e`/
-- `f`/`t`), the selection verbs (`x`/`%`/`s`/`;`/`,`/`_`/…), the operators (`d`/`c`/
-- `y`/`>`/`<`/`=`/`~`), paste (`p`/`P`), `v` (select toggle) and `<Esc>` are handled
-- natively and left unmapped so they keep working with or without this plugin.
--
-- `default = true` puts each at the overridable rung, so a user's own
-- `nx.keymap.set("helix", …)` wins (bind an empty function to disable a key).

-- Insert entry — Helix places the cursor at the selection edge, then opens Insert.
local function set(lhs, rhs, desc)
  nx.keymap.set("helix", lhs, rhs, { default = true, desc = desc })
end

set("i", nx.helix.actions.insert_mode, "Insert before selection")
set("a", nx.helix.actions.append_mode, "Append after selection")
set("I", nx.helix.actions.insert_at_line_start, "Insert at line start")
set("A", nx.helix.actions.insert_at_line_end, "Insert at line end")
set("o", nx.helix.actions.open_below, "Open line below")
set("O", nx.helix.actions.open_above, "Open line above")

-- Undo / redo (unreachable from the native grammar).
set("u", nx.helix.actions.undo, "Undo")
set("U", nx.helix.actions.redo, "Redo")

-- The goto (`g`) menu. `gg`/`ge`/`gh`/`gl`/`gs` are pure motions; `gd`/`gr`/`gy`/`gi`
-- defer to LSP (resolved at call time, so a no-server config just no-ops the request).
set("gg", nx.helix.actions.goto_file_start, "Go to file start")
set("ge", nx.helix.actions.goto_last_line, "Go to last line")
set("gh", nx.helix.actions.goto_line_start, "Go to line start")
set("gl", nx.helix.actions.goto_line_end, "Go to line end")
set("gs", nx.helix.actions.goto_first_nonwhitespace, "Go to first non-whitespace")
set("gd", function()
  nx.lsp.definition()
end, "Go to definition")
set("gy", function()
  nx.lsp.type_definition()
end, "Go to type definition")
set("gr", function()
  nx.lsp.references()
end, "Go to references")
set("gi", function()
  nx.lsp.implementation()
end, "Go to implementation")

-- The space (`<space>`) leader menu — Helix's file/search/LSP commands, mapped onto
-- nxvim's native pickers (`nx.picker.open`) and LSP verbs (`nx.lsp.*`).
local function pick(source)
  return function()
    nx.picker.open(source)
  end
end

set("<Space>f", pick("files"), "Find files")
set("<Space>b", pick("buffers"), "Find buffers")
set("<Space>j", pick("jumplist"), "Jumplist")
set("<Space>'", pick("marks"), "Marks")
set("<Space>/", pick("live_grep"), "Global search")
set("<Space>k", function()
  nx.lsp.hover()
end, "Hover documentation")
set("<Space>r", function()
  nx.lsp.rename()
end, "Rename symbol")
set("<Space>a", function()
  nx.lsp.code_action()
end, "Code action")
set("<Space>s", function()
  nx.lsp.document_symbol()
end, "Document symbols")
set("<Space>S", function()
  nx.lsp.workspace_symbol()
end, "Workspace symbols")
set("<Space>d", pick("diagnostics"), "Diagnostics")

-- Diagnostic navigation — `]d`/`[d` jump to the next/previous diagnostic, `]e`/`[e`
-- to the next/previous *error* (severity ERROR). Mirrors the vim-mode defaults in
-- `diagnostic.lua`, bound in the `helix` bucket (Helix's own `[d`/`]d` bindings).
set("]d", function()
  nx.diagnostic.goto_next()
end, "Next diagnostic")
set("[d", function()
  nx.diagnostic.goto_prev()
end, "Previous diagnostic")
set("]e", function()
  nx.diagnostic.goto_next({ severity = nx.diagnostic.severity.ERROR })
end, "Next error")
set("[e", function()
  nx.diagnostic.goto_prev({ severity = nx.diagnostic.severity.ERROR })
end, "Previous error")
