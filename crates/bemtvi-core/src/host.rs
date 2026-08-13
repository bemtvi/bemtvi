//! The host I/O seam: the synchronous filesystem interface the editor depends on.
//!
//! `bemtvi-core` reaches the filesystem only through [`HostFs`] — never `std::fs`
//! directly — so the *same* editing core can run against the local disk
//! ([`StdHostFs`], the default) or a remote daemon over the wire (the edit-host /
//! daemon split — see `docs/plans/2026-06-09-edit-host-and-browser-lua.md`). The
//! trait is
//! deliberately **synchronous**: it is called on the editing path's terms (at
//! buffer open / save, not per-keystroke), and any async waiting belongs *above*
//! core — in the server, which can populate a buffer off-tick and hand it down.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A file's on-disk identity, for change detection: last-modified time (when the
/// platform reports one) and byte length. Either alone can miss a change — a
/// same-length edit keeps the size; a coarse clock can repeat an mtime — so the
/// pair is always compared together. A `None` stat (file absent or inaccessible)
/// is itself a distinct, meaningful state, so a file that vanishes or appears
/// registers as a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub mtime: Option<SystemTime>,
    pub size: u64,
}

/// One entry in a directory listing: whether it is a sub-directory, and its name
/// (the final path component, not a full path).
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub is_dir: bool,
    pub name: String,
}

/// The filesystem operations the editor core needs. An implementation decides
/// *where* the bytes live (local disk, a remote daemon, an in-memory store);
/// core only ever knows this contract. Synchronous by design (see module docs).
pub trait HostFs {
    /// Whether `path` exists (a readable file the caller will load, vs. a
    /// not-yet-written new-file buffer).
    fn exists(&self, path: &Path) -> bool;

    /// Open `path` for streaming reads (so a large file lands in the rope at ~1×
    /// its size, not 2×). Errors on a real failure (permission, transport); the
    /// caller only opens paths it first found via [`HostFs::exists`].
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read>>;

    /// Stat `path`, or `None` if it can't be stat'd (absent/inaccessible).
    fn stat(&self, path: &Path) -> Option<FileStat>;

    /// Atomically replace `path`'s contents with `contents`: a reader never
    /// observes a half-written file. The strategy (temp + rename, or a
    /// daemon-side equivalent) is the implementation's concern.
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()>;

    /// List `dir`'s immediate entries, unsorted (the caller sorts).
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>>;

    /// Resolve symlinks and `.`/`..` to a canonical absolute path.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Recursively create `dir` (and any missing parents); a no-op if it already
    /// exists. The default fails loud — a backend with no directory-create support
    /// must say so at runtime, not silently succeed (the no-silent-stub rule). Used
    /// by the remote-shada mirror to ensure its per-namespace dir before writing.
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "create_dir_all is not supported by this filesystem backend",
        ))
    }

    /// Remove the file at `path`. The default fails loud (see [`HostFs::create_dir_all`]).
    /// Used by the remote-shada mirror's clean-exit compaction to delete absorbed
    /// sibling stores on the daemon.
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "remove_file is not supported by this filesystem backend",
        ))
    }
}

/// The default [`HostFs`]: the real local filesystem via `std::fs`. Every front
/// end uses this today; remote/daemon implementations arrive with the edit-host
/// split. (It compiles on `wasm32-unknown-unknown` too — the serverless web
/// build constructs an `Editor` but never opens files, so these are never hit.)
#[derive(Debug, Default, Clone, Copy)]
pub struct StdHostFs;

impl HostFs for StdHostFs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read>> {
        // Refuse anything but a regular file, loudly. A FIFO or device would block
        // the (synchronous, uncancellable) edit thread on read forever — `:e /dev/zero`
        // or a stray `mkfifo` in a repo would hang the whole editor — or grow memory
        // without bound. `metadata` follows symlinks, so a symlink to a regular file
        // still opens.
        if !std::fs::metadata(path)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(Box::new(std::io::BufReader::new(std::fs::File::open(
            path,
        )?)))
    }

    fn stat(&self, path: &Path) -> Option<FileStat> {
        let meta = std::fs::metadata(path).ok()?;
        Some(FileStat {
            mtime: meta.modified().ok(),
            size: meta.len(),
        })
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        write_atomic(path, contents)
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(DirEntry {
                is_dir,
                name: entry.file_name().to_string_lossy().into_owned(),
            });
        }
        Ok(out)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

