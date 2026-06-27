//! The shada persistence store — cross-session editor state.
//!
//! Persistence is an *impure capability*, so it sits behind a seam — the
//! [`ShadaStore`] trait — exactly like [`HostFs`](nxvim_core::HostFs) /
//! [`HostProc`](crate::HostProc). The server core only ever calls `load` / `flush`;
//! the platform layer constructs the concrete store and injects it through
//! [`ServerInit::shada`](crate::ServerInit). That matters because the *server* (not
//! just the core) targets the browser: the native build injects [`RedbFileStore`]
//! (redb over a real file), and the wasm Worker build will inject a redb store over
//! an **OPFS** `StorageBackend` — same engine, different bytes underneath. So the store
//! logic always runs where the editor runs (local disk or browser storage).
//!
//! **Remote shada (Approach A).** A `Remote`-config daemon session is the one case the
//! bytes live *on the daemon*: the store still runs client-side (redb over a local
//! staging dir), but at connect it is seeded from the daemon's copy and after each flush
//! its bytes are uploaded back over the whole-file fs seam (the daemon runs no shada
//! logic — only files cross the wire). See [`prepare_remote_shada`] /
//! [`resolve_session_shada`] and the *Remote shada* section below.
//!
//! **Why per-instance files + merge.** redb is single-process: it takes an
//! exclusive lock on open, so a shared store every instance writes is impossible.
//! Instead each instance owns its own `<pid>.<nanos>.<seq>.redb` and, on startup,
//! **recency-merges** every *other* store it can open (a live instance holds its
//! lock and is skipped; a cleanly exited or crashed one released it). This is
//! neovim's "you see another instance's data once it has written" contract, plus
//! crash-safety neovim lacks.
//!
//! **Why this doesn't accumulate files.** The merge is *carry-forward compaction*:
//! a startup merges the dead siblings into memory, and the **clean-exit flush**
//! then folds that data into this instance's own file and **deletes the siblings it
//! absorbed** — so the only files left behind are this instance's plus any
//! currently-*live* (locked) ones. File count is bounded by concurrent instances,
//! not by total launches, and startup cost is O(live instances), not O(history); a
//! normal single-editor user always ends with exactly one file.
//!
//! **Why the delete waits for exit (not load).** Compaction deletes at the clean
//! exit, not at load. Deleting an absorbed sibling at load would hide its
//! already-written data from any instance that launches while we still hold our own
//! file's lock — for our entire session, breaking neovim's "you see another
//! instance's data once it has written" contract. Deferring the delete keeps the
//! absorbed sibling readable on disk until our exit flush has durably folded its
//! data into our file, shrinking the data-hidden window from the whole session to
//! the teardown instant. It stays crash-safe by ordering: a crash before that final
//! commit leaves the siblings intact (re-absorbed next launch); a crash after it but
//! mid-delete leaves redundant copies (likewise harmless).
//!
//! Phase 1 persists **registers**; Phase 2 the global file marks `A`–`Z`; Phase 3
//! the per-file marks (`a`–`z`, specials, the `"` last-cursor) and search/ex
//! history; Phase 4 the numbered marks `'0`–`'9` (shifted at load), the per-file
//! changelist, the focused window's jumplist, and the `meta` row carrying the
//! clean-exit cursor. Compaction is payload-agnostic (it deletes whole absorbed
//! files) so it does not change. Full design:
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nxvim_core::{
    FileChangelist, FileFolds, FileMarkEntry, GlobalMarkEntry, InputHistoryEntry, JumpPos,
    NumberedMark, PersistState, PluginEntry, PluginNamespace, RegisterEntry,
};
use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};

/// The per-history entry cap (newest-N kept) applied on merge, mirroring vim's
/// `'history'`. A later phase exposes it as an option.
const HISTORY_CAP: usize = 10_000;

/// The persisted schema version, stamped into `meta`. Bumped when the on-disk
/// layout changes; an unknown future version is read best-effort, never discarded.
const SCHEMA_VERSION: u32 = 1;

/// The persistence seam the server drives. `load` is called once before the first
/// frame (it merges readable siblings and returns the snapshot to import); `flush`
/// writes the current snapshot back, and the clean-exit flush also compacts (deletes
/// the absorbed siblings). Both run off the editor hot loop (startup / exit), so
/// a synchronous trait is fine on every platform — native file I/O and an OPFS sync
/// access handle (in a Worker) are both synchronous.
pub trait ShadaStore {
    /// Open this instance's store and recency-merge every readable sibling store
    /// into the returned snapshot. The absorbed siblings are *recorded* for
    /// compaction but **not** deleted here — see [`flush`](ShadaStore::flush)'s
    /// `compact` flag for why the delete waits for a clean exit.
    fn load(&mut self) -> std::io::Result<PersistState>;
    /// Persist `state` into this instance's store. When `compact` is set (only the
    /// clean-exit flush), the siblings absorbed at [`load`](ShadaStore::load) are
    /// deleted *after* this snapshot commits durably — folding their data into our
    /// file and bounding the file count, without hiding their data mid-session.
    fn flush(&mut self, state: &PersistState, compact: bool) -> std::io::Result<()>;
    /// Re-read this instance's own store plus every readable sibling and return the
    /// merged snapshot, **without** minting a file, shifting the numbered marks, or
    /// compacting (the load-only steps). This is the `:rshada` read: it picks up any
    /// sibling that has exited since startup (a still-live one is locked, hence
    /// skipped — neovim's contract) and folds it into the running session. The
    /// numbered marks come through un-shifted (the `'0` shift is a launch event,
    /// not a re-read) and the snapshot carries no `exit_cursor`.
    fn reload(&mut self) -> std::io::Result<PersistState>;

    /// This instance's own store file on disk, once [`load`](ShadaStore::load) has
    /// opened it (`None` before, or for a store with no single backing file). The
    /// remote-shada sync reads these bytes after each flush to upload them to the
    /// daemon — the staged local redb *is* the on-remote artifact (Approach A). The
    /// default returns `None`: a store that isn't a single local file has nothing to
    /// upload whole.
    fn current_path(&self) -> Option<PathBuf> {
        None
    }
}

/// `registers` table: key is the one-char register name, value is a msgpack
/// [`StoredRegister`].
const REGISTERS: TableDefinition<&str, &[u8]> = TableDefinition::new("registers");

/// `marks_global` table: key is the one-char mark name (`A`–`Z`), value is a
/// msgpack [`StoredMark`].
const MARKS_GLOBAL: TableDefinition<&str, &[u8]> = TableDefinition::new("marks_global");

/// `marks_file` table: key is `(path, mark-name)` — the per-file marks `a`–`z`,
/// the specials, and `"`. Value is a msgpack [`StoredFileMark`].
const MARKS_FILE: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("marks_file");

