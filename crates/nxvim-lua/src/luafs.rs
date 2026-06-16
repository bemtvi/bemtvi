//! The Lua-visible **filesystem** seam — the fs analogue of the [`BlockingSystem`]
//! shell-out seam (`system.rs`).
//!
//! Plugins read the *project* through a handful of `vim.fn` builtins (`readblob`,
//! `glob`, `filereadable`, `executable`, `getftime`, `isdirectory`, …) and the
//! `nx._readdir` primitive. Those bind *directly* to `std::fs` today, which is
//! correct for a local session but wrong in the edit-host / daemon split
//! (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → *The full split*): there
//! the editor + Lua VM run **locally** while the project files live on the remote
//! **daemon**, so a `root_dir` detector or a file previewer touching the disk
//! directly would see the *wrong* machine.
//!
//! This module is the synchronous seam those project-facing fs calls route through.
//! The default ([`StdLuaFs`]) is today's `std::fs` logic factored verbatim, so a bare
//! local session is byte-for-byte unchanged. A daemon session injects a *blocking
//! bridge* (`RemoteLuaFs`, in `nxvim-server`'s daemon wire) that runs each operation
//! on the remote and parks the editor thread on the reply — the fs analogue of the
//! `vim.system` blocking bridge. The seam is **synchronous** because a `vim.fn` fs
//! builtin returns its value inline on the Lua tick. (The [`LuaFs`] trait still
//! models the full libuv-shaped fs operation set so the daemon wire can serve it;
//! the live Lua surface only exercises `stat`/`scandir`/`read_file`/`realpath`/`which`.)
//!
//! # The split-brain routing rule (decided up front, per the plan)
//!
//! **Routes through this seam** (project-facing — must reach the daemon): the `vim.fn`
//! fs builtins `readblob`/`glob`/`filereadable`/`isdirectory`/`getftime`/`resolve`/
//! `executable`/`exepath`; and the `nx._readdir` primitive `vim.fs.find` walks the
//! project with.
//!
//! **Stays local** (config / plugin / VM state — never routed): raw Lua `io.*` /
//! `os.*`, `require` / `package.path`, `nvim_get_runtime_file` (runtimepath = local
//! plugins), `nx._read_file` (sources an `lsp/<name>.lua` *config* found on the
//! runtimepath), and `stdpath` (local config/data/state dirs). These keep their
//! direct `std::fs` calls — plugins and config live on the local machine by design.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The dirent / stat kind, not followed through symlinks for `lstat`/`scandir`
/// (matching libuv). `stat` follows links, so it never yields [`FileKind::Link`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Link,
}

impl FileKind {
    /// The libuv kind string (`"file"` / `"directory"` / `"link"`) plugins test.
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Dir => "directory",
            FileKind::Link => "link",
        }
    }

    /// Reconstruct from the wire string; anything unknown is a plain file.
    pub fn from_wire(s: &str) -> FileKind {
        match s {
            "directory" => FileKind::Dir,
            "link" => FileKind::Link,
            _ => FileKind::File,
        }
    }
}

/// A libuv-shaped stat result, in pure-Rust form (no `mlua` types — the `uvfs`
/// layer turns this into the Lua table). Carries every field `stat_table` reads:
/// the kind/size/`st_mode`, the `{sec,nsec}` access/modify times, and the unix
/// `st_*` extras (`0` off unix). The whole-struct shape is what crosses the daemon
/// wire, so a remote `fs_stat` reproduces a local one field-for-field.
#[derive(Clone, Debug)]
pub struct LuaStat {
    pub kind: FileKind,
    pub size: u64,
    pub mode: u32,
    /// Modify time as `(secs, nsecs)` since the Unix epoch, when available.
    pub mtime: Option<(i64, u32)>,
    /// Access time as `(secs, nsecs)` since the Unix epoch, when available.
    pub atime: Option<(i64, u32)>,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub dev: u64,
}

