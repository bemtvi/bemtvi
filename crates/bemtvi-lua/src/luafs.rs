//! The **filesystem** seam behind the async `btv.fs` API — the only fs surface Lua
//! has (there are no synchronous Lua fs builtins; blocking IO never runs on the
//! editor thread).
//!
//! Each `btv.fs` call becomes a whole high-level [`FsJob`] that the server runs
//! **off the editor tick**, on the machine that actually holds the project files:
//! a bare native session decomposes it via [`run_fs_job`] against [`StdLuaFs`] on
//! the blocking pool, while a daemon session ships the job across the wire in one
//! `luafs_op` request and it decomposes daemon-side — the same leg the wasm
//! edit-host uses (see `bemtvi-server`'s `FsBackend`; never a per-primitive wire
//! round-trip). The [`LuaFs`] trait models the full libuv-shaped operation set so
//! both worlds serve one surface; the plugin manager's clone/discover/source jobs
//! run against the *local* backend even in a daemon session (plugins load into the
//! local Lua VM).
//!
//! **Stays local, off this seam entirely** (config / plugin / VM state): raw Lua
//! `io.*` / `os.*`, `require` / `package.path`, `nvim_get_runtime_file`
//! (runtimepath = local plugins), `btv._read_file` (sources an `lsp/<name>.lua`
//! *config* found on the runtimepath), and `stdpath` (local config/data/state
//! dirs). These keep their direct `std::fs` calls — plugins and config live on
//! the local machine by design.

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
    /// Read a whole file's bytes (backs `vim.fn.readfile`/`readblob`/`btv._readdir`'s
    /// `btv._read_file` is *not* this — that stays local).
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
    let to_time = |secs: f64| -> io::Result<std::time::SystemTime> {
        if !secs.is_finite() {
            // `Duration::from_secs_f64` panics on NaN/±inf, and the timestamps
            // arrive from Lua (`math.huge`, `0/0`) or a remote fs op — fail as
            // a recoverable error instead of panicking the server thread.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("EINVAL: utime: non-finite timestamp {secs}"),
            ));
        }
        Ok(if secs >= 0.0 {
            UNIX_EPOCH + Duration::from_secs_f64(secs)
        } else {
            UNIX_EPOCH - Duration::from_secs_f64(-secs)
        })
    };
    let times = std::fs::FileTimes::new()
        .set_accessed(to_time(atime)?)
        .set_modified(to_time(mtime)?);
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

// ===== the off-tick op executor =============================================
// `btv.fs.*` ops run off the editor tick: the `btv._fs_op` bridge queues an [`FsJob`]
// and the event-loop actor (native) / daemon leg (wasm) runs it here, against the
// active [`LuaFs`]. This is the synchronous heart of the op — it returns the typed
// [`FsValue`] / [`FsError`] the runtime marshals into the resolved / rejected Lua
// value once, regardless of where it ran. (The previous inline `btv._fs_*` bridges in
// install.rs are gone; this is their semantics, lifted off the tick.)

use crate::ops::{FsError, FsJob, FsValue};