/// `hist_search` / `hist_ex` tables: key is a per-flush time-ordered sequence
/// (so oldest→newest survives and cross-instance recency compares cleanly), value
/// is a msgpack [`StoredHist`]. Rewritten wholesale each flush.
const HIST_SEARCH: TableDefinition<u64, &[u8]> = TableDefinition::new("hist_search");
const HIST_EX: TableDefinition<u64, &[u8]> = TableDefinition::new("hist_ex");

/// `hist_input` table: the per-namespace `nx.ui.input{ history = … }` rings. Key is
/// `(namespace, sequence)` — the same time-ordered sequence as the `:` / `/` tables,
/// scoped per namespace — value a msgpack [`StoredHist`]. Rewritten wholesale each
/// flush, like the other history tables.
const HIST_INPUT: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("hist_input");

/// `marks_numbered` table: key is the digit `0`–`9`, value a msgpack [`StoredMark`].
/// The store *shifts* these at load (`'0` ← last-exit cursor, old `'0`→`'1`, …).
const MARKS_NUMBERED: TableDefinition<&str, &[u8]> = TableDefinition::new("marks_numbered");

/// `changelist_file` table: key is the file path, value a msgpack [`StoredChangelist`].
const CHANGELIST_FILE: TableDefinition<&str, &[u8]> = TableDefinition::new("changelist_file");

/// `folds_file` table: key is the file path, value a msgpack [`StoredFolds`] — a
/// file's persisted **manual** folds (vim `:mkview`-style fold persistence).
const FOLDS_FILE: TableDefinition<&str, &[u8]> = TableDefinition::new("folds_file");

/// `jumplist` table: key is the entry's sequence index (`0`-based, oldest first),
/// value a msgpack [`StoredPos`]. Rewritten wholesale each flush; the newest store's
/// list wins on merge (a jumplist is an ordered sequence, not a union).
const JUMPLIST: TableDefinition<u64, &[u8]> = TableDefinition::new("jumplist");

/// `plugin` table: key is `(namespace, key)` — one opted-in plugin's isolated
/// key/value data (`nx.shada.plugin`). Value is a msgpack [`StoredPlugin`]. Keyed
/// apart from every core table, so a plugin's blob can never reach the registers /
/// marks / history. Same `(&str, &str)` shape and clear-then-rewrite discipline as
/// [`MARKS_FILE`].
const PLUGIN: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("plugin");

/// `meta` table: a single `"meta"` row holding the schema version, this write's
/// timestamp (the jumplist recency key), and the last clean-exit cursor that
/// becomes `'0` next launch.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// `session` table: a single `"v"` row holding the msgpack [`SessionState`] (the
/// open files + tab/split layout for a namespaced workspace). Like the jumplist, the
/// newest store's whole value wins on merge (keyed by `meta.flush_ts`). A separate
/// table — rather than a `StoredMeta` field — so an existing global store's
/// array-encoded `meta` row stays byte-compatible; old stores simply lack the table.
const SESSION: TableDefinition<&str, &[u8]> = TableDefinition::new("session");

/// `workspace_options` table: a single `"v"` row holding the msgpack workspace option
/// overlay ([`nxvim_core::options::WorkspaceOptions`] — the `nx.wso` per-workspace global
/// overrides). Like [`SESSION`], the newest store's whole value wins on merge (keyed by
/// `meta.flush_ts`), and a separate table keeps old stores byte-compatible (they lack it).
const WORKSPACE_OPTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("workspace_options");

/// A register as stored on disk: its contents, paste kind, and the write
/// timestamp that drives the cross-instance recency merge.
#[derive(Serialize, Deserialize)]
struct StoredRegister {
    text: String,
    linewise: bool,
    ts: u64,
}

/// A global mark as stored on disk: the file path it points into, the 0-based
/// `(line, col)`, and the write timestamp driving the recency merge.
#[derive(Serialize, Deserialize)]
struct StoredMark {
    path: String,
    line: usize,
    col: usize,
    ts: u64,
}

/// A per-file mark as stored on disk: the 0-based `(line, col)` in its file and
/// the write timestamp. The file path and mark name live in the table key.
#[derive(Serialize, Deserialize)]
struct StoredFileMark {
    line: usize,
    col: usize,
    ts: u64,
}

/// One history entry as stored on disk: just its text (order and recency are
/// carried by the table key, a per-flush time-ordered sequence).
#[derive(Serialize, Deserialize)]
struct StoredHist {
    text: String,
}

/// One plugin key/value pair as stored on disk: the plugin's serialized blob and
/// the write timestamp driving the cross-instance recency merge. The namespace and
/// key live in the `(namespace, key)` table key.
#[derive(Serialize, Deserialize)]
struct StoredPlugin {
    value: String,
    ts: u64,
}

/// A bare file position as stored on disk — a jumplist entry or the exit cursor.
#[derive(Serialize, Deserialize, Clone)]
struct StoredPos {
    path: String,
    line: usize,
    col: usize,
}

/// A per-file changelist as stored on disk: its `(line, col)` change positions
/// (oldest first) and the write timestamp.
#[derive(Serialize, Deserialize)]
struct StoredChangelist {
    entries: Vec<(usize, usize)>,
    ts: u64,
}

/// A per-file manual-fold set as stored on disk: each fold's `(start, end, closed)`
/// (outer-before-inner) and the write timestamp (newest per file wins on merge).
#[derive(Serialize, Deserialize)]
struct StoredFolds {
    folds: Vec<(usize, usize, bool)>,
    ts: u64,
}

/// The single `meta` row: schema version, this write's timestamp (jumplist recency
/// key), and the last clean-exit cursor (`None` after the store consumes it into
/// `'0`). `exit_ts` recency-orders the exit cursor across sibling stores.
#[derive(Serialize, Deserialize)]
struct StoredMeta {
    version: u32,
    flush_ts: u64,
    exit: Option<StoredPos>,
    exit_ts: u64,
}

/// The native (and, via a custom `StorageBackend`, browser) shada store: a
/// per-instance redb database in a shared directory, with carry-forward
/// compaction of sibling stores.
pub struct RedbFileStore {
    dir: PathBuf,
    /// This instance's database, opened lazily by [`load`](RedbFileStore::load) and
    /// reused by [`flush`](RedbFileStore::flush) / [`reload`](RedbFileStore::reload).
    db: Option<Database>,
    /// This instance's own store path, set at [`load`](RedbFileStore::load) so
    /// [`reload`](RedbFileStore::reload) can exclude it from the sibling glob (its
    /// data is read through the live `db` handle, never re-opened — redb's exclusive
    /// lock forbids a second open of our own file).
    path: Option<PathBuf>,
    /// The sibling stores absorbed at [`load`](RedbFileStore::load), pending deletion
    /// by the compacting [`flush`](RedbFileStore::flush) on clean exit. Empty between
    /// launches with no dead siblings, and drained once compacted.
    absorbed: Vec<PathBuf>,
}

