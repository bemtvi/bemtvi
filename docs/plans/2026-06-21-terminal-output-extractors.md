# Terminal output extractors → `TermOutput` autocmd + Lua scraping plugins

**Status:** planned · **Date:** 2026-06-21

## Goal

Let a plugin observe a built-in terminal's output as it streams and react to it
— the motivating use-case being **capturing file references from Claude Code's
output and populating a location list** so the user can jump straight to each
file.

The user's first instinct was a *Rust* API for registering text extractors
(start trigger + end trigger → engine ships the matching lines to the plugin).
This plan deliberately **moves all of that into Lua** and keeps Rust to the one
primitive that genuinely cannot live above the engine: a bounded, incremental
stream of finalized terminal output.

## Guiding constraints

- **Dogfood the `btv.*` plugin API.** Triggers, region semantics, ref-parsing and
  loclist policy are *policy* — they belong in a Lua plugin, not calcified in
  Rust. Rust ships only a generic output stream that *any* terminal-scraping
  plugin can build on. (See [[dogfood-btv-plugin-api]].)
- **Reuse existing machinery.** The event registry, autocmd version-gating, and
  the loclist/quickfix write path already exist; this feature is mostly wiring.
  (See [[reuse-existing-apis-before-new-ones]].)
- **The editor must never freeze.** The output event must be incremental and
  bounded per burst — ship only the *delta* of newly-finalized lines, coalesced
  per repaint tick, never the whole buffer. A terminal flood must not turn into
  an event flood. (See [[editor-must-never-freeze]], [[terminal-flood-backpressure]].)

## Key realization — finalized scrollback is the clean signal

Terminal output reaches the buffer by **splice**, not append:

- `EditHost::terminal_feed()` (`crates/bemtvi-server/src/terminal.rs:155`) pushes
  raw PTY bytes through the vt100 parser.
- `EditHost::terminal_project()` (`terminal.rs:326`) runs once per repaint and
  splices `history` + the live screen into the buffer via
  `Editor::terminal_update()` (`crates/bemtvi-core/src/editor/terminal.rs:176`).

So a buffer-attach `on_lines` observer is fragile for terminals: the **live
screen region is constantly rewritten** (cursor moves, spinners, progress bars)
and **scrollback rewrites shift line numbers**. A naive observer re-scans
churning rows and watches its line indices go stale.

The stable signal is the growth of `TermEmu::history: Vec<String>`
(`terminal.rs:93`) — oldest-first, append-only. **Once a row scrolls off the
live screen into history it is final and immutable.** That is the natural "a
line of output is now committed" event. Streaming *finalized scrollback only*
sidesteps both the churn and the line-shift problems.

### Two streams, picked per observer

Finalized-only is the right *default*, but it has one gap: output that never
scrolls off the live screen — a full-screen alt-screen TUI, or a streaming CLI
whose final block stays on-screen until the prompt returns — emits nothing (or
emits late, only once it scrolls). So the plan exposes **both modes** and lets
the plugin pick:

- **`committed` (default)** — finalized scrollback rows only. Append-only,
  immutable, no churn, no line-shift. The clean signal for `path:line:col`
  scraping where you only care about settled output.
- **`live`** — also stream the current live-screen rows on each repaint,
  flagged `provisional = true`. Captures everything immediately, but the
  contract is weaker: a provisional row may be **superseded** by a later frame
  (the line was rewritten) or **promoted** to committed (it scrolled into
  history unchanged). The plugin owns reconciliation — typically keyed on the
  live-screen row index, which `data` carries so a plugin can replace rather
  than append.

A `committed`-only observer never pays for live-tail bookkeeping; a `live`
observer gets committed rows too (as the authoritative version of a row it may
have already seen provisionally). Version-gating is per-mode, so the live path
costs nothing unless something opts in.

## Reuse map

| Need | Existing machinery |
|---|---|
| Fire an event to Lua | `fire_autocmd_data(event, pattern, buf, file, …)` (`crates/bemtvi-lua/src/runtime.rs:2065`) — already carries an `args.data` payload (LspAttach uses it) |
| Dispatch to handlers | `btv._fire(event, pattern, buf, file, data)` (`prelude/autocmd.lua:285`) |
| Register a handler | `btv.autocmd.create` / `btv.on` (`autocmd.lua:199`) |
| Cost nothing when unobserved | `btv._au_version` gating — server only fires events Lua has registered for (`autocmd.lua:41`) |
| Finalized output rows | `TermEmu::history` growth; `read_scrollback_text()` (`terminal.rs:500`) |
| Populate a loclist | `vim.fn.setloclist(win, items, action)`; or `btv._qf_populate(lines, efm, …)` (`install.rs:2492`) to parse raw lines through an errorformat |

## Seams (the only non-trivial parts)

