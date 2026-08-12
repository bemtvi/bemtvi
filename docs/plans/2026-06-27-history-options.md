# `history` + `persisthistory` options

Status: Phase 1 + 2 done
Date: 2026-06-27

## Goal

Two configurable options controlling command-line / search (and prompt) history —
the knobs bemtvi was missing vs neovim's `'history'` + `'shada'`.

- **`history`** (Num, global, default `10000`) — the in-memory cap on each history
  ring (ex `:`, search `/`, and the `btv.ui.input` namespace rings). `0` disables
  history. Mirrors neovim's `'history'`.
- **`persisthistory`** (Str, global, default `"workspace,global"`) — where history
  persists across sessions, as a **priority list**: the first *available* scope wins
  (a single target, not a dual write). Tokens: `workspace` (the per-namespace store,
  available only in a `--workspace` / `--shada-namespace` session), `global` (the shared
  store, always available), or the lone `none`.
  - `workspace,global` (default) — save to the workspace store **when a workspace is
    open, else** the global store. So a project's history is restored in that project;
    a plain session uses global.
  - `global` — always the global store (even inside a workspace).
  - `workspace` — only the workspace store (a plain session then persists nothing).
  - `none` — never persist history.

## Behavior

### `history` (Phase 1)
Cap each ring to the newest `history` entries whenever it grows or is seeded:
`remember_ex` / `remember_search` / `remember_prompt` after push, `import_persist`
after the history merge, and when the option itself changes. `0` clears them. The
store keeps its own `HISTORY_CAP = 10000` ceiling on the persisted set (a value above
10000 is kept in memory but only the newest 10000 survive a restart — documented).

### `persisthistory` (Phase 2)
Resolve the option to a **single** `HistoryScope` (`Workspace` / `Global` / `None`)
via `effective_history_scope(value, workspace_open)` — the first available token in the
priority list. The *primary* store is `self.shada` — the global store for a plain
launch, the namespaced store for a workspace launch; its scope is `Global` / `Workspace`
accordingly. Routing:

- **Primary store** carries history iff the chosen scope **is** the primary's scope
  (`gate_primary_history` clears the history fields from the snapshot otherwise). So the
  default writes workspace history to the namespace store (workspace session) or global
  history to the global store (plain session) — the common cases use only `self.shada`.
- **Global store handle** (native, local only): used **only** when a workspace launch
  resolves to `Global` (`"global"` / `"global,workspace"`). Then history routes there
  *instead* of the workspace store — a `RedbFileStore` over `shada_dir()` flushed
  **history-only** and **never compacting** (it shares the dir with plain sessions'
  full-state files; compaction there would drop their marks/registers). Restored
  post-config via a history-only merge that leaves marks/registers untouched
  (`init_global_history` drops the handle when the session targets workspace / none).

So the default is workspace-scoped when a workspace is open (restored on reload) and
global otherwise; `global` forces the shared store; `none` persists nothing.

Caveats / scope:
- The global-store override is native + local only. A `Remote`/daemon session keeps the
  existing single-store behavior; the option still gates `none` and the primary scope.
- wasm/EditHost has a single OPFS store, so the option is effectively `persist` vs
  `none` there.
- Phase 2 routes the ex/search histories. The `btv.ui.input` namespace rings are capped
  by `history` but not yet persisted (a later extension on top of this seam).

## Phases
1. ✅ `history` option + in-memory cap (+ tests).
2. ✅ `persisthistory`: validate (E474 on `:set`, lenient `btv.o`), resolve to a single
   `HistoryScope` (priority list), primary-store write gating (`gate_primary_history`),
   and — only when a workspace launch targets `global` — route to the injected global
   store (`ServerInit::global_shada`) opened post-config (`init_global_history`) with a
   history-only merge (`Editor::merge_persisted_history`) + history-only never-compacting
   flush. Tests: the `none` gate, default workspace-scoping, `global` crossing
   workspaces, validation.

## Implementation notes
- `shada_load` runs **before** `init.lua`, so the config-set `persisthistory` is known
  only post-config: the primary store loads its own-scope history pre-config (unchanged
  — this is "restore the workspace history when it's loaded" for the default), and the
  global store is merged in `init_global_history` after sourcing. Consequence: on a
  workspace session targeting `global` (a non-default override), the workspace history
  still loads pre-config, so those entries carry for that session — a documented edge of
  the load-before-config ordering. The default and `none` are exact.
- The global store handle is injected (`global_shada`) so tests never touch the real
  `~/.local/state/bemtvi/shada`; the binary sets it to `default_shada()` for a local
  workspace launch, `None` for plain/remote.
