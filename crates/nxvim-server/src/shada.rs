//! The shada persistence store — cross-session editor state in a per-instance
//! [redb](https://docs.rs/redb) database.
//!
//! redb is single-process (it takes an exclusive file lock on open), so a shared
//! store every daemon writes is not possible — and not wanted. Instead each
//! instance owns **its own** `<pid>.<nanos>.redb` file in `stdpath("state")/shada/`,
//! getting redb's full ACID / crash-safety / incremental-write guarantees with no
//! cross-process contention. On startup an instance reads every *other* sibling
//! file it can open (a live instance holds its lock and is skipped; a cleanly
//! exited or crashed one released it) and **recency-merges** them — newest
//! timestamp wins per key — into one [`PersistState`] it imports before the first
//! frame. This reproduces neovim's "you see another instance's data once it has
//! written" contract while adding crash-safety neovim lacks.
//!
//! Phase 1 persists **registers** only, load-on-start / flush-on-exit (no debounce
//! yet). Later phases grow the schema (marks, jumplist/changelist, history,
//! numbered marks) and route the off-tick write through the `HostEffects` fs seam.
//! Full design: `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nxvim_core::{PersistState, RegisterEntry};
use redb::{Database, ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};

/// `registers` table: key is the one-char register name, value is a msgpack
/// [`StoredRegister`].
const REGISTERS: TableDefinition<&str, &[u8]> = TableDefinition::new("registers");

/// A register as stored on disk: its contents, paste kind, and the write
/// timestamp that drives the cross-instance recency merge.
#[derive(Serialize, Deserialize)]
struct StoredRegister {
    text: String,
    linewise: bool,
    ts: u64,
}

/// A live shada store: this instance's own redb file, opened for writing.
pub struct Shada {
    db: Database,
}

impl Shada {
    /// Open the store directory, recency-merge every readable sibling store into a
    /// [`PersistState`] to import, and mint **this** instance's own writable file.
    /// Returns the merged state alongside the live handle the server keeps to flush
    /// through on exit.
    pub fn open(dir: &Path) -> std::io::Result<(PersistState, Shada)> {
        std::fs::create_dir_all(dir)?;
        let merged = load_merged(dir);
        let file = dir.join(instance_filename());
        let db = Database::create(&file).map_err(std::io::Error::other)?;
        Ok((merged, Shada { db }))
    }

    /// Write the snapshot into this instance's file in one transaction, stamping
    /// each row with the current time (the merge key a later instance reads).
    pub fn flush(&self, state: &PersistState) -> std::io::Result<()> {
        let ts = now_ms();
        let wtxn = self.db.begin_write().map_err(std::io::Error::other)?;
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
        wtxn.commit().map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// Read every sibling `*.redb` we can open (skipping locked/live ones) and merge
/// their registers, newest `ts` winning per name.
fn load_merged(dir: &Path) -> PersistState {
    let mut best: std::collections::HashMap<char, StoredRegister> =
        std::collections::HashMap::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return PersistState::default();
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("redb") {
            continue;
        }
        // A live instance holds the lock → `open` fails → skip it (its data is
        // simply not visible yet, exactly neovim's contract).
        let Ok(db) = Database::open(&path) else {
            continue;
        };
        merge_registers(&db, &mut best);
    }
    let registers = best
        .into_iter()
        .map(|(name, stored)| RegisterEntry {
            name,
            text: stored.text,
            linewise: stored.linewise,
        })
        .collect();
    PersistState { registers }
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

/// This instance's store filename. `pid` plus a high-resolution timestamp keeps
/// it unique across the instances that may share one state dir on a machine.
fn instance_filename() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}.{}.redb", std::process::id(), nanos)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The default shada directory for the real binary: `stdpath("state")/shada`,
/// i.e. `$XDG_STATE_HOME/nxvim/shada` (or `$HOME/.local/state/nxvim/shada`). Tests
/// pass an explicit temp dir through [`ServerInit::state_dir`](crate::ServerInit)
/// instead, so they never touch the real one.
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