impl RedbFileStore {
    /// A store living under `dir` (`stdpath("state")/shada` for the real binary, a
    /// temp dir for tests). No I/O happens until [`load`](RedbFileStore::load).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            db: None,
            path: None,
            absorbed: Vec::new(),
        }
    }
}

impl ShadaStore for RedbFileStore {
    fn load(&mut self) -> std::io::Result<PersistState> {
        std::fs::create_dir_all(&self.dir)?;
        let my_path = self.dir.join(instance_filename());
        let db = Database::create(&my_path).map_err(std::io::Error::other)?;

        // Open every sibling we can (a live instance holds the lock → `open` fails →
        // skip it, its data simply not visible yet — neovim's contract). Keep the
        // handles alive so we can read them, and the paths so the *clean-exit* flush
        // can delete the dead ones once our merged snapshot is durable.
        //
        // We deliberately do NOT delete (compact) here. Deleting an absorbed sibling
        // at load would hide its already-written data from any instance that launches
        // while we hold our own file's lock — for our whole session, breaking
        // neovim's "you see another instance's data once it has written" contract.
        // Deferring the delete to the exit flush (see `flush(.., compact = true)`)
        // shrinks that data-hidden window to the teardown instant.
        let mut siblings: Vec<Database> = Vec::new();
        let mut absorbed: Vec<PathBuf> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&self.dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path == my_path || path.extension().and_then(|e| e.to_str()) != Some("redb") {
                    continue;
                }
                if let Ok(sibling) = Database::open(&path) {
                    siblings.push(sibling);
                    absorbed.push(path);
                }
            }
        }

        let mut merged = collect_merge(siblings.iter());
        // The numbered-mark shift: a consumed clean-exit cursor becomes `'0` and the
        // prior `'0`–`'8` slide down one (`'9` drops). With no exit to consume the
        // set passes through unchanged (e.g. after a crash, so `'0` stays put). This
        // is the one load-only step — a `:rshada` re-read never re-shifts.
        let numbered_marks =
            shift_numbered_marks(std::mem::take(&mut merged.numbered), merged.exit.1.take());
        let state = build_state(merged, numbered_marks);

        // Release the sibling handles: we keep no claim on them between now and the
        // exit-time compaction (deletion is by recorded path), and another instance
        // must stay free to open them while we run. The absorbed data is durable in
        // *their* files until our exit flush folds it into ours, so there is no
        // window where it lives nowhere. (No carry-forward write at load: the data
        // stays in the siblings, and our own snapshot is written by the first
        // checkpoint / the exit flush.)
        drop(siblings);

        self.path = Some(my_path);
        self.db = Some(db);
        self.absorbed = absorbed;
        Ok(state)
    }

    fn reload(&mut self) -> std::io::Result<PersistState> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shada reload before load"))?;
        let my_path = self.path.as_deref();

        // Open every readable sibling (a still-live one is locked, hence skipped),
        // but do NOT record them for deletion — a re-read is non-destructive;
        // compaction stays a load-time concern.
        let mut siblings: Vec<Database> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&self.dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if Some(path.as_path()) == my_path
                    || path.extension().and_then(|e| e.to_str()) != Some("redb")
                {
                    continue;
                }
                if let Ok(sibling) = Database::open(&path) {
                    siblings.push(sibling);
                }
            }
        }

        // Merge our own live store (read through the existing handle — redb forbids a
        // second open of it) together with the readable siblings. No shift, no exit
        // consumption: the numbered marks come through as stored.
        let mut merged = collect_merge(std::iter::once(db).chain(siblings.iter()));
        let numbered_marks = numbered_passthrough(std::mem::take(&mut merged.numbered));
        Ok(build_state(merged, numbered_marks))
    }

    fn flush(&mut self, state: &PersistState, compact: bool) -> std::io::Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shada flush before load"))?;
        write_state(db, state)?;
        // Compaction (only on a clean exit): our snapshot — which folds in everything
        // absorbed at load — is now durable, so the absorbed siblings are safe to
        // delete. A crash before this commit leaves them intact (re-absorbed next
        // launch); a crash after it but mid-delete leaves redundant copies (likewise
        // harmless). Per-file errors are ignored — a sibling another live instance
        // still holds open (Windows) must not fail the flush.
        if compact {
            for path in std::mem::take(&mut self.absorbed) {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    fn current_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }
}

