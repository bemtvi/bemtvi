# Undo navigation: `:undolist`, `g-`/`g+`, `:earlier`/`:later`, `'undolevels'`

Close the gap between bemtvi's undo **data model** (complete) and its undo **command
surface** (four entry points). The branching `UndoTree` already stores everything vim's
undo features need; almost nothing reads it.

## What exists, and what reads it

`crates/bemtvi-core/src/editor/undo.rs` stores per node:

| field | who reads it today |
|---|---|
| `seq` | `u`, `<C-r>`, `:undo {N}`, the `undotree()` projection |
| `parent` / `children` | `u` (parent), `<C-r>` (`children.last()`) |
| `time` (monotonic secs, `Editor::now_mono`) | the projection only — **no command** |
| `save` / `save_last` | the projection only — **no command** |
| `node_of_seq()` | `:undo {N}` only |

So `time` and `save` are minted on every commit and every write and are, today, dead
weight: the two features that would consume them (`:earlier {N}f`, `:undolist`'s saved
column) don't exist.

Verified missing (behaviorally, against a running server):

| | symptom |
|---|---|
| `g-` / `g+` | silent no-op — never enters the `g` submap (`command.rs:1290–1348`) |
| `:undolist` / `:undol` | `E492: Not an editor command` |
| `:earlier` / `:later` (`:ea`/`:lat`) | `E492` — incl. count, `{N}f`, `{N}s/m/h/d` |
| `'undolevels'` | `E518`, and the tree is genuinely **unbounded** (300 edits → 300 retained nodes) |

Deliberately **out of scope** here (each is a feature of its own, not a read of the
existing tree): `U` (undo-line), `:undojoin`, `'undoreload'`, and persistent undo
(`'undofile'` / `'undodir'` / `:wundo` / `:rundo`).

## The one model decision: time is monotonic, so "when" is relative

`now_mono` is **seconds since server start** (`dispatch.rs:46`, stamped once per
message), chosen so elapsed labels survive wall-clock jumps. There is no wall-clock
source in the core. vim's `:undolist` prints `HH:MM:SS` for entries older than 100
seconds; bemtvi cannot, and inventing a wall clock for one column would break the
monotonic guarantee the field was created for.

So **every** "when" is rendered relative — `"12 seconds ago"`, `"3 minutes ago"`,
`"2 hours ago"`, `"4 days ago"` — extending vim's own `<100s` form rather than
contradicting it. Same basis for `:earlier {N}s`: it compares against node `time`
deltas, which are exact in this timeline.

## Phase 1 — `:undolist`

`:undol[ist]` lists **the leafs in the tree of changes** (vim's wording), not every
state: a leaf is a node with no children, plus the virtual pending node when the live
buffer has diverged. Columns are vim's:

```
number changes  when               saved
     3        3  12 seconds ago
     5        4  3 seconds ago         1
```

- `number` — the leaf's `seq`.
- `changes` — its depth from the root (how many changes reach it).
- `when` — relative age of its `time` (see above).
- `saved` — its `save` number, blank when the state was never written.

Empty history (root only, not dirty) echoes `Nothing to undo`, as vim does.

**Touches:** a `UndoTree::leaves()` walk + `Editor::ex_undolist` next to `ex_changes`
(`changelist.rs`) rendering through the existing `open_scratch_listing`; the `"undol" |
… | "undolist"` arm in `ex.rs`'s dispatch; the `cmdline_complete.lua` command table.

## Phase 2 — `g-` / `g+` and `:earlier`/`:later` counts

All four are one operation: **move `N` states along seq order**, across branches, which
is exactly what the flat `nodes` vec already supports.

- `g-` / `:earlier [N]` → target `cur_seq - N`
- `g+` / `:later [N]` → target `cur_seq + N`
- clamp the target to `[0, seq_last]`, then land on the **nearest existing** node in the
  direction of travel — not `node_of_seq(target)` directly, because phase 4's pruning
  makes seqs sparse. Written nearest-seeking from the start so phase 4 needs no revisit.
- at either end, echo vim's `Already at oldest change` / `Already at newest change`.

Note this is *not* `u`/`<C-r>`: `u` walks to the tree **parent**, `g-` walks to the
previous state **in time**. On a linear history they coincide; after a branch they
deliberately don't, which is the whole point of the pair.

**Touches:** `Editor::undo_step_seq(delta)` in `undo.rs` (shares `restore_snapshot`
with `undo_to_seq`); `NormalCmd::TimeTravel(bool)` wired into the `g` submap in
`command.rs`; `"earlier"`/`"later"` arms in `ex.rs`; dot-repeat exclusion the way
`NormalCmd::Undo` sets `change_not_repeatable`.

## Phase 3 — `:earlier`/`:later` time and file forms

Same landing machinery as phase 2, different target resolution:

- **`{N}s|m|h|d`** — target time = *the current node's* `time` ± N seconds (vim's basis
  is `b_u_time_cur`, not "now"). Going earlier, land on the node with the greatest
  `time <= target`, tie-broken by greatest `seq`; going later, the mirror.
