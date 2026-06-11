# Shada persistence — cross-session state in a multi-writer redb store

Today nxvim keeps no state across sessions. Registers (`Registers`,
`crates/nxvim-core/src/editor/registers.rs:34`), the global `A`–`Z` file marks
(`global_marks`, `mod.rs:424`), per-buffer `a`–`z`/special marks
(`Buffer.marks`), and the search/ex history (`search_history`/`ex_history`,
`mod.rs:507`/`:511`) all die with the process. This plan adds neovim's **shada**
("shared data") layer: a durable, cross-session store of that state, written
back on the next launch — so a yank in one session pastes in the next, `'A`
still jumps to the file you marked, `` `" `` reopens a file at its last cursor,
and `/` history survives.

It is **not** a transliteration of neovim's `shada.c`. nxvim's topology is
different in two ways that change the right design, and we lean into both:

1. **The daemon is long-lived.** A single `nxvim --server` outlives client
   connections, so cross-session state already lives in one authoritative
   process between attaches. The on-disk file is for durability across daemon
   restart / reboot / crash — not, as in neovim, the *only* shared memory
   between otherwise-isolated short-lived processes.
2. **There can be several writers.** Multiple editor servers may run on one
   machine (several local `nxvim` instances; or, in the remote-SSH topology,
   several `nxvim --server`s sharing one host's state dir). They must reconcile.

We persist into a **per-instance [redb](https://docs.rs/redb) store** — pure-Rust,
ACID, copy-on-write B-tree, single-file — and reconcile concurrent writers by a
**recency merge on read across sibling stores**, the one piece of shada's design
that is genuinely warranted once writers are concurrent and isolated.

**Scope.** Registers, global + per-file marks (special marks and the `` `" ``
last-cursor included), search/ex command history, the **jumplist** and the
**changelist** (both now exist in core, see below), and the **numbered marks
`'0`–`'9`** — which are a *pure persistence construct* (cursor on last exit,
shifted across the last ten sessions) and were explicitly **deferred to this
plan** by the jumplist commit (`5672aa8`: "numbered marks … need a persistence
layer of their own"). So shada doesn't just *save* state — it **unlocks** a
feature that cannot exist without it. **Out of scope by decision:** `oldfiles`
and `vim.g` globals.

> **Update (origin sync).** This revises the first draft, which predated
> `5672aa8 feat(editor): jumplist (C-o/C-i), changelist (g;/g,), and g'/g\`` and
> `019260a feat(server): the HostEffects seam`. Jumplist/changelist are no
> longer "reserved slots" — they are real, persisted payloads; and the off-tick
> writes now ride the `HostEffects` seam rather than calling the evloop/fs
> directly (see *The store* below).

---

## Why redb, and why *per-instance* (the load-bearing constraint)

redb's concurrency model, verified against its docs before planning:

- **Within one process** a `redb::Database` is `Send + Sync`, shared via `Arc`:
  many concurrent read transactions, a single writer, writes serialized. ACID,
  copy-on-write, crash-safe — a `kill -9` mid-write leaves the last committed
  state intact, never a half-written file.
- **Across processes** redb takes an **exclusive advisory file lock** on
  `Database::open`. A second process opening the *same* file fails/blocks. redb
  is a single-process store by design.

So "one shared `main.redb` every daemon writes" is not available — it would
block or corrupt. The lock is not an obstacle, though; it *is* the merge
boundary:

- Each daemon owns **its own** file, `stdpath("state")/shada/<host>.<pid>.<rand>.redb`.
  Single-process ⇒ redb's full ACID/crash-safety/incremental-write guarantees
  apply to each writer with zero contention.
- A daemon reads every **other** sibling file it can open. A *live* instance
  holds its lock, so its file is skipped (invisible until it checkpoints/exits).
  A *cleanly-exited or crashed* instance released its lock (the OS drops advisory
  locks on process death), so its file is openable read-only and redb's recovery
  handles any uncommitted tail. The reader **recency-merges** those payloads into
  its in-memory editor state, then writes only to its own file thereafter.

This reproduces neovim's exact observable contract — *you see another instance's
data once it has written, not while it is live* — while adding crash-safety
neovim lacks (neovim writes shada only on `:q`, losing everything on a crash).
And it needs **no second file format**: the merge reads sibling redb files
directly.

### Why this is feasible now (de-risking facts)

Verified in the current tree before planning:

- **The state to persist is already isolated, owned data.** Registers are a
  `HashMap<char, RegisterCell>` behind `Registers` (`registers.rs:34`); global
  marks are `global_marks: HashMap<char, (BufferId, Cursor)>` (`mod.rs:424`);
  per-buffer marks are `Buffer.marks: HashMap<char, (usize, usize)>`
  (`buffer.rs:146`); history is two `Vec<String>` (`mod.rs:507`/`:511`). None of
  it touches I/O, async, or the rope — exactly the plain owned data a snapshot
  wants.
- **Jumplist and changelist now exist and are plain, copyable position lists.**
  The jumplist is per-window — `Window.jumps: Vec<JumpEntry { buf, line, col }>`
  + `jump_idx` (`windows.rs:234`, `jumps.rs`), 100-cap, funnelled through the
  single `record_jump_context` choke point. The changelist is per-buffer —
  `Buffer.changelist: Vec<(usize, usize)>` + `changelistidx` (`buffer.rs:155`),
  already snapshotted/restored across undo. Both are `(line, col)`-shaped owned
  data; persisting them is the same path → save → restore-by-path treatment marks
  get, with the jumplist's `BufferId` resolved to a path on export.
- **The `HostEffects` seam is the right home for the off-tick write.** The
  EditHost extraction (`019260a`) routes the sync tick's *outbound async effects*
  through `trait HostEffects` (wire `notify`/`respond`, the evloop
  `loop_command`), with the trait growing an **off-tick fs slice (4b)**. Shada's
  debounced file write is precisely an off-tick fs effect, so it lands on that
  seam instead of touching the filesystem from the tick — and because the seam is
  what lets "one sync core serve both the native server and the wasm Worker," a
  future **wasm persistence backend** (IndexedDB/OPFS) is a different
  `HostEffects` impl, not a rewrite.
- **Core already does dependency injection for impure providers.** The syntax
  engine (`Option<Box<dyn SyntaxEngine>>`, `mod.rs`) and the clipboard
  (`Option<Box<dyn Clipboard>>`, the registers plan) keep `nxvim-core` pure while
  the server supplies the real implementation. Persistence follows the *same*
  pattern inverted: core exposes plain `export_persist`/`import_persist`
  accessors and the server owns every byte of I/O. No purity violation — serde
  *derives* on a plain struct are pure (no I/O), matching the "no I/O beyond
  `Buffer` read/write" rule.
- **There is one clean quit point and a ready debounce mechanism.** The server's
  `select!` loop checks `server.editor.should_quit` and emits `nxvim_exit`
  (`crates/nxvim-server/src/lib.rs:819`, again `:883`) — the single place a final
  flush hooks in. The evloop actor already arms wall-clock timers
  (`LoopCommand::TimerStart { id, delay, repeat }`, `evloop.rs:38`) off the
  editor thread; the debounced background flush reuses that machinery rather than
  inventing a timer.
- **`stdpath("state")` already resolves.** `crates/nxvim-lua/src/host.rs:190`
  maps `state` → `$XDG_STATE_HOME/nxvim` (or `~/.local/state/nxvim`) — exactly
  where neovim 0.10+ keeps shada (`stdpath('state')/shada/`). The store dir is a
  `shada/` subdir of that, no new path logic.
- **serde + msgpack are already in the workspace.** `serde = "=1.0.228"` (with
  `derive`) and `rmpv = "=1.3.1"` are pinned in the root `Cargo.toml`. We add
  `redb` and `rmp-serde` (the serde↔msgpack bridge) as the only new exact-pinned
  deps, both pure-Rust (no C — keeps the cross-platform/wasm posture; note shada
  is a *server* concern, so the wasm `nxvim-core` build never links redb anyway).

---

## Architecture

### The snapshot type lives in core; all I/O lives in the server

`nxvim-core` gains a small `persist` module of plain owned data — no redb, no
serde-of-I/O, just the schema and the two accessors:

```rust
// crates/nxvim-core/src/editor/persist.rs
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct PersistState {
    pub version: u32,                       // schema version, bumped on change
    pub registers: Vec<RegisterEntry>,      // name + text + kind + timestamp
    pub global_marks: Vec<GlobalMarkEntry>, // A–Z: path + line + col + ts
    pub numbered_marks: Vec<NumberedMark>,  // '0–'9: path + line + col + ts (NEW)
    pub file_marks: Vec<FileMarkEntry>,     // (path, name) → line/col + ts;
                                            //   carries a–z, the specials, and `"`
    pub file_changelists: Vec<FileChanges>, // (path) → Vec<(line,col)> + ts
    pub jumplist: Vec<JumpPos>,             // path + line + col, focused window's
    pub search_history: Vec<HistEntry>,     // text + timestamp
    pub ex_history: Vec<HistEntry>,
}