impl LuaStat {
    /// Build from a `std::fs::Metadata`. `kind` reflects the metadata as given — a
    /// `metadata()` (follow-symlinks) call never reports [`FileKind::Link`]; a
    /// `symlink_metadata()` one can — so the caller picks which it stats with.
    fn from_metadata(md: &std::fs::Metadata) -> LuaStat {
        let kind = if md.is_dir() {
            FileKind::Dir
        } else if md.file_type().is_symlink() {
            FileKind::Link
        } else {
            FileKind::File
        };
        let to_pair = |t: io::Result<SystemTime>| {
            t.ok().and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
            })
        };
        let stat = LuaStat {
            kind,
            size: md.len(),
            mode: st_mode(md),
            mtime: to_pair(md.modified()),
            atime: to_pair(md.accessed()),
            ino: 0,
            uid: 0,
            gid: 0,
            nlink: 0,
            dev: 0,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            LuaStat {
                ino: md.ino(),
                uid: md.uid(),
                gid: md.gid(),
                nlink: md.nlink(),
                dev: md.dev(),
                ..stat
            }
        }
        #[cfg(not(unix))]
        {
            stat
        }
    }
}

/// One entry from [`LuaFs::scandir`]: the final path component and its dirent kind
/// (not followed through symlinks, matching libuv's `fs_scandir_next`).
#[derive(Clone, Debug)]
pub struct LuaDirEntry {
    pub name: String,
    pub kind: FileKind,
}

/// The synchronous filesystem seam the project-facing Lua fs surface runs through.
///
/// An implementation decides *where* the bytes live (local disk via [`StdLuaFs`], or
/// a remote daemon via `RemoteLuaFs`). Open files are referred to by an opaque `i64`
/// **fd token** the implementation mints in [`open`](LuaFs::open) and looks back up in
/// [`read`](LuaFs::read)/[`write`](LuaFs::write)/[`fstat`](LuaFs::fstat)/
/// [`close`](LuaFs::close) — so the daemon, not the edit-host, owns the real `File`.
/// Errors are `io::Result` so the `uvfs` layer's existing libuv-style error shaping is
/// unchanged; [`access`](LuaFs::access) returns a bare `bool` (libuv maps the
/// `access(2)` result to one rather than erroring).
pub trait LuaFs {
    fn open(&self, path: &str, flags: &str, mode: u32) -> io::Result<i64>;
    fn close(&self, fd: i64) -> io::Result<()>;
    fn read(&self, fd: i64, size: usize, offset: Option<i64>) -> io::Result<Vec<u8>>;
    fn write(&self, fd: i64, data: &[u8], offset: Option<i64>) -> io::Result<usize>;
    fn fstat(&self, fd: i64) -> io::Result<LuaStat>;
    fn stat(&self, path: &str) -> io::Result<LuaStat>;
    fn lstat(&self, path: &str) -> io::Result<LuaStat>;
    /// The directory's entries, materialized in one shot (so a remote scandir is one
    /// round-trip, not one per entry — the libuv iterator handle is reconstructed
    /// locally over this `Vec`).
    fn scandir(&self, path: &str) -> io::Result<Vec<LuaDirEntry>>;
    /// Create `path` with permission `mode`; `recursive` creates parents too (the
    /// `vim.fn.mkdir(_, "p")` form), else a single level.
    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> io::Result<()>;
    fn rmdir(&self, path: &str) -> io::Result<()>;
    fn unlink(&self, path: &str) -> io::Result<()>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// Copy `src` to `dest`; `excl` fails when `dest` already exists (libuv's
    /// `UV_FS_COPYFILE_EXCL`).
    fn copyfile(&self, src: &str, dest: &str, excl: bool) -> io::Result<()>;
    /// Set `path`'s access/modify times from fractional-second timestamps.
    fn utime(&self, path: &str, atime: f64, mtime: f64) -> io::Result<()>;
    /// Whether `path` grants every requested access letter (`R`/`W`/`X`; empty =
    /// existence). Never errors — an inaccessible path is just `false`.
    fn access(&self, path: &str, modes: &str) -> bool;
    /// Resolve symlinks and `.`/`..` to a canonical absolute path.
    fn realpath(&self, path: &str) -> io::Result<String>;
    /// Read a whole file's bytes (backs `vim.fn.readfile`/`readblob`/`nx._readdir`'s
    /// `nx._read_file` is *not* this — that stays local).
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>>;
    /// Resolve `name` to an executable path on the host's `$PATH` (or accept an
    /// explicit executable path). Backs `vim.fn.executable`/`exepath` — the *remote's*
    /// PATH in a daemon session, so LSP server discovery finds the right tool.
    fn which(&self, name: &str) -> Option<String>;
}

