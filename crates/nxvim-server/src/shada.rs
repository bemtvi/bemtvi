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
//! history; Phase 4 the numbered marks `'0`–`'9` (shifted at load), the per-file
//! changelist, the focused window's jumplist, and the `meta` row carrying the
//! clean-exit cursor. Compaction is payload-agnostic (it deletes whole absorbed
//! files) so it does not change. Full design:
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nxvim_core::{
    FileChangelist, FileMarkEntry, GlobalMarkEntry, JumpPos, NumberedMark, PersistState,
    RegisterEntry,
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
    /// Re-read this instance's own store plus every readable sibling and return the
    /// merged snapshot, **without** minting a file, shifting the numbered marks, or
    /// compacting (the load-only steps). This is the `:rshada` read: it picks up any
    /// sibling that has exited since startup (a still-live one is locked, hence
    /// skipped — neovim's contract) and folds it into the running session. The
    /// numbered marks come through un-shifted (the `'0` shift is a launch event,
    /// not a re-read) and the snapshot carries no `exit_cursor`.
    fn reload(&mut self) -> std::io::Result<PersistState>;
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

/// `marks_numbered` table: key is the digit `0`–`9`, value a msgpack [`StoredMark`].
/// The store *shifts* these at load (`'0` ← last-exit cursor, old `'0`→`'1`, …).
const MARKS_NUMBERED: TableDefinition<&str, &[u8]> = TableDefinition::new("marks_numbered");

/// `changelist_file` table: key is the file path, value a msgpack [`StoredChangelist`].
const CHANGELIST_FILE: TableDefinition<&str, &[u8]> = TableDefinition::new("changelist_file");

/// `jumplist` table: key is the entry's sequence index (`0`-based, oldest first),
/// value a msgpack [`StoredPos`]. Rewritten wholesale each flush; the newest store's
/// list wins on merge (a jumplist is an ordered sequence, not a union).
const JUMPLIST: TableDefinition<u64, &[u8]> = TableDefinition::new("jumplist");

/// `meta` table: a single `"meta"` row holding the schema version, this write's
/// timestamp (the jumplist recency key), and the last clean-exit cursor that
/// becomes `'0` next launch.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

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
}

impl RedbFileStore {
    /// A store living under `dir` (`stdpath("state")/shada` for the real binary, a
    /// temp dir for tests). No I/O happens until [`load`](RedbFileStore::load).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            db: None,
            path: None,
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
        // handles alive so we can read them, and the paths so we can delete the dead
        // ones once our merged snapshot is durable.
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

        // Make the absorbed data durable in *our* file before deleting the
        // siblings. A crash before this commit leaves the siblings intact (the next
        // instance re-absorbs them); a crash after the commit but before the deletes
        // leaves redundant copies (likewise harmless). Either way: no data loss.
        write_state(&db, &state)?;
        // Release the sibling handles before unlinking their files (some platforms
        // refuse to delete a still-open file).
        drop(siblings);
        for path in absorbed {
            let _ = std::fs::remove_file(path);
        }

        self.path = Some(my_path);
        self.db = Some(db);
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
    hist_search: HistMerge,
    hist_ex: HistMerge,
    /// The jumplist is an ordered sequence, not a union: the newest store's whole
    /// list wins (keyed by its `meta.flush_ts`).
    jumplist: (u64, Vec<StoredPos>),
    /// The newest *clean* exit cursor across the stores (keyed by `meta.exit_ts`).
    exit: (u64, Option<StoredPos>),
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
        merge_history(db, HIST_SEARCH, &mut m.hist_search);
        merge_history(db, HIST_EX, &mut m.hist_ex);
        if let Some(meta) = read_meta(db) {
            if meta.flush_ts > m.jumplist.0 {
                m.jumplist = (meta.flush_ts, read_jumplist(db));
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
        numbered_marks,
        file_changelists: m
            .changelist
            .into_iter()
            .map(|(path, stored)| FileChangelist {
                path: path.into(),
                entries: stored.entries,
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
    }
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

/// Whether `path` is a shada store file, for tests asserting compaction. Public so
/// the integration suite can count surviving files without re-deriving the layout.
pub fn is_store_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("redb")
}