impl Editor {
    pub fn export_persist(&self) -> PersistState { /* read live state */ }
    pub fn import_persist(&mut self, state: PersistState) { /* seed live state */ }
}
```

Every entry carries a `timestamp` (monotonic-with-wall-clock at write) because
the timestamp **is** the merge key. Positions store a **path**, not a `BufferId`
(ids are per-session and meaningless across restarts); `import_persist` resolves
a path back to a buffer lazily on first jump, exactly as the existing global
mark `(buffer, cursor)` jump already opens-or-switches by path.

**Jumplist.** Core's jumplist is *per-window* (`Window.jumps`), but a restored
session has no windows yet — so on export we snapshot the **focused window's**
jumplist (mapping each `JumpEntry`'s `BufferId` → path) and on import seed it
into the initial window. This matches the observable neovim contract (one
restored jumplist you `<C-o>` back through), without pretending to restore N
splits that no longer exist.

**Changelist.** Core's changelist is *per-buffer* (`Buffer.changelist`), so it
persists **per file**, keyed by path alongside that file's marks — restored when
the file is reopened, the same lazy-by-path resolution. (Note: stock neovim does
*not* put the changelist in shada; persisting it is an nxvim improvement, cheap
because the data is already per-buffer owned state.)

**Numbered marks `'0`–`'9` (the unlocked feature).** These have **no live core
state to read** — they exist *only* in the persistence layer, by vim's rule:
`'0` is the cursor where this session last exited; on the next launch the old
`'0` shifts to `'1`, `'1`→`'2`, …, dropping `'9`. So the *shift* happens in
`shada.rs` at load/merge time (not in core), and `import_persist` seeds the
resulting ten `(path, line, col)` marks into a new `Editor` store that `` `0 ``…
`` `9 `` jump through. This is why the jumplist commit deferred them here:
they're a function of *sessions over time*, which only the store sees.

