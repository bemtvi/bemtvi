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
>
> **Update 2 (the server targets wasm).** The *server* — not just the core — runs
> in the browser (a wasm Worker; the daemon is the remote fs/proc piece, and
> classic remote-editor-over-SSH is being removed). So persistence must work in
> the browser too, which forces two corrections, now folded in below: (1) the
> store sits behind a **`ShadaStore` seam** the platform injects (native redb-over-file;
> browser redb-over-**OPFS** via redb's `StorageBackend` — *same engine, different
> bytes*), so it is never hardcoded into the server loop; and (2) the per-instance
> files **compact** (carry-forward + delete) on every load, so file count is
> bounded by *concurrent* instances, not total launches. SQLite was considered and
> rejected: it would need a wasm OPFS-VFS in the browser *and* a C dependency on
> native, whereas redb spans both in pure Rust.

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

So "one shared `main.redb` every instance writes" is not available — it would
block or corrupt. The lock is not an obstacle, though; it *is* the merge
boundary:

- Each instance owns **its own** file, `stdpath("state")/shada/<pid>.<nanos>.<seq>.redb`.
  Single-process ⇒ redb's full ACID/crash-safety/incremental-write guarantees
  apply to each writer with zero contention.
- An instance reads every **other** sibling file it can open. A *live* instance
  holds its lock, so its file is skipped (invisible until it checkpoints/exits).
  A *cleanly-exited or crashed* instance released its lock (the OS drops advisory
  locks on process death), so its file is openable and redb's recovery handles any
  uncommitted tail. The reader **recency-merges** those payloads into its in-memory
  editor state.

This reproduces neovim's exact observable contract — *you see another instance's
data once it has written, not while it is live* — while adding crash-safety
neovim lacks (neovim writes shada only on `:q`, losing everything on a crash).
And it needs **no second file format**: the merge reads sibling redb files
directly.

**The same model runs in the browser.** redb is *not* tied to a filesystem — its
[`StorageBackend`] trait abstracts the database to `len`/`read`/`write`/`set_len`/
`sync_data` over bytes, and `Builder::create_with_backend` plugs any
implementation in. The native build uses the default file backend; the wasm Worker
build uses a backend over an **OPFS** sync access handle (`getSize`/`read`/`write`/
`truncate`/`flush` — a 1:1 fit, available synchronously inside a Worker). OPFS
files are exclusively locked too, so the *same* per-instance-file + merge model
covers multiple browser tabs exactly as it covers multiple native processes. One
engine, one design, two byte-backends — which is why the store sits behind a
**`ShadaStore` seam** (`trait ShadaStore { load; flush }`) the platform injects via
[`ServerInit::shada`], the same discipline as `HostFs` / `HostProc` / `HostEffects`.