1. **Compute the finalized-output delta cheaply.** `terminal_project()` already
   detects "scrollback changed since last frame" (`terminal.rs:337`). We need the
   *newly-committed* rows, not the whole history. Track a per-terminal
   `committed_len` (count of history rows already streamed); when `history.len()`
   grows, the delta is `history[committed_len..]`. Advance `committed_len`. This
   is O(new rows), not O(buffer). Coalesce within a single `terminal_project()`
   pass so a flood that commits many rows in one repaint fires **one** event with
   a bounded line vector (cap + drop-marker on extreme bursts, mirroring the
   existing `^C` flood-trim policy).

2. **`history` is server-side; the event must fire from the projection tick.**
   `terminal_project()` lives in `bemtvi-server` and already holds `editor` +
   the Lua runtime handle at repaint. Fire `TermOutput` from there, after the
   splice, with `data = { lines = <delta> }` and the terminal `buf` as both the
   pattern target and `args.buf`. Version-gate: skip the delta bookkeeping
   entirely when no `TermOutput`/`TermOpen`/`TermClose` autocmd is registered.

3. **Lifecycle bookends.** Add `TermOpen` (at `open_terminal` /
   `TerminalOp::Open` realization) and `TermClose` (at `terminal_freeze`,
   `terminal.rs:213`, when the child exits) so a plugin can arm its extractor on
   open and flush/teardown on close. Both are plain `fire_autocmd_buf` calls.

## API shape

A standard autocmd — no parallel registry, consistent with every other event.
The mode is an autocmd `pattern`-style opt-in so the registry/version-gating
needs no new concept: a plugin that wants the live tail registers for
`TermOutput` with `data.mode == "live"` honored at the fire site (or, more
simply, a distinct `pattern` the server checks — finalized-only being the bare
event).

```lua
-- committed-only (default): settled output, append-only
btv.autocmd.create("TermOutput", {
  callback = function(ev)
    -- ev.buf        = terminal buffer
    -- ev.data.lines = { "finalized", "rows", ... }   -- append-only, immutable
  end,
})

-- live: also see the current screen tail as it's drawn
btv.autocmd.create("TermOutput", {
  pattern = "live",                                   -- opt into the live tail
  callback = function(ev)
    -- ev.data.lines       = rows for this frame
    -- ev.data.provisional = true for live-screen rows (may be superseded/promoted)
    -- ev.data.row         = live-screen row index, for replace-not-append
  end,
})
```

The start/end-trigger extractor is **pure Lua** — a small state machine over the
incoming finalized lines:

```lua
-- a region extractor: collect lines between a start and end trigger,
-- then hand the captured block to a processor
local function extractor(opts)
  local capturing, block = false, {}
  return function(ev)
    for _, line in ipairs(ev.data.lines) do
      if not capturing and line:match(opts.start) then capturing = true end
      if capturing then table.insert(block, line) end
      if capturing and line:match(opts.stop) then
        capturing = false
        opts.process(block)   -- e.g. parse file refs → setloclist
        block = {}
      end
    end
  end
end
```

The Claude-Code → loclist plugin is then just an `extractor` whose `process`
pulls `path:line:col` refs out of the block and calls `vim.fn.setloclist` (or
hands the raw lines to `btv._qf_populate` with an errorformat).

## Phases (commit + pause for review between each — [[big-feature-workflow-cadence]])

### Phase 1 — `TermOutput` finalized-delta stream (`committed` mode)
- Per-terminal `committed_len`; compute `history[committed_len..]` in
  `terminal_project()`, fire `TermOutput` with `data.lines`, advance the marker.
- Version-gate so unobserved terminals do zero extra work.
- Bound + drop-marker on flood bursts.
- Tests (black-box, `tests/terminal*.rs`): register a `TermOutput` autocmd, run a
  command that emits known lines that scroll off-screen, assert the accumulated
  `data.lines` exactly match the committed output in order with no dupes/gaps;
  a flood test asserting boundedness + the drop marker.

### Phase 1b — `live` mode (provisional tail)
- Opt-in via the `live` pattern; gate separately so `committed`-only observers
  pay nothing. After the splice, emit the live-screen rows with
  `provisional = true` and a stable `row` index; committed rows still fire as the
  authoritative version.
- Tests: a command whose final block stays on-screen (never scrolls) is seen by a
  `live` observer but not a `committed`-only one; a rewritten row (spinner /
  progress) arrives as successive provisional frames at the same `row`, then —
  if it scrolls — once more as committed.

### Phase 2 — `TermOpen` / `TermClose` lifecycle events
- Fire from terminal open and `terminal_freeze`.
- Tests: autocmd observes open with the right `buf`; close fires once on child
  exit.

### Phase 3 — Lua extractor helper + example plugin
- Ship the `extractor` region-state-machine helper in the prelude (generally
  useful for plugin authors — [[expose-general-helpers-in-btv]]).
- Build a runnable `examples/terminal-loclist/` config: a Claude-Code file-ref
  extractor wired to `setloclist`, with a sample transcript fixture, verified
  end-to-end ([[example-config-for-testing]]).

## Out of scope
- Raw pre-vt100 PTY byte access from Lua.
- `TermEnter`/`TermLeave` mode events (separate concern from output).