/// The default [`LuaFs`]: the real local filesystem via `std::fs` — today's
/// `uvfs.rs`/`host.rs` logic factored behind the seam, so a local session is
/// unchanged. It owns the open-fd table (formerly a `thread_local!` in `uvfs.rs`),
/// behind a `Mutex` so it is `Send + Sync` and can serve as the daemon-side backend
/// too (where requests arrive on a blocking pool thread).
pub struct StdLuaFs {
    fds: Mutex<FdTable>,
}

struct FdTable {
    /// Start at 3 so handed-out fds never visually collide with stdin/stdout/stderr.
    next: i64,
    open: HashMap<i64, File>,
}

impl Default for StdLuaFs {
    fn default() -> Self {
        StdLuaFs {
            fds: Mutex::new(FdTable {
                next: 3,
                open: HashMap::new(),
            }),
        }
    }
}

impl StdLuaFs {
    pub fn new() -> StdLuaFs {
        StdLuaFs::default()
    }

    /// Run `f` against the `File` registered under `fd`, or an `EBADF` error.
    fn with_fd<R>(&self, fd: i64, f: impl FnOnce(&mut File) -> io::Result<R>) -> io::Result<R> {
        let mut t = self.fds.lock().expect("luafs fd table poisoned");
        match t.open.get_mut(&fd) {
            Some(file) => f(file),
            None => Err(io::Error::other(format!(
                "EBADF: bad file descriptor: fd {fd}"
            ))),
        }
    }
}

impl LuaFs for StdLuaFs {
    fn open(&self, path: &str, flags: &str, mode: u32) -> io::Result<i64> {
        let mut opts = open_options(flags).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("EINVAL: invalid open flag '{flags}'"),
            )
        })?;
        apply_mode(&mut opts, mode);
        let file = opts.open(path)?;
        let mut t = self.fds.lock().expect("luafs fd table poisoned");
        let fd = t.next;
        t.next += 1;
        t.open.insert(fd, file);
        Ok(fd)
    }

    fn close(&self, fd: i64) -> io::Result<()> {
        let removed = self
            .fds
            .lock()
            .expect("luafs fd table poisoned")
            .open
            .remove(&fd)
            .is_some();
        if removed {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "EBADF: bad file descriptor: fd {fd}"
            )))
        }
    }

    fn read(&self, fd: i64, size: usize, offset: Option<i64>) -> io::Result<Vec<u8>> {
        self.with_fd(fd, |file| {
            seek_to(file, offset)?;
            let mut buf = vec![0u8; size];
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            Ok(buf)
        })
    }

    fn write(&self, fd: i64, data: &[u8], offset: Option<i64>) -> io::Result<usize> {
        self.with_fd(fd, |file| {
            seek_to(file, offset)?;
            file.write_all(data)?;
            Ok(data.len())
        })
    }

    fn fstat(&self, fd: i64) -> io::Result<LuaStat> {
        self.with_fd(fd, |file| file.metadata())
            .map(|md| LuaStat::from_metadata(&md))
    }

    fn stat(&self, path: &str) -> io::Result<LuaStat> {
        std::fs::metadata(path).map(|md| LuaStat::from_metadata(&md))
    }

    fn lstat(&self, path: &str) -> io::Result<LuaStat> {
        std::fs::symlink_metadata(path).map(|md| LuaStat::from_metadata(&md))
    }

    fn scandir(&self, path: &str) -> io::Result<Vec<LuaDirEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry
                .file_type()
                .map(|ft| {
                    if ft.is_dir() {
                        FileKind::Dir
                    } else if ft.is_symlink() {
                        FileKind::Link
                    } else {
                        FileKind::File
                    }
                })
                .unwrap_or(FileKind::File);
            out.push(LuaDirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        Ok(out)
    }

    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> io::Result<()> {
        mkdir_with_mode(path, mode, recursive)
    }

    fn rmdir(&self, path: &str) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn unlink(&self, path: &str) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn copyfile(&self, src: &str, dest: &str, excl: bool) -> io::Result<()> {
        if excl && std::fs::symlink_metadata(dest).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("EEXIST: file already exists: {dest}"),
            ));
        }
        std::fs::copy(src, dest).map(|_| ())
    }

    fn utime(&self, path: &str, atime: f64, mtime: f64) -> io::Result<()> {
        set_utime(path, atime, mtime)
    }

    fn access(&self, path: &str, modes: &str) -> bool {
        access_ok(path, modes)
    }

    fn realpath(&self, path: &str) -> io::Result<String> {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())
    }

    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn which(&self, name: &str) -> Option<String> {
        find_executable(name)
    }
}

