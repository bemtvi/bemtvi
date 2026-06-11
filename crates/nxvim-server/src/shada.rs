//! The shada persistence store — cross-session editor state.
//!
//! Persistence is an *impure capability*, so it sits behind a seam — the
//! [`ShadaStore`] trait — exactly like [`HostFs`](nxvim_core::HostFs) /
//! [`HostProc`](crate::HostProc). The server core only ever calls `load` / `flush`;
//! the platform layer constructs the concrete store and injects it through
//! [`ServerInit::shada`](crate::ServerInit). That matters because the *server* (not
//! just the core) targets the browser: the native build injects [`RedbFileStore`]
//! (redb over a real file), and the wasm Worker build will inject a redb store over
//! an **OPFS** `StorageBackend` — same engine, different bytes underneath. So shada
//! lives wherever the editor runs (local disk or browser storage), never on the
//! remote daemon.
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
//! after merging the dead siblings into memory, the store **flushes that merged
//! snapshot into its own file** (so the absorbed data is durable here) and then
//! **deletes the siblings it absorbed**. So the only files that survive a startup
//! are this instance's plus any currently-*live* (locked) ones — file count is
//! bounded by concurrent instances, not by total launches, and startup cost is
//! O(live instances), not O(history). A normal single-editor user always has
//! exactly one file.
//!
//! Phase 1 persists **registers**; Phase 2 the global file marks `A`–`Z`; Phase 3
//! the per-file marks (`a`–`z`, specials, the `"` last-cursor) and search/ex
//! history. Later phases grow the schema (numbered marks, jumplist/changelist);
//! compaction is payload-agnostic (it deletes whole absorbed files) so it does not
//! change. Full design: `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nxvim_core::{FileMarkEntry, GlobalMarkEntry, PersistState, RegisterEntry};
use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};

/// The per-history entry cap (newest-N kept) applied on merge, mirroring vim's
/// `'history'`. A later phase exposes it as an option.
const HISTORY_CAP: usize = 10_000;

/// The persistence seam the server drives. `load` is called once before the first
/// frame (it merges + compacts and returns the snapshot to import); `flush` writes
/// the current snapshot back. Both run off the editor hot loop (startup / exit), so
/// a synchronous trait is fine on every platform — native file I/O and an OPFS sync
/// access handle (in a Worker) are both synchronous.
pub trait ShadaStore {
    /// Open this instance's store, recency-merge every readable sibling store,
    /// compact (absorb + delete) the dead ones, and return the merged snapshot.
    fn load(&mut self) -> std::io::Result<PersistState>;
    /// Persist `state` into this instance's store.
    fn flush(&mut self, state: &PersistState) -> std::io::Result<()>;
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

/// The native (and, via a custom `StorageBackend`, browser) shada store: a
/// per-instance redb database in a shared directory, with carry-forward
/// compaction of sibling stores.
pub struct RedbFileStore {
    dir: PathBuf,
    /// This instance's database, opened lazily by [`load`](RedbFileStore::load) and
    /// reused by [`flush`](RedbFileStore::flush).
    db: Option<Database>,
}

impl RedbFileStore {
    /// A store living under `dir` (`stdpath("state")/shada` for the real binary, a
    /// temp dir for tests). No I/O happens until [`load`](RedbFileStore::load).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            db: None,
        }
    }
}

impl ShadaStore for RedbFileStore {
    fn load(&mut self) -> std::io::Result<PersistState> {
        std::fs::create_dir_all(&self.dir)?;
        let my_path = self.dir.join(instance_filename());
        let db = Database::create(&my_path).map_err(std::io::Error::other)?;

        // Merge every sibling we can open; record the ones we absorbed so we can
        // delete them once our merged snapshot is durable.
        let mut best_regs: std::collections::HashMap<char, StoredRegister> =
            std::collections::HashMap::new();
        let mut best_marks: std::collections::HashMap<char, StoredMark> =
            std::collections::HashMap::new();
        let mut best_file_marks: std::collections::HashMap<(String, char), StoredFileMark> =
            std::collections::HashMap::new();
        let mut hist_search = HistMerge::default();
        let mut hist_ex = HistMerge::default();
        let mut absorbed: Vec<PathBuf> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&self.dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path == my_path || path.extension().and_then(|e| e.to_str()) != Some("redb") {
                    continue;
                }
                // A live instance holds the lock → `open` fails → skip it (its data
                // is simply not visible yet, exactly neovim's contract). A dead one
                // opens; we read it, drop the handle, and mark it for deletion.
                if let Ok(sibling) = Database::open(&path) {
                    merge_registers(&sibling, &mut best_regs);
                    merge_global_marks(&sibling, &mut best_marks);
                    merge_file_marks(&sibling, &mut best_file_marks);
                    merge_history(&sibling, HIST_SEARCH, &mut hist_search);
                    merge_history(&sibling, HIST_EX, &mut hist_ex);
                    drop(sibling);
                    absorbed.push(path);
                }
            }
        }

        let merged = PersistState {
            registers: best_regs
                .into_iter()
                .map(|(name, stored)| RegisterEntry {
                    name,
                    text: stored.text,
                    linewise: stored.linewise,
                })
                .collect(),
            global_marks: best_marks
                .into_iter()
                .map(|(name, stored)| GlobalMarkEntry {
                    name,
                    path: stored.path.into(),
                    line: stored.line,
                    col: stored.col,
                })
                .collect(),
            file_marks: best_file_marks
                .into_iter()
                .map(|((path, name), stored)| FileMarkEntry {
                    path: path.into(),
                    name,
                    line: stored.line,
                    col: stored.col,
                })
                .collect(),
            search_history: hist_search.finish(),
            ex_history: hist_ex.finish(),
        };

        // Make the absorbed data durable in *our* file before deleting the
        // siblings. A crash before this commit leaves the siblings intact (the next
        // instance re-absorbs them); a crash after the commit but before the deletes
        // leaves redundant copies (likewise harmless). Either way: no data loss.
        write_state(&db, &merged)?;
        for path in absorbed {
            let _ = std::fs::remove_file(path);
        }

        self.db = Some(db);
        Ok(merged)
    }

    fn flush(&mut self, state: &PersistState) -> std::io::Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shada flush before load"))?;
        write_state(db, state)
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
    write_history(&wtxn, HIST_SEARCH, &state.search_history, ts)?;
    write_history(&wtxn, HIST_EX, &state.ex_history, ts)?;
    wtxn.commit().map_err(std::io::Error::other)?;
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

/// Whether `path` is a shada store file, for tests asserting compaction. Public so
/// the integration suite can count surviving files without re-deriving the layout.
pub fn is_store_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("redb")
}