`import_persist` is **additive/clamped**: a restored mark whose file no longer
exists is dropped on jump (the existing `E20`/`E37` path), a cursor past EOF is
re-clamped by the canonical `set_cursor_char` — restoring never trusts the file
blindly.

### The store: `nxvim-server/src/shada.rs`

A new server module, sibling to `save.rs`/`daemon.rs`, owns the redb file and
the merge. It never holds the `!Send` editor; it takes a `PersistState` value
in and hands one out.

```
stdpath("state")/shada/
├── arch.4711.a3f1.redb      ← this instance (locked while live)
├── arch.4699.91bc.redb      ← a cleanly-exited instance (readable)
└── arch.4702.0c5d.redb      ← a crashed instance (lock dropped by OS, readable)
```

**Tables** (one redb file, msgpack-encoded values via `rmp-serde`):

| table             | key                    | value (msgpack)                |
| ----------------- | ---------------------- | ------------------------------ |
| `meta`            | `"meta"`               | `{ version, instance, mtime, exit_cursor }` |
| `registers`       | register name (`char`) | `{ text, kind, ts }`           |
| `marks_global`    | mark name (`A`–`Z`)    | `{ path, line, col, ts }`      |
| `marks_numbered`  | digit (`0`–`9`)        | `{ path, line, col, ts }`      |
| `marks_file`      | `(path, name)`         | `{ line, col, ts }`            |
| `changelist_file` | `path`                 | `{ entries: Vec<(line,col)>, ts }` |
| `jumplist`        | `seq` (ordered)        | `{ path, line, col, ts }`      |
| `hist_search`     | `ts` (ordered)         | `{ text }`                     |
| `hist_ex`         | `ts` (ordered)         | `{ text }`                     |