/// Write a snapshot into `db` in one transaction, stamping each row with the
/// current time (the merge key a later instance reads). Shared by the startup
/// carry-forward flush and the exit flush.
fn write_state(db: &Database, state: &PersistState) -> std::io::Result<()> {
    let ts = now_ms();
    let wtxn = db.begin_write().map_err(std::io::Error::other)?;
    {
        let mut table = wtxn.open_table(REGISTERS).map_err(std::io::Error::other)?;
        for entry in &state.registers {
            let stored = StoredRegister {
                text: entry.text.clone(),
                linewise: entry.linewise,
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let key = entry.name.to_string();
            table
                .insert(key.as_str(), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        let mut table = wtxn
            .open_table(MARKS_GLOBAL)
            .map_err(std::io::Error::other)?;
        for entry in &state.global_marks {
            let stored = StoredMark {
                path: entry.path.to_string_lossy().into_owned(),
                line: entry.line,
                col: entry.col,
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let key = entry.name.to_string();
            table
                .insert(key.as_str(), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // Cleared before rewrite: per-file marks key on `(path, name)`, so a mark
        // dropped this session (its line deleted) must not linger and resurrect.
        let mut table = wtxn.open_table(MARKS_FILE).map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for entry in &state.file_marks {
            let stored = StoredFileMark {
                line: entry.line,
                col: entry.col,
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let path = entry.path.to_string_lossy().into_owned();
            let name = entry.name.to_string();
            table
                .insert((path.as_str(), name.as_str()), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // Numbered marks key on the digit (stable), but a shorter set must not
        // leave stale higher digits behind — clear before rewrite.
        let mut table = wtxn
            .open_table(MARKS_NUMBERED)
            .map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for mark in &state.numbered_marks {
            let stored = StoredMark {
                path: mark.path.to_string_lossy().into_owned(),
                line: mark.line,
                col: mark.col,
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let key = mark.digit.to_string();
            table
                .insert(key.as_str(), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // Changelists key on path; clear so a file whose list emptied drops out.
        let mut table = wtxn
            .open_table(CHANGELIST_FILE)
            .map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for cl in &state.file_changelists {
            let stored = StoredChangelist {
                entries: cl.entries.clone(),
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let path = cl.path.to_string_lossy().into_owned();
            table
                .insert(path.as_str(), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // Manual folds key on path; clear so a file whose folds were all deleted
        // drops out (mirrors the changelist table).
        let mut table = wtxn.open_table(FOLDS_FILE).map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for ff in &state.file_folds {
            let stored = StoredFolds {
                folds: ff.folds.clone(),
                ts,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            let path = ff.path.to_string_lossy().into_owned();
            table
                .insert(path.as_str(), bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // Plugin data keys on `(namespace, key)`; clear before rewrite so a key a
        // plugin deleted this session drops out (mirrors the per-file marks).
        let mut table = wtxn.open_table(PLUGIN).map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for ns in &state.plugin_data {
            for entry in &ns.entries {
                let stored = StoredPlugin {
                    value: entry.value.clone(),
                    ts,
                };
                let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
                table
                    .insert(
                        (ns.namespace.as_str(), entry.key.as_str()),
                        bytes.as_slice(),
                    )
                    .map_err(std::io::Error::other)?;
            }
        }
    }
    {
        // The jumplist is rewritten wholesale (keys 0..N), so clear first lest a
        // now-shorter list leave stale tail rows.
        let mut table = wtxn.open_table(JUMPLIST).map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        for (i, j) in state.jumplist.iter().enumerate() {
            let stored = StoredPos {
                path: j.path.to_string_lossy().into_owned(),
                line: j.line,
                col: j.col,
            };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            table
                .insert(i as u64, bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        let mut table = wtxn.open_table(META).map_err(std::io::Error::other)?;
        let meta = StoredMeta {
            version: SCHEMA_VERSION,
            flush_ts: ts,
            exit: state.exit_cursor.as_ref().map(|c| StoredPos {
                path: c.path.to_string_lossy().into_owned(),
                line: c.line,
                col: c.col,
            }),
            exit_ts: if state.exit_cursor.is_some() { ts } else { 0 },
        };
        let bytes = rmp_serde::to_vec(&meta).map_err(std::io::Error::other)?;
        table
            .insert("meta", bytes.as_slice())
            .map_err(std::io::Error::other)?;
    }
    {
        // The session is a whole-value record (newest store wins, keyed by flush_ts):
        // rewrite the single row, or clear it when there's nothing to save.
        let mut table = wtxn.open_table(SESSION).map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        if let Some(session) = &state.session {
            let bytes = rmp_serde::to_vec(session).map_err(std::io::Error::other)?;
            table
                .insert("v", bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    {
        // The workspace option overlay is a whole-value record too (newest store wins):
        // rewrite the single row, or clear it when there are no overrides.
        let mut table = wtxn
            .open_table(WORKSPACE_OPTIONS)
            .map_err(std::io::Error::other)?;
        table.retain(|_, _| false).map_err(std::io::Error::other)?;
        if !state.workspace_options.is_empty() {
            let bytes =
                rmp_serde::to_vec(&state.workspace_options).map_err(std::io::Error::other)?;
            table
                .insert("v", bytes.as_slice())
                .map_err(std::io::Error::other)?;
        }
    }
    write_history(&wtxn, HIST_SEARCH, &state.search_history, ts)?;
    write_history(&wtxn, HIST_EX, &state.ex_history, ts)?;
    write_input_history(&wtxn, &state.input_history, ts)?;
    wtxn.commit().map_err(std::io::Error::other)?;
    Ok(())
}

/// Rewrite the per-namespace `nx.ui.input` history table wholesale: clear it, then
/// re-key each namespace's entries by the same time-ordered sequence `write_history`
/// uses (so cross-instance recency compares cleanly), scoped under the namespace.
fn write_input_history(
    wtxn: &redb::WriteTransaction,
    input: &[InputHistoryEntry],
    ts: u64,
) -> std::io::Result<()> {
    let mut table = wtxn.open_table(HIST_INPUT).map_err(std::io::Error::other)?;
    table.retain(|_, _| false).map_err(std::io::Error::other)?;
    let base = ts.saturating_mul(HISTORY_CAP as u64);
    for entry in input {
        for (i, text) in entry.entries.iter().enumerate() {
            let stored = StoredHist { text: text.clone() };
            let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
            table
                .insert(
                    (entry.namespace.as_str(), base + i as u64),
                    bytes.as_slice(),
                )
                .map_err(std::io::Error::other)?;
        }
    }
    Ok(())
}

/// Rewrite one history table wholesale: clear it, then re-key each entry by a
/// time-ordered sequence (`ts` scaled, plus the entry's index) so the row order is
/// oldest→newest *and* a later instance's rows sort after an earlier one's. Clearing
/// avoids stale rows accumulating, since every flush re-mints fresh keys.
fn write_history(
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<u64, &[u8]>,
    entries: &[String],
    ts: u64,
) -> std::io::Result<()> {
    let mut table = wtxn.open_table(def).map_err(std::io::Error::other)?;
    table.retain(|_, _| false).map_err(std::io::Error::other)?;
    let base = ts.saturating_mul(HISTORY_CAP as u64);
    for (i, text) in entries.iter().enumerate() {
        let stored = StoredHist { text: text.clone() };
        let bytes = rmp_serde::to_vec(&stored).map_err(std::io::Error::other)?;
        table
            .insert(base + i as u64, bytes.as_slice())
            .map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// The raw recency-merge of one or more stores, before the numbered-mark shift and
/// the projection into a [`PersistState`]. Shared by [`RedbFileStore::load`] (which
/// shifts the numbered marks) and [`RedbFileStore::reload`] (which doesn't), so the
/// fold logic lives in exactly one place.
#[derive(Default)]
struct MergedRaw {
    regs: std::collections::HashMap<char, StoredRegister>,
    global_marks: std::collections::HashMap<char, StoredMark>,
    file_marks: std::collections::HashMap<(String, char), StoredFileMark>,
    numbered: std::collections::HashMap<char, StoredMark>,
    changelist: std::collections::HashMap<String, StoredChangelist>,
    folds: std::collections::HashMap<String, StoredFolds>,
    /// Per-plugin isolated data, keyed `(namespace, key)` — newest `ts` per pair
    /// wins, exactly like the file marks.
    plugin: std::collections::HashMap<(String, String), StoredPlugin>,
    hist_search: HistMerge,
    hist_ex: HistMerge,
    /// The per-namespace `nx.ui.input` history rings: one [`HistMerge`] per namespace,
    /// each folded independently across sibling stores like `hist_search` / `hist_ex`.
    hist_input: std::collections::HashMap<String, HistMerge>,
    /// The jumplist is an ordered sequence, not a union: the newest store's whole
    /// list wins (keyed by its `meta.flush_ts`).
    jumplist: (u64, Vec<StoredPos>),
    /// The newest *clean* exit cursor across the stores (keyed by `meta.exit_ts`).
    exit: (u64, Option<StoredPos>),
    /// The workspace session (open files + layout): newest store's whole value wins,
    /// keyed by `meta.flush_ts` like the jumplist. `None` until a store carries one.
    session: (u64, Option<nxvim_core::SessionState>),
    /// The per-workspace option overlay (`nx.wso`): newest store's whole value wins,
    /// keyed by `meta.flush_ts` like the session. Empty until a store carries one.
    workspace_options: (u64, nxvim_core::options::WorkspaceOptions),
}

/// Recency-merge a set of opened stores into one [`MergedRaw`]: per-key newest-`ts`
/// wins for registers / marks / changelists, history unions by text keeping the
/// newest sequence, and the jumplist / exit-cursor take the newest store's whole
/// value. Order-independent, so the caller can pass siblings alone (load) or its own
/// store plus siblings (reload).
fn collect_merge<'a>(dbs: impl IntoIterator<Item = &'a Database>) -> MergedRaw {
    let mut m = MergedRaw::default();
    for db in dbs {
        merge_registers(db, &mut m.regs);
        merge_global_marks(db, &mut m.global_marks);
        merge_file_marks(db, &mut m.file_marks);
        merge_numbered_marks(db, &mut m.numbered);
        merge_changelists(db, &mut m.changelist);
        merge_folds(db, &mut m.folds);
        merge_plugin(db, &mut m.plugin);
        merge_history(db, HIST_SEARCH, &mut m.hist_search);
        merge_history(db, HIST_EX, &mut m.hist_ex);
        merge_input_history(db, &mut m.hist_input);
        if let Some(meta) = read_meta(db) {
            if meta.flush_ts > m.jumplist.0 {
                m.jumplist = (meta.flush_ts, read_jumplist(db));
            }
            if meta.flush_ts > m.session.0 {
                m.session = (meta.flush_ts, read_session(db));
            }
            if meta.flush_ts > m.workspace_options.0 {
                m.workspace_options = (meta.flush_ts, read_workspace_options(db));
            }
            if let Some(exit) = meta.exit {
                if meta.exit_ts > m.exit.0 {
                    m.exit = (meta.exit_ts, Some(exit));
                }
            }
        }
    }
    m
}

/// Read one store's `session` row (the msgpack [`SessionState`] blob), or `None` when
/// the table is absent (old store) / empty / undecodable.
fn read_session(db: &Database) -> Option<nxvim_core::SessionState> {
    let rtxn = db.begin_read().ok()?;
    let table = match rtxn.open_table(SESSION) {
        Ok(t) => t,
        Err(TableError::TableDoesNotExist(_)) => return None,
        Err(_) => return None,
    };
    let bytes = table.get("v").ok()??;
    rmp_serde::from_slice(bytes.value()).ok()
}

/// Read one store's `workspace_options` row (the msgpack overlay blob), or empty when the
/// table is absent (old store) / empty / undecodable.
fn read_workspace_options(db: &Database) -> nxvim_core::options::WorkspaceOptions {
    let read = || -> Option<nxvim_core::options::WorkspaceOptions> {
        let rtxn = db.begin_read().ok()?;
        let table = match rtxn.open_table(WORKSPACE_OPTIONS) {
            Ok(t) => t,
            Err(_) => return None,
        };
        let bytes = table.get("v").ok()??;
        rmp_serde::from_slice(bytes.value()).ok()
    };
    read().unwrap_or_default()
}

/// Project a [`MergedRaw`] (its numbered marks already resolved to `numbered_marks`,
/// shifted or not by the caller) into the [`PersistState`] the editor imports. The
/// snapshot never carries an `exit_cursor`: load consumes it into `'0` via the
/// shift, and a `:rshada` re-read ignores it.
fn build_state(m: MergedRaw, numbered_marks: Vec<NumberedMark>) -> PersistState {
    PersistState {
        registers: m
            .regs
            .into_iter()
            .map(|(name, stored)| RegisterEntry {
                name,
                text: stored.text,
                linewise: stored.linewise,
            })
            .collect(),
        global_marks: m
            .global_marks
            .into_iter()
            .map(|(name, stored)| GlobalMarkEntry {
                name,
                path: stored.path.into(),
                line: stored.line,
                col: stored.col,
            })
            .collect(),
        file_marks: m
            .file_marks
            .into_iter()
            .map(|((path, name), stored)| FileMarkEntry {
                path: path.into(),
                name,
                line: stored.line,
                col: stored.col,
            })
            .collect(),
        search_history: m.hist_search.finish(),
        ex_history: m.hist_ex.finish(),
        input_history: group_input_history(m.hist_input),
        numbered_marks,
        file_changelists: m
            .changelist
            .into_iter()
            .map(|(path, stored)| FileChangelist {
                path: path.into(),
                entries: stored.entries,
            })
            .collect(),
        file_folds: m
            .folds
            .into_iter()
            .map(|(path, stored)| FileFolds {
                path: path.into(),
                folds: stored.folds,
            })
            .collect(),
        jumplist: m
            .jumplist
            .1
            .into_iter()
            .map(|p| JumpPos {
                path: p.path.into(),
                line: p.line,
                col: p.col,
            })
            .collect(),
        exit_cursor: None,
        // The restored workspace session (newest store wins); the server only acts on
        // it when a workspace namespace is active.
        session: m.session.1,
        plugin_data: group_plugin(m.plugin),
        // The restored per-workspace option overlay (newest store wins); the editor
        // re-applies it at import (`seed`/`apply_persist`).
        workspace_options: m.workspace_options.1,
    }
}

/// Group the merged `(namespace, key) -> StoredPlugin` map back into one
/// [`PluginNamespace`] per namespace, each carrying its key→value entries. The
/// namespaces and the keys within each are sorted, so the seeded Lua state is
/// deterministic across runs (a redb iteration order isn't guaranteed stable).
fn group_plugin(
    map: std::collections::HashMap<(String, String), StoredPlugin>,
) -> Vec<PluginNamespace> {
    let mut by_ns: std::collections::BTreeMap<String, Vec<PluginEntry>> =
        std::collections::BTreeMap::new();
    for ((namespace, key), stored) in map {
        by_ns.entry(namespace).or_default().push(PluginEntry {
            key,
            value: stored.value,
        });
    }
    by_ns
        .into_iter()
        .map(|(namespace, mut entries)| {
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            PluginNamespace { namespace, entries }
        })
        .collect()
}

/// Project the per-namespace input-history merge map into the ordered, capped rings the
/// editor imports — one [`InputHistoryEntry`] per namespace, namespaces sorted so the
/// seeded state is deterministic (a `HashMap` iteration order isn't).
fn group_input_history(
    map: std::collections::HashMap<String, HistMerge>,
) -> Vec<InputHistoryEntry> {
    let mut by_ns: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (namespace, merge) in map {
        by_ns.insert(namespace, merge.finish());
    }
    by_ns
        .into_iter()
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(namespace, entries)| InputHistoryEntry { namespace, entries })
        .collect()
}

/// The merged numbered marks passed through *unchanged* — the `:rshada` re-read
/// path, where the `'0` shift (a launch-only event) must not re-run.
fn numbered_passthrough(marks: std::collections::HashMap<char, StoredMark>) -> Vec<NumberedMark> {
    marks
        .into_iter()
        .map(|(digit, m)| NumberedMark {
            digit,
            path: m.path.into(),
            line: m.line,
            col: m.col,
        })
        .collect()
}

/// Fold one store's `registers` table into the running best-by-timestamp map.
fn merge_registers(db: &Database, best: &mut std::collections::HashMap<char, StoredRegister>) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(REGISTERS) {
        Ok(table) => table,
        // A store written before any register existed has no table — not an error.
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let Some(name) = key.value().chars().next() else {
            continue;
        };
        let Ok(stored) = rmp_serde::from_slice::<StoredRegister>(value.value()) else {
            continue;
        };
        match best.get(&name) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(name, stored);
            }
        }
    }
}

/// Fold one store's `marks_global` table into the running best-by-timestamp map.
/// Identical recency discipline to [`merge_registers`]: newest `ts` per mark name
/// wins; a store predating any global mark has no table, which is not an error.
fn merge_global_marks(db: &Database, best: &mut std::collections::HashMap<char, StoredMark>) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(MARKS_GLOBAL) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let Some(name) = key.value().chars().next() else {
            continue;
        };
        let Ok(stored) = rmp_serde::from_slice::<StoredMark>(value.value()) else {
            continue;
        };
        match best.get(&name) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(name, stored);
            }
        }
    }
}

/// Fold one store's `marks_file` table into the running best-by-timestamp map,
/// keyed `(path, mark-name)`. Same recency discipline as the other marks.
fn merge_file_marks(
    db: &Database,
    best: &mut std::collections::HashMap<(String, char), StoredFileMark>,
) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(MARKS_FILE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let (path, name_str) = key.value();
        let Some(name) = name_str.chars().next() else {
            continue;
        };
        let Ok(stored) = rmp_serde::from_slice::<StoredFileMark>(value.value()) else {
            continue;
        };
        let composite = (path.to_string(), name);
        match best.get(&composite) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(composite, stored);
            }
        }
    }
}

/// Accumulates history entries across sibling stores: text → the highest sequence
/// key seen for it (most recent), so a duplicate keeps its newest position. The
/// final ordered, capped list is produced by [`HistMerge::finish`].
#[derive(Default)]
struct HistMerge {
    by_text: std::collections::HashMap<String, u64>,
}

impl HistMerge {
    /// Record `text` at sequence `key`, keeping the newest occurrence.
    fn observe(&mut self, text: String, key: u64) {
        let slot = self.by_text.entry(text).or_insert(key);
        *slot = (*slot).max(key);
    }

    /// The merged history oldest→newest, capped to the newest [`HISTORY_CAP`].
    fn finish(self) -> Vec<String> {
        let mut entries: Vec<(u64, String)> =
            self.by_text.into_iter().map(|(t, k)| (k, t)).collect();
        entries.sort_by_key(|(k, _)| *k);
        if entries.len() > HISTORY_CAP {
            entries.drain(0..entries.len() - HISTORY_CAP);
        }
        entries.into_iter().map(|(_, t)| t).collect()
    }
}

/// Fold one store's history table (`hist_search` or `hist_ex`) into `merge`,
/// preserving each entry's sequence key as its recency.
fn merge_history(db: &Database, def: TableDefinition<u64, &[u8]>, merge: &mut HistMerge) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(def) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let Ok(stored) = rmp_serde::from_slice::<StoredHist>(value.value()) else {
            continue;
        };
        merge.observe(stored.text, key.value());
    }
}

/// Fold one store's `hist_input` table into the per-namespace merge map, preserving
/// each entry's sequence key as its recency — one [`HistMerge`] per namespace, exactly
/// like [`merge_history`] but split by the key's namespace component.
fn merge_input_history(db: &Database, merge: &mut std::collections::HashMap<String, HistMerge>) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(HIST_INPUT) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let (namespace, seq) = key.value();
        let Ok(stored) = rmp_serde::from_slice::<StoredHist>(value.value()) else {
            continue;
        };
        merge
            .entry(namespace.to_string())
            .or_default()
            .observe(stored.text, seq);
    }
}

/// Fold one store's `marks_numbered` table into the best-by-timestamp map, keyed by
/// the digit `'0'`–`'9'`. Same recency discipline as the other marks.
fn merge_numbered_marks(db: &Database, best: &mut std::collections::HashMap<char, StoredMark>) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(MARKS_NUMBERED) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let Some(digit) = key.value().chars().next() else {
            continue;
        };
        let Ok(stored) = rmp_serde::from_slice::<StoredMark>(value.value()) else {
            continue;
        };
        match best.get(&digit) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(digit, stored);
            }
        }
    }
}

/// Fold one store's `changelist_file` table into the best-by-timestamp map, keyed
/// by path (newest `ts` per file wins — a changelist persists as one row).
fn merge_changelists(
    db: &Database,
    best: &mut std::collections::HashMap<String, StoredChangelist>,
) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(CHANGELIST_FILE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let path = key.value().to_string();
        let Ok(stored) = rmp_serde::from_slice::<StoredChangelist>(value.value()) else {
            continue;
        };
        match best.get(&path) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(path, stored);
            }
        }
    }
}

/// Fold one store's `folds_file` table into the best-by-timestamp map, keyed by
/// path (newest `ts` per file wins — a file's manual folds persist as one row,
/// like its changelist).
fn merge_folds(db: &Database, best: &mut std::collections::HashMap<String, StoredFolds>) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(FOLDS_FILE) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let path = key.value().to_string();
        let Ok(stored) = rmp_serde::from_slice::<StoredFolds>(value.value()) else {
            continue;
        };
        match best.get(&path) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(path, stored);
            }
        }
    }
}