- **`{N}f`** — step over file writes using `save`. The current save number is the
  current node's `save` if it has one, else the nearest **ancestor**'s (an ancestor
  walk, not a seq comparison: a save on an abandoned branch is not "behind" you). When
  the current state is not itself a save point, going earlier spends one step reaching
  the last write, matching vim. Past either end, land on seq 0 / `seq_last`.

Bare `:earlier` / `:later` mean `1`. An unparseable argument is `E475: Invalid
argument`, loud, never a silent no-op.

**Touches:** an `EarlierTarget` parse in `ex.rs` + two resolvers in `undo.rs`.

## Phase 4 — `'undolevels'`

The only entry here that is arguably a live bug rather than a missing feature: every
node holds a full `Snapshot`, and while the rope clone is cheap (persistent), the
`extmarks` / `marks` / `changelist` clones are not. A long session retains all of them.

- Buffer option with a global tier, so it lands in `BufferOptions` and must be
  classified in `inherit_settable` (which destructures — it fails to compile until
  classified). Default `1000`, vim's.
- `-1` disables undo recording entirely; `0` keeps one level.
- **Pruning** runs after `commit()`: while the tree holds more than `undolevels`
  states below the root, drop the oldest. In a *branching* tree "drop the oldest" means
  re-rooting: promote the root's child that is an ancestor of `cur` to be the new root
  (`parent = None`), and delete the old root together with every other child's subtree —
  vim's `u_freeheader` + `u_freebranch`, i.e. discarding the oldest history takes its
  abandoned branches with it.
- `nodes` is a `Vec` indexed by `NodeIdx`, so pruning must **compact and remap**:
  rebuild the vec, remap `parent`/`children`/`cur`. Seqs are preserved (never
  renumbered), which is why phase 2 seeks the nearest existing seq rather than an exact
  one, and why `:undo {N}` on a pruned-away seq keeps failing with its existing `E830`.

**Touches:** `options.rs` (`OPTIONS` registry, `apply_set_*`, `inherit_settable`);
`UndoTree::prune`; the `btv.bo`/`BoMirror` row and `state.lua`'s alias/default tables.
The `every_known_option_is_wired_not_silent` guard needs no edit — it enumerates the
catalog itself, so it covers the new option automatically.

## Testing

Per the project convention, everything is black-box through the running server, in a new
`crates/bemtvi-server/tests/editing/undo.rs` submodule behind the `editing.rs`
entrypoint. Each phase lands with its own tests and a commit:

1. leaf-only listing (linear history lists one leaf; a branch lists two), the `changes`
   depth column, the `saved` column after `:w`, the pending-node row, `Nothing to undo`.
2. `g-`/`g+` round trip on a linear history; the branch case where `g-` and `u` diverge;
   clamping messages at both ends; `:earlier 3` / `:later 2`.
3. `:earlier 1f` from a dirty state and from a saved state; `{N}s` with the injected
   monotonic clock; `E475` on garbage.
4. that a bounded `'undolevels'` prunes (tree size stops growing), that the surviving
   spine still undoes correctly, that `-1` records nothing, and that the global tier
   seeds new buffers.

## One thing the work changed that wasn't planned

`UndoTree::commit` stamped each node with the time of the **commit**, which — under the
lazy-commit model — is the moment the *next* change group starts, not when the change
was made. That made the same state report two different `time`s to `undotree()` before
and after commit (`view` already used `dirty_since` for the pending node), and would
have made `:earlier {N}s` measure from when a change ended. `commit` now stamps
`dirty_since`, matching both vim and the projection's own claim.

Testing the time forms needed a deterministic clock, so `ServerInit` gained a
`mono_clock` test seam mirroring the existing `mouse_clock` — same shape, same
`TestClock` type (`set_secs` alongside `set_ms`), read by `EditHost::mono_stamp_secs`.
Without it a `{N}s` test would have to sleep for real seconds.