/// Run one [`FsJob`] through `fs`, returning the typed success value or the
/// `{ code, message }` error. Pure and synchronous — safe to call from a blocking
/// pool thread (the native actor's `spawn_blocking`) or the daemon. `read_text`
/// transcodes through `encoding_rs`, failing loud (EILSEQ) on invalid bytes — never
/// lossy replacement text.
pub fn run_fs_job(fs: &dyn LuaFs, job: &FsJob) -> Result<FsValue, FsError> {
    match job {
        FsJob::Stat { path } => fs.stat(path).map(FsValue::Stat).map_err(fs_error),
        FsJob::Lstat { path } => fs.lstat(path).map(FsValue::Stat).map_err(fs_error),
        // Existence of the entry itself (`lstat` — a dangling symlink still "exists").
        // Never errors, per the promise-always `exists` contract.
        FsJob::Exists { path } => Ok(FsValue::Bool(fs.lstat(path).is_ok())),
        FsJob::Readdir { path } => fs.scandir(path).map(FsValue::Dir).map_err(fs_error),
        FsJob::Read { path } => fs.read_file(path).map(FsValue::Bytes).map_err(fs_error),
        FsJob::ReadText { path, encoding } => {
            let bytes = fs.read_file(path).map_err(fs_error)?;
            // Decode through the encoding seam (default UTF-8). A label we don't know
            // or bytes that don't decode are HARD errors — `read_text` never returns
            // lossy replacement-char text (use `btv.fs.read` for raw bytes).
            let Some(enc) = encoding_rs::Encoding::for_label(encoding.as_bytes()) else {
                return Err(FsError {
                    code: "EINVAL".into(),
                    message: format!("unknown encoding '{encoding}'"),
                });
            };
            let (text, _, had_errors) = enc.decode(&bytes);
            if had_errors {
                return Err(FsError {
                    code: "EILSEQ".into(),
                    message: format!("invalid {encoding} byte sequence in '{path}'"),
                });
            }
            Ok(FsValue::Text(text.into_owned()))
        }
        FsJob::Write { path, data } => write_whole(fs, path, data, false)
            .map(|()| FsValue::Nil)
            .map_err(fs_error),
        FsJob::Append { path, data } => write_whole(fs, path, data, true)
            .map(|()| FsValue::Nil)
            .map_err(fs_error),
        FsJob::Mkdir {
            path,
            recursive,
            mode,
        } => fs
            .mkdir(path, *mode, *recursive)
            .map(|()| FsValue::Nil)
            .map_err(fs_error),
        FsJob::Rename { from, to } => fs.rename(from, to).map(|()| FsValue::Nil).map_err(fs_error),
        FsJob::Remove { path, recursive } => remove_path(fs, path, *recursive)
            .map(|()| FsValue::Nil)
            .map_err(fs_error),
        FsJob::Copy {
            src,
            dst,
            recursive,
        } => copy_path(fs, src, dst, *recursive)
            .map(|()| FsValue::Nil)
            .map_err(fs_error),
        FsJob::Realpath { path } => fs.realpath(path).map(FsValue::Text).map_err(fs_error),
        // Not-found is `nil`, never a rejection: a config asking "is the local
        // `node_modules/.bin` copy there?" gets a plain no, and only a *transport*
        // failure would be an error.
        FsJob::Which { name } => Ok(fs.which(name).map_or(FsValue::Nil, FsValue::Text)),
        FsJob::HashFile { path, algo } => hash_file(fs, path, algo).map(FsValue::Text),
    }
}

/// Stream `path` through `algo` and return its lowercase-hex digest. Reads the file
/// in fixed 64 KiB chunks off the fd seam and folds each into the hasher, so peak
/// memory is one chunk regardless of file size — hashing a 300 MB file costs 64 KiB,
/// not 300 MB. An unknown `algo` is a loud `EINVAL` (never a silent wrong digest).
fn hash_file(fs: &dyn LuaFs, path: &str, algo: &str) -> Result<String, FsError> {
    // 64 KiB — large enough to amortize syscall / wire overhead, small enough that
    // the transient buffer never shows up in a memory profile.
    const CHUNK: usize = 64 * 1024;

    // Reject an unknown algorithm *before* opening the file, so a typo fails the same
    // way whether or not the path exists.
    let mut hasher = new_digest(algo).ok_or_else(|| FsError {
        code: "EINVAL".into(),
        message: format!("btv.hash.file: unknown algorithm '{algo}'"),
    })?;

    let fd = fs.open(path, "r", 0).map_err(fs_error)?;
    // Fold the stream, then always close the fd — even on a mid-read error — so a
    // failed hash can't leak a descriptor.
    let result = (|| {
        loop {
            let chunk = fs.read(fd, CHUNK, None)?;
            if chunk.is_empty() {
                break; // EOF
            }
            hasher.update(&chunk);
        }
        Ok(hasher.hex_digest())
    })();
    let _ = fs.close(fd);
    result.map_err(fs_error)
}

/// Construct a boxed incremental hasher for `algo`, or `None` if the name is unknown.
/// Shared by the streaming `btv.hash.file` fs op (here) and the incremental `btv.hash.new`
/// object (`install.rs`), so the same four algorithm names mean the same thing in both.
pub(crate) fn new_digest(algo: &str) -> Option<Box<dyn DigestStream>> {
    use sha2::Digest as _;
    Some(match algo {
        "sha1" => Box::new(sha1::Sha1::new()),
        "sha256" => Box::new(sha2::Sha256::new()),
        "sha512" => Box::new(sha2::Sha512::new()),
        "md5" => Box::new(md5::Md5::new()),
        _ => return None,
    })
}

/// Object-safe view over a RustCrypto hasher so a hasher can be picked by algorithm name
/// at runtime without threading the concrete `Digest` type through. (The `digest` crate's
/// own `DynDigest` needs its `alloc` feature; this keeps the dependency surface to the
/// `Digest` trait the hashers already pull in.)
///
/// `hex_digest` takes `&self` (it clones, then finalizes the clone) rather than consuming,
/// so the incremental `btv.hash.new` object can read an intermediate digest and keep
/// feeding chunks afterward — and so one trait serves both the one-finalize file path and
/// the read-many object. Every RustCrypto hasher is `Clone`, so the bound always holds.
pub(crate) trait DigestStream {
    fn update(&mut self, data: &[u8]);
    fn hex_digest(&self) -> String;
}