/// Write `contents` to `path` atomically: stream into a temp file in the same
/// directory, fsync it, then `rename` it over the target. A crash, `SIGKILL`,
/// full disk, or power loss mid-save can lose the *new* write but never
/// truncates or half-writes the file the way `std::fs::write`'s `O_TRUNC` would —
/// the rename either fully publishes the new contents or leaves the old file
/// untouched. (Atomicity holds within one filesystem, which is why the temp sits
/// next to the final file.)
///
/// If `path` is a symlink it is resolved first, so the rename replaces the file
/// the link points at — keeping the link rather than clobbering it with a regular
/// file — and the temp lands on the same filesystem as that real file. An
/// existing target's permissions (and, best-effort, its ownership) are carried
/// onto the replacement so a save never silently downgrades them.
///
/// Trade-off vs. an in-place write: an atomic save needs to *create* a temp entry
/// in the target's directory, so a writable file inside a read-only directory can
/// no longer be saved — it fails loudly (which the editor surfaces) instead of
/// silently truncating. That matches bemtvi's "fail loud" posture.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    // Resolve a symlink (and any `..`) to the real file so we replace *it*, not
    // the link, and so the temp shares its filesystem. A path that doesn't exist
    // yet (a brand-new file) has no canonical form — keep it as given.
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = real.parent().unwrap_or_else(|| Path::new("."));
    let existing = std::fs::metadata(&real).ok();

    // The temp must be created with `O_EXCL` (`create_new`) and an **unpredictable**
    // name: a predictable `.name.bemtvi-tmp.<pid>` path is a symlink-attack surface
    // (a local attacker with write access to the save directory can pre-create it
    // pointing at any file the victim can write, and `File::create` would open —
    // and truncate — through the link). The random component defeats the
    // pre-creation; `create_new` makes a collision fail loudly instead of following.
    // Retry with a fresh random name on the (astronomically unlikely) collision.
    let write = || -> io::Result<()> {
        for _ in 0..8 {
            let tmp = dir.join(temporary_name(&real)?);
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            let result = (|| -> io::Result<()> {
                let mut file = file;
                file.write_all(contents)?;
                // Durability: the bytes (and the file's metadata) must reach disk
                // before the rename publishes the temp under the target's name.
                file.sync_all()?;
                drop(file);
                // Carry the prior file's mode (and best-effort owner) onto the temp
                // so a save preserves them; a fresh file keeps `create_new`'s default.
                if let Some(meta) = &existing {
                    std::fs::set_permissions(&tmp, meta.permissions())?;
                    #[cfg(unix)]
                    preserve_owner(&tmp, meta);
                }
                std::fs::rename(&tmp, &real)
            })();
            if result.is_err() {
                // Never leave a partial temp behind on failure.
                let _ = std::fs::remove_file(&tmp);
            }
            return result;
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not create a unique temp file next to {} (8 collisions)",
                real.display()
            ),
        ))
    };

    write()
}

/// A unique temp-file name next to `real`: `.{name}.bemtvi-tmp.{pid}-{rand:016x}`.
/// The random component is what defeats the symlink pre-creation attack — a name
/// predictable from the pid alone lets a local attacker with write access to the
/// directory plant a symlink that the save would write through (see [`write_atomic`]).
fn temporary_name(real: &Path) -> io::Result<std::ffi::OsString> {
    let mut rand = [0u8; 8];
    getrandom::fill(&mut rand).map_err(io::Error::other)?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(real.file_name().unwrap_or(std::ffi::OsStr::new("bemtvi")));
    tmp_name.push(format!(
        ".bemtvi-tmp.{}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(rand)
    ));
    Ok(tmp_name)
}

/// Best-effort: carry `meta`'s owner/group onto `path`. Only the super-user can
/// `chown` to an arbitrary owner, so for a normal user saving their own file this
/// is a no-op; a failure (e.g. `EPERM` when an unprivileged user saves a file
/// owned by someone else) must not fail the save, so the result is ignored.
#[cfg(unix)]
fn preserve_owner(path: &Path, meta: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    let _ = std::os::unix::fs::chown(path, Some(meta.uid()), Some(meta.gid()));
}