**Carry-forward compaction (why files don't accumulate).** Minting a new file per
launch *without* cleanup would leave an ever-growing pile and an O(launches)
startup. So `load` doesn't just merge — it **compacts**: after merging the dead
siblings into memory it (1) **flushes the merged snapshot into its own file** so
the absorbed data is durable here, then (2) **deletes the siblings it absorbed**.
The only files surviving a load are this instance's plus any currently-*live*
(locked) ones, so file count is bounded by *concurrent* instances and startup is
O(live instances), not O(history) — a normal single-editor user always has exactly
one file. It is crash-safe by ordering: a crash before the flush-commit leaves the
siblings (re-absorbed next launch); a crash after the commit but before the deletes
leaves redundant copies (likewise harmless). Compaction is payload-agnostic — it
deletes whole absorbed files — so it never changes as the schema grows.

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

A server module, sibling to `save.rs`/`daemon.rs`, implementing the `ShadaStore`
seam. It never holds the `!Send` editor; `load` hands a `PersistState` out,
`flush` takes one in. The native impl is `RedbFileStore` (one redb `Database`
over a file); a wasm impl over an OPFS `StorageBackend` lands in Phase 6.

```
stdpath("state")/shada/
├── 4711.1749.0.redb      ← this instance (locked while live)
├── 4699.1736.0.redb      ← a cleanly-exited instance  ┐ absorbed + deleted on
└── 4702.1740.0.redb      ← a crashed instance          ┘ the next instance's load
```
(`<pid>.<nanos>.<seq>`; only the live instance's file persists across a load —
the rest are merged in and removed by carry-forward compaction.)

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

**Lifecycle**, driven from the server through the `ShadaStore` seam:

1. **`load` (startup) — merge + compact.** Mint `<pid>.<nanos>.<seq>.redb`,
   `Database::create` it (acquire the lock). Glob sibling `*.redb`; for each, *try*
   `Database::open` — skip the ones that fail (a live instance holds the lock),
   record the ones that succeed (dead → absorbable). Recency-merge every absorbed
   sibling into one `PersistState`:
   - registers / marks: group by key, **newest `ts` wins**;
   - history: union all entries, dedup by text keeping newest `ts`, sort, **cap**
     to the `shada`-history limit (default 10 000, neovim's `'1000`-ish but
     configurable later).
   Then **flush the merged snapshot into my own file** (so the absorbed data is
   durable here) and **delete the absorbed siblings**. Return the merged
   `PersistState`; the server hands it to `editor.import_persist(..)` **before the
   first frame**, the same pre-first-frame slot `init.lua` already uses.
2. **Run (debounced checkpoint).** Every handled message re-arms a one-shot ~150 ms
   timer through the `HostEffects` timer seam; when it fires (the run loop's
   `loop_events` arm) the server calls `editor.export_persist()` and hands the value
   to the store's `flush` to write the snapshot into **my** redb in one write txn —
   off the input tick, so the sync editor path never stalls on disk. The checkpoint
   omits `exit_cursor` (that is the exit flush's alone, so `'0` stays clean-exit-only).
   Single writer to my own file ⇒ no contention, full ACID. This is the crash-safety
   win over neovim: a crash loses at most the last debounce window, not the session.
   *(Implemented in Phase 5; Phases 1–4 shipped only the startup + exit flush. The
   write executes in the run loop where the store lives rather than behind a
   `HostEffects` fs-spawn — a native redb commit is fast and the wasm OPFS flush is
   synchronous in a Worker, so neither needs a background task; only the **arming**
   rides `HostEffects`, which is what lets the wasm Worker debounce on its own timer.)*
3. **Exit (final flush).** When the server loop ends — `should_quit` **or** client
   disconnect — a last `export_persist` (including `meta.exit_cursor` for `'0`)
   flushes through the store, then the `Database` is dropped (releases the lock).
   My file remains a clean checkpoint the next instance will absorb.

### Where the store lives under each topology

Shada is **editor** state, so it lives wherever the editor (= the server) runs —
**never on the daemon**:

- **Local:** `stdpath("state")/shada/` on the local machine (redb-over-file).
- **Edit-host split** (`nxvim --daemon` serves remote fs/proc, editor is local):
  **local** — consistent with the standing split-brain rule that editor state,
  plugins, and caches stay local while only *project*-facing fs routes remote. The
  marks' stored paths are remote-project paths, but the store is local.
- **Browser** (the server in a wasm Worker): browser storage (redb-over-OPFS), per
  origin; multiple tabs are multiple instances under the same per-instance-file +
  compaction model.

(The classic remote-editor-over-SSH topology, where the editor ran on the remote
host, is being removed, so there is no "shada on the remote host" case.)

---

## Build sequence (phased, each independently testable)

1. **Phase 1 — the seam, the store, registers, compaction. ✅ DONE.** Core gets
   `persist.rs` (`PersistState` + `export_persist`/`import_persist`, registers
   only). Server gets the `ShadaStore` seam + the native `RedbFileStore`
   (per-instance file, recency-merge of siblings, **carry-forward compaction**,
   load-before-first-frame + flush-on-exit), injected via `ServerInit::shada`.
   Tested end-to-end (`tests/shada.rs`): a register survives a respawn; the store
   compacts to **one file** across sessions with carry-forward intact; `None`
   disables persistence so every other test stays hermetic. *(Because the project
   forbids core unit tests, the seam is exercised through a real respawned server,
   not an in-core round-trip — the original Phase 1/2 split is collapsed into this
   one vertical slice.)*
2. **Phase 2 — global marks. ✅ DONE.** Extends `PersistState` + the
   `marks_global` table with the global file marks `A`–`Z`. Export resolves each
   live mark's `BufferId`→path (and carries through any still-pending restored
   mark); import seeds `Editor::pending_global_marks`, and the first `` `A `` jump
   resolves path→buffer lazily (opening the file) and clamps the restored cursor.
   `:marks` lists a pending mark by its stored path. Respawn test
   (`tests/shada.rs`): `` `A `` reopens the marked file at the saved spot in a
   fresh session that never loaded it at startup. *(The `marks_numbered` table
   lands with the numbered-mark feature in Phase 4, where it is actually
   populated — no unused table is written ahead of its behavior.)*
3. **Phase 3 — per-file marks + history. ✅ DONE.** Adds `marks_file`
   (`a`–`z`, specials, and the `` `" `` last-cursor — a new jumpable mark recorded
   on buffer-leave and stamped from the live cursor at export) keyed by
   `(path, name)`, plus the `hist_search` / `hist_ex` tables (capped at
   `HISTORY_CAP`, recency-merged by a per-flush time-ordered sequence key).
   Restored file marks seed `Editor::pending_file_marks` and reattach to a buffer
   the moment it loads (`seed_pending_file_marks`, hooked into every path-binding
   point) — never eagerly at launch. Respawn tests (`tests/shada.rs`):
   `` `" `` reopens a file at its last cursor, and `/<Up>` recalls a search across
   sessions. The `marks_file` / history tables clear-and-rewrite each flush so a
   dropped mark or trimmed history never resurrects.
4. **Phase 4 — jumplist, changelist, numbered marks. ✅ DONE.** Exports the
   focused window's jumplist (ids→paths, materialized back into live entries on
   the first `<C-o>`/`<C-i>` so launch stays lazy) + each buffer's changelist
   (seeded by path when the file reopens, alongside the marks). Adds the `'0`–`'9`
   shift at load (the unlocked feature): a new `meta` row carries the clean-exit
   cursor (`exit_cursor`), which the store consumes on the next load to become
   `'0`, sliding the prior `'0`–`'8` down one (`'9` drops); with no exit to consume
   (a crash) the set passes through unchanged. `'0`–`'9` are now jumpable marks
   (resolved by path like a restored global mark) and list in `:marks`. Respawn
   tests (`tests/shada.rs`): `<C-o>` walks a restored jumplist; `` `0 `` lands at
   the last exit and `` `1 `` the one before; `g;` walks a restored changelist.
   The jumplist / changelist / numbered-mark tables clear-and-rewrite each flush.
5. **Phase 5 — the debounced live checkpoint. ✅ DONE.** Adds a ~150 ms debounced
   flush *during* the session so a crash loses at most that window, not the whole
   session. Every handled message **re-arms** a one-shot timer (`SHADA_FLUSH_TIMER_ID`,
   reserved above `INTERNAL_WATCH_BASE`) through the `HostEffects` timer seam
   (`loop_command` — so the wasm Worker can drive the same debounce off its own timer
   wheel); the timer wakes the run loop's `loop_events` arm, where the flush executes
   inline next to the exit flush (the store lives in `run()`, owning load-before-first-frame
   and the lock, so the *write* stays there rather than moving behind `HostEffects` —
   only the *arming* needs the seam; a native redb commit is fast and the wasm OPFS
   flush is synchronous in a Worker, so neither blocks). The live checkpoint writes the
   snapshot **without `exit_cursor`** — `'0` tracks *clean* exits only, so a crash leaves
   the prior session's `'0` intact; the exit flush remains the sole writer of the
   clean-exit cursor. Tested (`tests/shada.rs`): a probe `ShadaStore` records a flush
   carrying register `a` *mid-session* (no quit, idle past the debounce), and asserts
   every live checkpoint leaves `exit_cursor` unset. *(`shada: None` arms nothing, so
   every other suite stays untouched.)*
6. **Phase 6 — the OPFS backend (browser).** A second `ShadaStore`/`StorageBackend`
   impl over an OPFS sync access handle, landing with the wasm-Worker server
   (Phase 5 of the edit-host plan). The native `RedbFileStore` is unchanged.
7. **Phase 7 — caps, retention, `:wshada`/`:rshada`.** History caps, file-mark
   count caps (newest-N per neovim), and the explicit `:wshada`/`:rshada[!]`
   ex-commands (loud, real — they flush/reload now, never a no-op). A concurrent
   two-*live*-instance test (both running at once, not just sequential) lands here.

---

## Open decisions

- **SQLite was considered and rejected.** WAL-mode SQLite genuinely supports
  multi-process concurrent writers (one shared file, no per-instance dance), which
  is attractive on native. But the server targets the browser, where SQLite needs
  a wasm OPFS-VFS (wa-sqlite) — and it is a C dependency on native. redb spans both
  in pure Rust via `StorageBackend`, and carry-forward compaction makes the
  per-instance-file cost a non-issue, so redb wins.
- **Live cross-instance visibility.** The merge sees a sibling only once it has
  checkpointed (debounce) or exited — neovim's exact contract. If we ever want
  *truly live* sharing (instance A's yank visible in B before A checkpoints), the
  escape hatch is a single broker the others write through. Deferred: shada does
  not need it, and a broker reintroduces a single point of failure.
- **History cap default.** Start at 10 000 entries per history; expose as a
  `shada`/`'history'`-style option later.
- **Schema evolution.** `meta.version` gates reads; an unknown future version is
  read best-effort (known keys only), never discarded — so a newer instance
  writing the dir doesn't destroy an older instance's ability to read it.

## Testing

All black-box, per the harness convention: inject a `RedbFileStore` over a **temp
dir** through `ServerInit::shada` (never the real `~/.local/state`), drive via
`nvim_input`/`feed`, **respawn** against the same dir, and assert the restored
state through `nvim_buf_get_lines` / cursor / `"ap` paste / `` `A `` jump. The
exit/flush barrier is "drain the client channel until it closes" (the server
returns only after the final flush). Compaction is asserted by counting surviving
`*.redb` files. No unit tests; no dependence on the real state dir.

## Example config

Per the project convention, ship `examples/shada/` — an `init.lua` that sets a
couple of marks and yanks, plus a short script that launches twice and shows the
second session inheriting the first's registers/marks/history.