Per-key tables (rather than one blob) are what redb's incremental write buys: a
single yank rewrites one `registers` row in a tiny commit, not the whole store.
`meta.exit_cursor` is what becomes `'0` on the next launch — it is written only
in the final-flush txn at quit, so a crash leaves the previous session's `'0`
intact (vim's own behavior: `'0` tracks *clean* exits).

**Lifecycle**, driven from the server:

1. **Open (startup).** Mint `<host>.<pid>.<rand>.redb`, `Database::create` it
   (acquire the lock). Glob sibling `*.redb`; for each, *try* `Database::open`
   read-only — skip the ones that fail (a live instance holds the lock).
   Recency-merge every readable sibling **plus** my own prior file (if the name
   collides, which it won't with `rand`) into one `PersistState`:
   - registers / marks: group by key, **newest `ts` wins**;
   - history: union all entries, dedup by text keeping newest `ts`, sort, **cap**
     to the `shada`-history limit (default 10 000, neovim's `'1000`-ish but
     configurable later).
   Hand that merged `PersistState` to `editor.import_persist(..)` **before the
   first frame**, the same pre-first-frame slot `init.lua` already uses.
2. **Prune.** Delete the readable sibling files just merged whose `mtime` is
   beyond retention — their surviving entries now live in my in-memory state and
   will land in my own file on first checkpoint. (Compaction by carry-forward;
   never touch a *locked* sibling.)
3. **Run (debounced checkpoint).** Yank / mark-set / history-push set a `dirty`
   flag on the server. ~150 ms after the last change the server calls
   `editor.export_persist()` and hands the value to the **`HostEffects` off-tick
   fs slice** to write the changed tables into **my** redb in one write txn —
   the redb handle and the debounce live in `NativeEffects`, not on the editor
   tick, so the sync core never blocks on disk and the wasm Worker can swap a
   different backend. Single writer to my own file ⇒ no contention, full ACID.
   This is the crash-safety win over neovim: a crash loses at most the last
   debounce window, not the session.
4. **Exit (final flush).** At the `should_quit` point (`lib.rs:819`), a last
   `export_persist` (including `meta.exit_cursor` for `'0`) flushes through the
   same seam, then the `Database` is dropped (releases the lock). My file remains
   a clean checkpoint the next instance will merge.

### Where the file lives under the split topologies

Shada is **editor** state, so it lives wherever the editor core runs:

- **Embedded / local:** `stdpath("state")/shada/` on the local machine.
- **Edit-host split** (`nxvim --daemon` serves remote fs/proc, editor is local):
  **local** — consistent with the standing split-brain rule that editor state,
  plugins, and caches stay local while only *project*-facing fs routes remote.
  The marks' stored paths are remote-project paths, but the store is local.
- **Remote-SSH client** (editor + Lua run on the remote host): on the **remote**
  host's `stdpath("state")`. Several thin clients SSHing into one host spawn
  several `nxvim --server`s sharing that host's state dir — the same per-machine
  multi-writer case, resolved by the same per-instance-file merge.

---

## Build sequence (phased, each independently testable)

1. **Phase 1 — the snapshot seam (core).** Add `persist.rs`: `PersistState` +
   `export_persist`/`import_persist` for **registers and global marks only**.
   No I/O yet. Tested in core's black-box style by round-tripping through a
   second `Editor` (`export_persist` on A → `import_persist` on B → assert
   registers/marks match). Proves the seam without touching disk.
2. **Phase 2 — the redb store (server), single instance.** Add `shada.rs`: open
   one file, write on the debounce + on quit, load on startup. No multi-writer
   merge yet (one file). End-to-end test: harness spawns a server with a temp
   `XDG_STATE_HOME`, feeds `"ayiw` / `mA`, quits, **respawns** against the same
   dir, asserts `"ap` pastes and `` `A `` jumps. (Hermetic via the existing
   temp-dir/`serial_lock` harness helpers.)
3. **Phase 3 — per-file marks + history.** Extend `PersistState` and the tables
   with `marks_file` (`a`–`z`, specials, `` `" `` last-cursor) and the two
   history vecs. Test cross-session `` `" `` reopening a file at its cursor and
   `/`-history recall after respawn.
4. **Phase 4 — jumplist, changelist, numbered marks.** Export the focused
   window's jumplist (ids→paths) + each buffer's changelist; on import seed them
   back by path. Add the `'0`–`'9` shift at load (the new feature): `meta.exit_cursor`
   from the clean-exit txn becomes `'0`, the prior `'0`–`'8` shift down one.
   Test: `<C-o>` walks a restored jumplist after respawn; `` `0 `` lands where
   the last session exited; `` `1 `` where the one before exited.
5. **Phase 5 — the multi-writer merge.** Sibling glob, read-only open of unlocked
   files, recency merge, carry-forward prune. Test: two servers, same temp state
   dir; A yanks `"x` and exits, B yanks `"y` and exits, a third server sees the
   **newest** of each by timestamp; a deliberately lock-held file is skipped
   without error.
6. **Phase 6 — caps, retention, and `:wshada`/`:rshada`.** History caps,
   file-mark count caps (newest-N per neovim), retention-based prune, and the
   explicit `:wshada`/`:rshada[!]` ex-commands (loud, real — they flush/reload
   now, never a no-op).

---

## Open decisions

- **Live cross-instance visibility.** The merge sees a sibling only once it has
  checkpointed (debounce) or exited — neovim's exact contract. If we ever want
  *truly live* sharing (instance A's yank visible in B before A checkpoints),
  the escape hatch is a small atomic-msgpack export read lock-free, or a single
  broker daemon others RPC into (nxvim already has the wire machinery). Deferred:
  shada does not need it, and the broker reintroduces a single point of failure.
- **History cap default.** Start at 10 000 entries per history; expose as a
  `shada`/`'history'`-style option later.
- **Schema evolution.** `meta.version` gates reads; an unknown future version is
  read best-effort (known keys only), never discarded — so a newer instance
  writing the dir doesn't destroy an older instance's ability to read it.

## Testing

All black-box, per the harness convention: spawn a server against a temp
`XDG_STATE_HOME`, drive via `nvim_input`/`feed`, **respawn** against the same
dir, and assert the restored state through `nvim_buf_get_lines` / cursor / `"ap`
paste / `` `A `` jump. The multi-writer phase spawns two servers under
`serial_lock` against one temp dir. No unit tests; no dependence on the real
`~/.local/state`.

## Example config

Per the project convention, ship `examples/shada/` — an `init.lua` that sets a
couple of marks and yanks, plus a short script that launches twice and shows the
second session inheriting the first's registers/marks/history.