/// Fold one store's `plugin` table into the best-by-timestamp map, keyed
/// `(namespace, key)` (newest `ts` per pair wins). Same recency discipline as the
/// file marks; a store predating any opted-in plugin has no table (not an error).
fn merge_plugin(
    db: &Database,
    best: &mut std::collections::HashMap<(String, String), StoredPlugin>,
) {
    let Ok(rtxn) = db.begin_read() else {
        return;
    };
    let table = match rtxn.open_table(PLUGIN) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return,
        Err(_) => return,
    };
    let Ok(iter) = table.iter() else {
        return;
    };
    for row in iter.flatten() {
        let (key, value) = row;
        let (namespace, name) = key.value();
        let Ok(stored) = rmp_serde::from_slice::<StoredPlugin>(value.value()) else {
            continue;
        };
        let composite = (namespace.to_string(), name.to_string());
        match best.get(&composite) {
            Some(existing) if existing.ts >= stored.ts => {}
            _ => {
                best.insert(composite, stored);
            }
        }
    }
}

/// Read one store's `meta` row, or `None` if it has none (a store written before
/// the meta table existed, or unreadable).
fn read_meta(db: &Database) -> Option<StoredMeta> {
    let rtxn = db.begin_read().ok()?;
    let table = rtxn.open_table(META).ok()?;
    let row = table.get("meta").ok()??;
    rmp_serde::from_slice::<StoredMeta>(row.value()).ok()
}