impl<D: sha2::Digest + Clone> DigestStream for D {
    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(self, data);
    }
    fn hex_digest(&self) -> String {
        to_hex(&self.clone().finalize())
    }
}

/// Lowercase-hex encode a digest. Shared by the streaming `btv.hash.file` path and the
/// one-shot `btv.hash.*` natives in `install.rs`, so the two render digests identically.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Shape an [`io::Error`] into the `{ code, message }` an `btv.fs` reject carries.
fn fs_error(e: io::Error) -> FsError {
    FsError {
        code: errno_code(&e),
        message: e.to_string(),
    }
}

/// A libuv/errno-style code hint for a fs error (`btv.fs` rejections carry it as
/// `err.code`). Prefers the stable [`io::ErrorKind`] mapping; for the kinds std
/// still has no stable variant (`ENOTEMPTY`/`ENOTDIR`/`EISDIR`) it reads the raw OS
/// errno where recognized, and surfaces the number (`E<n>`) rather than guess a name.
fn errno_code(e: &io::Error) -> String {
    use io::ErrorKind as K;
    let named = match e.kind() {
        K::NotFound => "ENOENT",
        K::PermissionDenied => "EACCES",
        K::AlreadyExists => "EEXIST",
        K::InvalidInput => "EINVAL",
        K::InvalidData => "EILSEQ",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    match e.raw_os_error() {
        Some(20) => "ENOTDIR".to_string(),
        Some(21) => "EISDIR".to_string(),
        Some(28) => "ENOSPC".to_string(),
        Some(39 | 66) => "ENOTEMPTY".to_string(), // linux 39, macos 66
        Some(n) => format!("E{n}"),
        None => "EIO".to_string(),
    }
}

/// Write `data` to `path` as one whole file (truncate, or append when `append`),
/// through the fd-based [`LuaFs`] seam. Closes the fd even on a write error, and
/// loops on short writes so a partial-write seam can never silently truncate.
fn write_whole(fs: &dyn LuaFs, path: &str, data: &[u8], append: bool) -> io::Result<()> {
    let flags = if append { "a" } else { "w" };
    let fd = fs.open(path, flags, 0o644)?;
    let res = (|| {
        let mut off = 0usize;
        while off < data.len() {
            let n = fs.write(fd, &data[off..], None)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write made no progress",
                ));
            }
            off += n;
        }
        Ok(())
    })();
    let _ = fs.close(fd);
    res
}

/// Remove `path`: a file via `unlink`, a directory via `rmdir` (emptying it first
/// when `recursive`). Uses `lstat`, so a symlink to a directory is unlinked, not
/// walked into.
fn remove_path(fs: &dyn LuaFs, path: &str, recursive: bool) -> io::Result<()> {
    // Never remove the filesystem root: the recursive walk of `/` — a stray
    // `btv.fs.remove("/", { recursive = true })`, or an LSP workspace edit
    // deleting `file:///` — would take the whole disk with it. Everything
    // *below* the root is the ordinary delete contract; the root itself never
    // is (no editor feature has a legitimate reason to delete it).
    if path == "/" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "EINVAL: refusing to remove the filesystem root '/'",
        ));
    }
    let st = fs.lstat(path)?;
    if st.kind == FileKind::Dir {
        if recursive {
            for e in fs.scandir(path)? {
                let child = std::path::Path::new(path).join(&e.name);
                remove_path(fs, &child.to_string_lossy(), true)?;
            }
        }
        fs.rmdir(path)
    } else {
        fs.unlink(path)
    }
}

/// Copy `src` to `dst`: a file via `copyfile` (overwriting), or a directory tree
/// when `recursive`. A directory `src` without `recursive` is an error, not a silent
/// skip.
fn copy_path(fs: &dyn LuaFs, src: &str, dst: &str, recursive: bool) -> io::Result<()> {
    let st = fs.lstat(src)?;
    if st.kind == FileKind::Dir {
        if !recursive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{src}' is a directory (pass {{ recursive = true }})"),
            ));
        }
        fs.mkdir(dst, 0o755, true)?;
        for e in fs.scandir(src)? {
            let cs = std::path::Path::new(src).join(&e.name);
            let cd = std::path::Path::new(dst).join(&e.name);
            copy_path(fs, &cs.to_string_lossy(), &cd.to_string_lossy(), true)?;
        }
        Ok(())
    } else {
        fs.copyfile(src, dst, false)
    }
}