// ----- pure helpers, factored from uvfs.rs / host.rs -------------------------

/// Seek to `offset` when it is a real position; `None`/`-1` leaves the cursor
/// where it is (libuv's "read/write at current offset").
fn seek_to(file: &mut File, offset: Option<i64>) -> io::Result<()> {
    if let Some(o) = offset {
        if o >= 0 {
            file.seek(SeekFrom::Start(o as u64))?;
        }
    }
    Ok(())
}

/// Translate a libuv flag string into `OpenOptions`. Returns `None` for a flag we
/// don't model (the caller turns that into a loud `EINVAL`, never a silent
/// wrong-mode open). A trailing `x` maps to `O_EXCL` (`create_new`).
fn open_options(flags: &str) -> Option<OpenOptions> {
    let excl = flags.ends_with('x');
    let base = flags.strip_suffix('x').unwrap_or(flags);
    let mut o = OpenOptions::new();
    match base {
        "r" => {
            o.read(true);
        }
        "r+" | "rs+" | "sr+" => {
            o.read(true).write(true);
        }
        "w" => {
            o.write(true).truncate(true).create(true);
        }
        "w+" => {
            o.read(true).write(true).truncate(true).create(true);
        }
        "a" => {
            o.write(true).append(true).create(true);
        }
        "a+" => {
            o.read(true).append(true).create(true);
        }
        _ => return None,
    }
    if excl {
        o.create(false).truncate(false).create_new(true);
    }
    Some(o)
}

/// Apply the octal permission to an `OpenOptions` on unix (no-op elsewhere).
fn apply_mode(opts: &mut OpenOptions, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    {
        let _ = (opts, mode);
    }
}

/// The `st_mode` value (file-type bits | permission bits). On unix it is the real
/// value; elsewhere synthesise the type bits plus a conventional permission so
/// `bit.band(S_IFDIR, mode)`-style checks discriminate dir vs file.
fn st_mode(md: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.mode()
    }
    #[cfg(not(unix))]
    {
        const S_IFDIR: u32 = 0o040000;
        const S_IFREG: u32 = 0o100000;
        if md.is_dir() {
            S_IFDIR | 0o755
        } else {
            S_IFREG | 0o644
        }
    }
}

/// Create `path` with permission `mode` — a single level, or with parents when
/// `recursive`. On unix the mode is applied to every directory created.
fn mkdir_with_mode(path: &str, mode: u32, recursive: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(recursive)
            .mode(mode)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::DirBuilder::new().recursive(recursive).create(path)
    }
}

/// Set a file's atime/mtime from fractional-second libuv timestamps.
fn set_utime(path: &str, atime: f64, mtime: f64) -> io::Result<()> {
    let to_time = |secs: f64| {
        if secs >= 0.0 {
            UNIX_EPOCH + Duration::from_secs_f64(secs)
        } else {
            UNIX_EPOCH - Duration::from_secs_f64(-secs)
        }
    };
    let times = std::fs::FileTimes::new()
        .set_accessed(to_time(atime))
        .set_modified(to_time(mtime));
    OpenOptions::new().write(true).open(path)?.set_times(times)
}

/// Whether `path` grants every requested access letter. Existence is required (an
/// absent path is never accessible); each `R`/`W`/`X` is then probed by the natural
/// operation (no libc `access(2)` binding). Empty modes is a pure existence test.
fn access_ok(path: &str, modes: &str) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    modes.chars().all(|c| match c {
        'R' => read_access(path, &meta),
        'W' => !meta.permissions().readonly(),
        'X' => exec_access(path, &meta),
        _ => true, // F_OK / unknown: existence, already confirmed.
    })
}

fn read_access(path: &str, meta: &std::fs::Metadata) -> bool {
    if meta.is_dir() {
        std::fs::read_dir(path).is_ok()
    } else {
        File::open(path).is_ok()
    }
}

fn exec_access(path: &str, meta: &std::fs::Metadata) -> bool {
    if meta.is_dir() {
        std::fs::read_dir(path).is_ok()
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

/// Resolve `name` to an executable path: an explicit path is accepted when it is an
/// executable file; a bare name is searched across `$PATH`.
fn find_executable(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let p = std::path::Path::new(name);
        return is_executable_file(p).then(|| name.to_string());
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    p.is_file()
}