/// Read one store's `jumplist` rows in sequence order (the table key is the
/// 0-based index, and redb iterates a `u64` key ascending).
fn read_jumplist(db: &Database) -> Vec<StoredPos> {
    let Ok(rtxn) = db.begin_read() else {
        return Vec::new();
    };
    let table = match rtxn.open_table(JUMPLIST) {
        Ok(table) => table,
        Err(_) => return Vec::new(),
    };
    let Ok(iter) = table.iter() else {
        return Vec::new();
    };
    iter.flatten()
        .filter_map(|(_, value)| rmp_serde::from_slice::<StoredPos>(value.value()).ok())
        .collect()
}

/// Apply vim's numbered-mark shift to the merged set. A consumed clean-exit cursor
/// becomes `'0`, the prior `'0`–`'8` slide down one, and `'9` drops; with no exit
/// (a crash left none) the set passes through unchanged. The result is the ten — or
/// fewer — `(digit, path, line, col)` marks the editor seeds.
fn shift_numbered_marks(
    old: std::collections::HashMap<char, StoredMark>,
    exit: Option<StoredPos>,
) -> Vec<NumberedMark> {
    let to_entry = |digit: char, m: &StoredMark| NumberedMark {
        digit,
        path: m.path.clone().into(),
        line: m.line,
        col: m.col,
    };
    let Some(exit) = exit else {
        return old.iter().map(|(&d, m)| to_entry(d, m)).collect();
    };
    let mut out = vec![NumberedMark {
        digit: '0',
        path: exit.path.into(),
        line: exit.line,
        col: exit.col,
    }];
    for n in 1u8..=9 {
        let from = (b'0' + n - 1) as char;
        if let Some(m) = old.get(&from) {
            out.push(to_entry((b'0' + n) as char, m));
        }
    }
    out
}

/// A monotonic per-process counter so two instances minted in the same process and
/// nanosecond (e.g. two sessions in one test binary) never collide on a filename.
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// This instance's store filename: `pid`, a high-resolution timestamp, and a
/// process-local sequence — unique across every instance that may share a dir.
fn instance_filename() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{}.{}.{}.redb", std::process::id(), nanos, seq)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The default shada directory for the real binary: `stdpath("state")/shada`,
/// i.e. `$XDG_STATE_HOME/nxvim/shada` (or `$HOME/.local/state/nxvim/shada`). Tests
/// build a [`RedbFileStore`] over a temp dir instead, so they never touch this one.
pub fn shada_dir() -> PathBuf {
    let base = if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state")
    } else {
        PathBuf::from(".local/state")
    };
    base.join("nxvim").join("shada")
}

/// The default store for the native binaries: a [`RedbFileStore`] under
/// [`shada_dir`]. The wasm Worker build injects a redb-over-OPFS store here instead.
pub fn default_shada() -> Box<dyn ShadaStore + Send> {
    Box::new(RedbFileStore::new(shada_dir()))
}

/// A namespace becomes a single path component under `shada_dir()/ns/`, so it must be a
/// safe, traversal-free token. Accept only `[A-Za-z0-9_-]` (a v4 UUID and friends),
/// bounded in length; anything else is rejected. This guards the `--shada-namespace`
/// value against smuggling `../…` into the store path. Returns the validated namespace.
pub fn valid_namespace(ns: &str) -> Option<String> {
    if ns.is_empty() || ns.len() > 128 {
        return None;
    }
    if ns
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(ns.to_string())
    } else {
        None
    }
}

/// Derive a stable shada namespace from a workspace directory (the `--workspace` flag):
/// the **complete** absolute path with every character outside `[A-Za-z0-9_]` — path
/// separators included — folded to `-`, so `/home/ada/proj` becomes `-home-ada-proj`. The
/// mapping is lossy-but-reversible-by-eye and never truncated or hashed: the namespace dir
/// (`ns/<this>`) reads as the path it came from, so a user who *moves* a project can simply
/// rename that directory to the new path's folded form and the session follows. The result
/// is a single, traversal-free path component (no `/` or `.` survive the fold), though a
/// very deep path can exceed the filesystem's per-component limit — an acceptable trade for
/// portability, and it fails loud at store creation rather than silently colliding.
pub fn workspace_namespace(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The store for the native binary, honoring an optional shada `namespace` (the
/// `--shada-namespace` value, already validated by [`valid_namespace`]). With a
/// namespace the store lives under `shada_dir()/ns/<namespace>/`, isolating this
/// project's registers / marks / history / session from the global store and from
/// other workspaces; without one it is the global [`default_shada`].
pub fn workspace_shada(namespace: Option<&str>) -> Box<dyn ShadaStore + Send> {
    match namespace {
        Some(ns) => Box::new(RedbFileStore::new(shada_dir().join("ns").join(ns))),
        None => default_shada(),
    }
}

/// Whether `path` is a shada store file, for tests asserting compaction. Public so
/// the integration suite can count surviving files without re-deriving the layout.
pub fn is_store_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("redb")
}

// ============================================================================
// Remote shada (Approach A): the store on the daemon's machine
// ============================================================================
//
// A `Remote`-config daemon session keeps its shada on the **daemon**, not the local
// client, so the editor state for a remote workspace travels with that workspace. The
// daemon fs seam is whole-file read/write only (no random access, no cross-machine
// lock), so redb can't run *live* over it. Instead (Approach A): at connect, mirror the
// daemon's per-instance store files into a fresh local **staging** dir and run an ordinary
// [`RedbFileStore`] there (fast local random access, a real local lock); after each flush,
// upload our own staged instance file back, and at clean exit delete the absorbed siblings
// on the daemon. The staged local redb *is* the on-remote artifact — only whole-file bytes
// cross the wire, and the daemon runs no shada logic.
//
// **Per-instance mirror.** The remote shada lives in a directory (`<state_dir>/ns/<NS>/` for
// a namespaced session, `<state_dir>/remote/` for a global one — see [`remote_shada_dir`])
// holding the same `<pid>.<nanos>.<seq>.redb` per-instance
// files as the local store — so the merge + carry-forward compaction model ports verbatim:
// every session downloads + merges all siblings, uploads its own file, and at clean exit
// removes the siblings it absorbed (`fs_remove`), keeping the remote dir bounded by live
// sessions rather than total launches. Two concurrent remote sessions on the same workspace
// each see the other's data (recency merge); cross-machine liveness can't be detected, so a
// live sibling may be transiently removed and re-uploaded — harmless, like the local model's
// crash-redundancy. A namespaced session (`--workspace` / `--shada-namespace`) targets the
// daemon's *native* `<state_dir>/ns/<NS>/` — the SAME store a local editor on the daemon
// machine uses for that namespace — so the two share one per-project shada (the per-instance
// files merge as concurrent siblings). Only the anonymous *global* remote session stays under a
// `remote/` sibling, isolated from the daemon's own local global shada; `read_dir` is
// non-recursive, so the daemon's global `*.redb` is never globbed by a namespaced `ns/<NS>`.

/// The on-remote shada wiring for a `Remote`-config session: the daemon-side **directory**
/// the staged store mirrors to, plus the sibling files downloaded at connect (so the
/// clean-exit compaction can delete the ones it absorbed). Paired with the
/// [`RedbFileStore`] [`prepare_remote_shada`] returns; the edit-host uploads the store's
/// own [`current_path`](ShadaStore::current_path) file here after each flush.
#[cfg(feature = "native")]
pub struct RemoteShada {
    /// The remote shada directory on the daemon: `<state_dir>/ns/<NS>` for a namespaced
    /// (workspace) session — the daemon's native per-namespace store, shared with a local
    /// editor on that host — or `<state_dir>/remote` for an anonymous global session.
    pub remote_dir: String,
    /// The sibling store filenames downloaded + absorbed at connect, deleted on the daemon
    /// at clean-exit compaction (mirroring the local store's sibling deletion).
    pub downloaded: Vec<String>,
}

/// The remote shada directory for `state_dir` (= the daemon's native `shada_dir()`) + an
/// optional namespace.
///
/// A **namespaced** session (a `--workspace` or `--shada-namespace` identity) writes to the
/// daemon's *native* per-namespace store, `<state_dir>/ns/<NS>` — exactly where a local
/// editor running **on the daemon machine** with the same namespace keeps it. So a remote
/// daemon workspace and a native session on that host share one shada: editing
/// `/srv/proj` over the daemon and SSH-ing in to edit it locally see the same
/// marks/registers/session. (The per-instance `.redb` + merge/compaction model makes the
/// two sets of files coexist safely, like any two concurrent sessions.)
///
/// An **anonymous** (global, no-namespace) remote session instead stays isolated under
/// `<state_dir>/remote`, so a generic remote-editing session never merges into — or
/// clobbers — the daemon machine's own local global shada.
#[cfg(feature = "native")]
fn remote_shada_dir(state_dir: &str, namespace: Option<&str>) -> String {
    let base = state_dir.trim_end_matches('/');
    match namespace {
        Some(ns) => format!("{base}/ns/{ns}"),
        None => format!("{base}/remote"),
    }
}

/// A fresh per-process staging dir for a remote session's shada, cleared on entry so each
/// connect starts clean (mirrors the remote-config cache): `$XDG_CACHE_HOME/nxvim/
/// remote-shada/<pid>` (else `$HOME/.cache/…`, else a temp dir).
#[cfg(feature = "native")]
fn remote_shada_staging() -> std::io::Result<PathBuf> {
    let base = if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(d)
    } else if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h).join(".cache")
    } else {
        std::env::temp_dir()
    };
    let dir = base
        .join("nxvim")
        .join("remote-shada")
        .join(std::process::id().to_string());
    // Fresh every connect (a stale dir from a crashed prior pid-reuse can't leak in).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Prepare a `Remote`-config session's shada (Approach A, per-instance mirror): ensure the
/// remote shada dir exists (`fs_mkdir`), download every sibling `.redb` store into a fresh
/// local staging dir, then build a [`RedbFileStore`] there (which merges them on load).
/// Returns the store (for [`ServerInit::shada`](crate::ServerInit::shada)) plus the
/// [`RemoteShada`] sync target (for [`ServerInit::remote_shada`](crate::ServerInit::remote_shada)),
/// recording the downloaded sibling names for clean-exit compaction. A download error is
/// **loud** — the caller disables remote shada rather than risk a half-mirrored store.
#[cfg(feature = "native")]
pub async fn prepare_remote_shada(
    host_fs: &dyn crate::HostFsAsync,
    state_dir: &str,
    namespace: Option<&str>,
) -> std::io::Result<(Box<dyn ShadaStore + Send>, RemoteShada)> {
    let remote_dir = remote_shada_dir(state_dir, namespace);
    // Ensure the dir exists before listing/writing (`fs_write` doesn't create parents).
    host_fs.mkdir(remote_dir.clone()).await?;
    let staging = remote_shada_staging()?;
    let mut downloaded = Vec::new();

    // List the remote dir and mirror each sibling store file into staging under its own
    // name, so `RedbFileStore::load` merges them exactly as it merges local siblings.
    let entries = match host_fs.read(remote_dir.clone()).await? {
        crate::FsRead::Dir { entries, .. } => entries,
        // Just-created (or empty): no siblings yet — the first-ever session for this dir.
        crate::FsRead::New => Vec::new(),
        crate::FsRead::File(_, _) => {
            return Err(std::io::Error::other(format!(
                "remote shada dir {remote_dir:?} is a file, not a directory"
            )));
        }
    };
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(".redb") {
            continue;
        }
        let remote_file = format!("{remote_dir}/{}", entry.name);
        match host_fs.read(remote_file).await? {
            crate::FsRead::File(bytes, _) => {
                std::fs::write(staging.join(&entry.name), bytes)?;
                downloaded.push(entry.name);
            }
            // A sibling that vanished between the listing and the read (a concurrent
            // session's compaction) is simply not mirrored — no error.
            crate::FsRead::New => {}
            crate::FsRead::Dir { .. } => {}
        }
    }

    let store: Box<dyn ShadaStore + Send> = Box::new(RedbFileStore::new(staging));
    Ok((
        store,
        RemoteShada {
            remote_dir,
            downloaded,
        },
    ))
}

/// Pick a session's shada store from its [`ConfigSource`](crate::ConfigSource) — the
/// shared path both native clients take at connect, so the download-or-local decision
/// lives in one place. `Remote` (with a daemon-reported `state_dir`) keeps shada on the
/// daemon ([`prepare_remote_shada`]); a download error or a daemon that reported no
/// `state_dir` falls back to `fallback_local` (never clobbering the daemon's copy with a
/// fresh empty store). `Local` always uses `fallback_local`. Returns the store (for
/// [`ServerInit::shada`](crate::ServerInit::shada)) and the optional remote sync target
/// (for [`ServerInit::remote_shada`](crate::ServerInit::remote_shada)).
#[cfg(feature = "native")]
pub async fn resolve_session_shada(
    host_fs: &dyn crate::HostFsAsync,
    source: crate::ConfigSource,
    state_dir: Option<&str>,
    namespace: Option<&str>,
    fallback_local: Box<dyn ShadaStore + Send>,
) -> (Box<dyn ShadaStore + Send>, Option<RemoteShada>) {
    match (source, state_dir) {
        (crate::ConfigSource::Remote, Some(dir)) => {
            match prepare_remote_shada(host_fs, dir, namespace).await {
                Ok((store, rs)) => (store, Some(rs)),
                Err(e) => {
                    eprintln!("nxvim: remote shada unavailable ({e}); using local shada");
                    (fallback_local, None)
                }
            }
        }
        (crate::ConfigSource::Remote, None) => {
            eprintln!("nxvim: daemon reported no shada dir; using local shada");
            (fallback_local, None)
        }
        (crate::ConfigSource::Local, _) => (fallback_local, None),
    }
}
