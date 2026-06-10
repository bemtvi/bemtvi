//! The libuv **filesystem** surface on `vim.uv` / `vim.loop` (`fs_open`,
//! `fs_read`, `fs_write`, `fs_close`, `fs_stat`/`fs_lstat`/`fs_fstat`,
//! `fs_mkdir`/`fs_rmdir`, `fs_unlink`, `fs_rename`, `fs_copyfile`, `fs_utime`).
//!
//! Real plugins — `plenary.path` foremost — bind these *directly* rather than
//! going through `vim.system`, so a `vim.uv` table missing them makes the plugin
//! die with `attempt to call field 'fs_open' (a nil value)` the moment it touches
//! a file. nxvim's Lua VM is single-threaded and synchronous, so each call maps
//! to a blocking `std::fs` operation against the host filesystem (the same model
//! as the rest of the `vim.uv` host primitives in `install.rs`).
//!
//! Open files are tracked in a thread-local descriptor table: `fs_open` returns
//! an integer fd, and `fs_read`/`fs_write`/`fs_fstat`/`fs_close` look the `File`
//! back up by it. The table lives on the VM's thread (the editor + Lua state are
//! intentionally `!Send`, single-thread; see architecture.md), so a plain
//! `thread_local!` needs no locking.
//!
//! These are the **synchronous** forms only (no trailing callback). libuv's
//! async overload — a trailing callback that makes the call return immediately
//! and deliver `cb(err, value)` on a later loop iteration — is layered on top in
//! Lua by `prelude/uv.lua`, which wraps each of these to defer the callback
//! through `vim.schedule` when one is passed. So `plenary.path`'s synchronous
//! methods (`:read`, `:write`, `:mkdir`) bind these directly, and its async
//! readers (`:_read_async`) go through the wrapper.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mlua::{Lua, Table, Value};

use crate::convert::lua_int;
use crate::host::parse_mode;

thread_local! {
    /// Open descriptors handed out by `fs_open`. Keyed by the integer fd we
    /// return to Lua; dropping the `File` (via `fs_close` or table teardown)
    /// closes the OS handle.
    static FDS: RefCell<FdTable> = RefCell::new(FdTable::new());

    /// Live directory iterators handed out by `fs_scandir`. `fs_scandir_next`
    /// pulls the next entry; the iterator is dropped when it is exhausted (a
    /// `next` that returns nil) so a fully-walked handle frees itself.
    static SCANDIRS: RefCell<ScandirTable> = RefCell::new(ScandirTable::new());
}

struct FdTable {
    /// Next fd to hand out. Starts at 3 so our descriptors never visually
    /// collide with the conventional stdin/stdout/stderr 0/1/2 (cosmetic — the
    /// table is private to us, but it keeps logs unsurprising).
    next: i64,
    open: HashMap<i64, File>,
}

impl FdTable {
    fn new() -> Self {
        Self {
            next: 3,
            open: HashMap::new(),
        }
    }
}

struct ScandirTable {
    next: i64,
    open: HashMap<i64, std::fs::ReadDir>,
}

impl ScandirTable {
    fn new() -> Self {
        Self {
            next: 1,
            open: HashMap::new(),
        }
    }
}

/// Install the `fs_*` filesystem functions onto an existing `vim.uv` table.
/// Called from `install_runtime_api` right after the table is created, before it
/// is aliased to `vim.loop`. Note this also (re)defines `fs_stat`, supplying the
/// unix `st_mode` bits in the `mode` field that `plenary.path:is_dir()` needs —
/// the inline `install.rs` stub omitted them.
pub(crate) fn install(lua: &Lua, uv: &Table) -> mlua::Result<()> {
    // ----- stat: by path (follows symlinks), by path (no follow), by fd -------
    uv.set(
        "fs_stat",
        lua.create_function(|lua, path: String| match std::fs::metadata(&path) {
            Ok(md) => Ok(ret_ok(Value::Table(stat_table(lua, &md)?))),
            Err(e) => Ok(ret_err(lua, &path, &e)),
        })?,
    )?;
    uv.set(
        "fs_lstat",
        lua.create_function(|lua, path: String| match std::fs::symlink_metadata(&path) {
            Ok(md) => Ok(ret_ok(Value::Table(stat_table(lua, &md)?))),
            Err(e) => Ok(ret_err(lua, &path, &e)),
        })?,
    )?;
    uv.set(
        "fs_fstat",
        lua.create_function(|lua, fd: i64| match with_fd(fd, |file| file.metadata()) {
            Ok(md) => Ok(ret_ok(Value::Table(stat_table(lua, &md)?))),
            Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
        })?,
    )?;

    // ----- open / close -------------------------------------------------------
    // `fs_open(path, flags, mode)`: `flags` is a libuv mode string ("r", "w",
    // "a", "r+", "w+", "a+", optionally suffixed "x" for O_EXCL); `mode` is the
    // octal permission applied on create. Returns an integer fd, or (nil, err).
    uv.set(
        "fs_open",
        lua.create_function(
            |lua, (path, flags, mode): (String, String, Option<Value>)| {
                let mut opts = match open_options(&flags) {
                    Some(o) => o,
                    None => {
                        return Ok(ret_err_msg(
                            lua,
                            &format!("EINVAL: invalid open flag '{flags}': {path}"),
                        ))
                    }
                };
                apply_mode(&mut opts, parse_mode(mode));
                match opts.open(&path) {
                    Ok(file) => {
                        let fd = FDS.with(|t| {
                            let mut t = t.borrow_mut();
                            let fd = t.next;
                            t.next += 1;
                            t.open.insert(fd, file);
                            fd
                        });
                        Ok(ret_ok(Value::Integer(lua_int(fd))))
                    }
                    Err(e) => Ok(ret_err(lua, &path, &e)),
                }
            },
        )?,
    )?;
    uv.set(
        "fs_close",
        lua.create_function(|lua, fd: i64| {
            let removed = FDS.with(|t| t.borrow_mut().open.remove(&fd).is_some());
            if removed {
                Ok(ret_ok(Value::Boolean(true)))
            } else {
                Ok(ret_err_msg(
                    lua,
                    &format!("EBADF: bad file descriptor: fd {fd}"),
                ))
            }
        })?,
    )?;

    // ----- read / write -------------------------------------------------------
    // `fs_read(fd, size, offset)`: read up to `size` bytes; `offset >= 0` seeks
    // there first, `offset == -1` (or nil) reads at the current position.
    // Returns the data string (empty at EOF), or (nil, err).
    uv.set(
        "fs_read",
        lua.create_function(|lua, (fd, size, offset): (i64, usize, Option<i64>)| {
            let res = with_fd(fd, |file| {
                seek_to(file, offset)?;
                let mut buf = vec![0u8; size];
                let n = file.read(&mut buf)?;
                buf.truncate(n);
                Ok(buf)
            });
            match res {
                Ok(buf) => Ok(ret_ok(Value::String(lua.create_string(&buf)?))),
                Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
            }
        })?,
    )?;
    // `fs_write(fd, data, offset)`: write `data` (a string) at `offset` (or the
    // current position when -1/nil). Returns the byte count, or (nil, err).
    uv.set(
        "fs_write",
        lua.create_function(
            |lua, (fd, data, offset): (i64, mlua::String, Option<i64>)| {
                let bytes = data.as_bytes();
                let res = with_fd(fd, |file| {
                    seek_to(file, offset)?;
                    file.write_all(&bytes)?;
                    Ok(bytes.len())
                });
                match res {
                    Ok(n) => Ok(ret_ok(Value::Integer(lua_int(n as i64)))),
                    Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
                }
            },
        )?,
    )?;

    // ----- directory / link mutations ----------------------------------------
    // `fs_mkdir(path, mode)`: create a *single* directory level (plenary handles
    // its own recursion and branches on this returning nil for EEXIST).
    uv.set(
        "fs_mkdir",
        lua.create_function(|lua, (path, mode): (String, Option<Value>)| {
            let res = mkdir_with_mode(&path, parse_mode(mode));
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;
    uv.set(
        "fs_rmdir",
        lua.create_function(|lua, path: String| {
            Ok(bool_or_err(lua, &path, std::fs::remove_dir(&path)))
        })?,
    )?;
    uv.set(
        "fs_unlink",
        lua.create_function(|lua, path: String| {
            Ok(bool_or_err(lua, &path, std::fs::remove_file(&path)))
        })?,
    )?;
    uv.set(
        "fs_rename",
        lua.create_function(|lua, (from, to): (String, String)| {
            Ok(bool_or_err(lua, &from, std::fs::rename(&from, &to)))
        })?,
    )?;
    // `fs_copyfile(src, dest, opts)`: copy `src` to `dest`. `opts.excl == true`
    // fails when `dest` already exists (libuv's `UV_FS_COPYFILE_EXCL`).
    uv.set(
        "fs_copyfile",
        lua.create_function(|lua, (src, dest, opts): (String, String, Option<Table>)| {
            let excl = opts
                .and_then(|o| o.get::<Option<bool>>("excl").ok().flatten())
                .unwrap_or(false);
            if excl && std::fs::symlink_metadata(&dest).is_ok() {
                return Ok(ret_err_msg(
                    lua,
                    &format!("EEXIST: file already exists: {dest}"),
                ));
            }
            match std::fs::copy(&src, &dest) {
                Ok(_) => Ok(ret_ok(Value::Boolean(true))),
                Err(e) => Ok(ret_err(lua, &src, &e)),
            }
        })?,
    )?;
    // `fs_utime(path, atime, mtime)`: set access/modification times (seconds,
    // possibly fractional). Used by `plenary.path:touch` to bump an existing
    // file's mtime.
    uv.set(
        "fs_utime",
        lua.create_function(|lua, (path, atime, mtime): (String, f64, f64)| {
            let res = set_utime(&path, atime, mtime);
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;

    // ----- directory iteration -----------------------------------------------
    // `fs_access(path, mode)`: whether `path` is accessible for `mode` — a string
    // of `R`/`W`/`X` (or `F` for existence), or the libuv integer bitmask
    // (R_OK=4/W_OK=2/X_OK=1/F_OK=0). Returns a plain boolean (libuv maps the
    // access(2) result to one rather than erroring), which is what
    // `plenary.scandir` tests with `== false` to skip unreadable roots.
    uv.set(
        "fs_access",
        lua.create_function(|_, (path, mode): (String, Value)| {
            Ok(access_ok(&path, &access_modes(&mode)))
        })?,
    )?;
    // `fs_scandir(path)`: open a directory iterator, returning an integer handle
    // (or nil, err). `plenary.scandir` drives it with `fs_scandir_next`.
    uv.set(
        "fs_scandir",
        lua.create_function(|lua, path: String| match std::fs::read_dir(&path) {
            Ok(rd) => {
                let h = SCANDIRS.with(|t| {
                    let mut t = t.borrow_mut();
                    let h = t.next;
                    t.next += 1;
                    t.open.insert(h, rd);
                    h
                });
                Ok(ret_ok(Value::Integer(lua_int(h))))
            }
            Err(e) => Ok(ret_err(lua, &path, &e)),
        })?,
    )?;
    // `fs_scandir_next(handle)`: the next `(name, type)` pair from the iterator,
    // or a bare `nil` when exhausted (libuv's end-of-stream signal — the iterator
    // is then dropped). `type` is the dirent kind ("file"/"directory"/"link"/…),
    // not followed through symlinks (matching libuv).
    uv.set(
        "fs_scandir_next",
        lua.create_function(|lua, handle: i64| Ok(scandir_next(lua, handle)))?,
    )?;

    Ok(())
}

/// Pull the next entry from scandir `handle`. Returns `(name, type)`, a bare
/// `nil` at end-of-stream (dropping the iterator), or `(nil, err)` on a read
/// error.
fn scandir_next(lua: &Lua, handle: i64) -> (Value, Value) {
    let next = SCANDIRS.with(|t| t.borrow_mut().open.get_mut(&handle).map(Iterator::next));
    match next {
        // Unknown handle (already exhausted / never opened): end-of-stream.
        None | Some(None) => {
            SCANDIRS.with(|t| t.borrow_mut().open.remove(&handle));
            (Value::Nil, Value::Nil)
        }
        Some(Some(Ok(entry))) => {
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry
                .file_type()
                .map(|ft| {
                    if ft.is_dir() {
                        "directory"
                    } else if ft.is_symlink() {
                        "link"
                    } else {
                        "file"
                    }
                })
                .unwrap_or("file");
            match (lua.create_string(&name), lua.create_string(kind)) {
                (Ok(n), Ok(k)) => (Value::String(n), Value::String(k)),
                _ => (Value::Nil, Value::Nil),
            }
        }
        Some(Some(Err(e))) => ret_err(lua, &format!("scandir {handle}"), &e),
    }
}

/// Normalise an `fs_access` mode argument to the `R`/`W`/`X` characters it
/// requests. A string passes through (upper-cased); a libuv integer bitmask
/// (R_OK=4, W_OK=2, X_OK=1) expands to the same letters; anything else (F_OK / 0
/// / nil) is existence-only (the empty set).
fn access_modes(mode: &Value) -> String {
    match mode {
        Value::String(s) => s.to_str().map(|s| s.to_uppercase()).unwrap_or_default(),
        Value::Integer(n) => {
            let mut m = String::new();
            if n & 4 != 0 {
                m.push('R');
            }
            if n & 2 != 0 {
                m.push('W');
            }
            if n & 1 != 0 {
                m.push('X');
            }
            m
        }
        _ => String::new(),
    }
}

/// Whether `path` grants every requested access letter. Existence is required
/// (an absent path is never accessible); each `R`/`W`/`X` is then checked. With
/// no letters this is a pure existence (`F_OK`) test. Access is probed by the
/// natural operation rather than a raw `access(2)` (no libc binding): read/exec
/// on a directory is "can list it", read on a file is "can open it", write is
/// "not read-only", exec on a file is the unix exec bit.
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
        // "Execute" on a directory is the right to traverse it — i.e. list it.
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

/// Run `f` against the `File` registered under `fd`, or an `EBADF` error.
fn with_fd<R>(fd: i64, f: impl FnOnce(&mut File) -> std::io::Result<R>) -> std::io::Result<R> {
    FDS.with(|t| {
        let mut t = t.borrow_mut();
        match t.open.get_mut(&fd) {
            Some(file) => f(file),
            None => Err(std::io::Error::other(format!(
                "EBADF: bad file descriptor: fd {fd}"
            ))),
        }
    })
}

/// Seek to `offset` when it is a real position; `None`/`-1` leaves the cursor
/// where it is (libuv's "read/write at current offset").
fn seek_to(file: &mut File, offset: Option<i64>) -> std::io::Result<()> {
    if let Some(o) = offset {
        if o >= 0 {
            file.seek(SeekFrom::Start(o as u64))?;
        }
    }
    Ok(())
}

/// Translate a libuv flag string into `OpenOptions`. Returns `None` for a flag
/// we don't model (the caller turns that into a loud `EINVAL`, never a silent
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
        // O_EXCL: fail if the path already exists. `create_new` implies create,
        // and supersedes plain `create`/`truncate` (truncate is meaningless on a
        // guaranteed-fresh file).
        o.create(false).truncate(false).create_new(true);
    }
    Some(o)
}

/// Build a libuv-shaped stat table. `plenary.path` reads `type`, `size`, and the
/// unix `mode` bits; other consumers read the `{sec,nsec}` time sub-tables, so we
/// fill those too. On unix `mode`/`ino`/`uid`/`gid`/`nlink` come straight from
/// `st_*`; off unix `mode` is synthesised so the `S_IFDIR`/`S_IFREG` masks plugins
/// test still discriminate dir vs file.
fn stat_table(lua: &Lua, md: &std::fs::Metadata) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let kind = if md.is_dir() {
        "directory"
    } else if md.file_type().is_symlink() {
        "link"
    } else {
        "file"
    };
    t.set("type", kind)?;
    t.set("size", md.len())?;
    t.set("mode", st_mode(md))?;
    if let Ok(m) = md.modified() {
        t.set("mtime", time_table(lua, m)?)?;
    }
    if let Ok(a) = md.accessed() {
        t.set("atime", time_table(lua, a)?)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        t.set("ino", md.ino())?;
        t.set("uid", md.uid())?;
        t.set("gid", md.gid())?;
        t.set("nlink", md.nlink())?;
        t.set("dev", md.dev())?;
    }
    Ok(t)
}

/// The `st_mode` value (file-type bits | permission bits). On unix it is the real
/// value; elsewhere we synthesise the type bits (`S_IFDIR`/`S_IFREG`) plus a
/// conventional permission so `bit.band(S_IF.DIR, mode)`-style checks work.
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

/// `{ sec, nsec }` for a `SystemTime`, libuv's timespec shape.
fn time_table(lua: &Lua, t: SystemTime) -> mlua::Result<Table> {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let tt = lua.create_table()?;
    tt.set("sec", d.as_secs() as i64)?;
    tt.set("nsec", d.subsec_nanos() as i64)?;
    Ok(tt)
}

/// `std::fs::create_dir` with the requested permission on unix.
fn mkdir_with_mode(path: &str, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(mode).create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir(path)
    }
}

/// Set a file's atime/mtime from fractional-second libuv timestamps.
fn set_utime(path: &str, atime: f64, mtime: f64) -> std::io::Result<()> {
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

// ----- result shaping: libuv returns `value` on success, `(nil, err, errname)`
// on failure. plenary mostly `assert`s, so a precise `err` string is what shows
// up in a failure message. -----------------------------------------------------

/// `(value, nil)` — success.
fn ret_ok(value: Value) -> (Value, Value) {
    (value, Value::Nil)
}

/// `(nil, "<errno-ish>: <msg>: <path>")` from a real `io::Error`.
fn ret_err(lua: &Lua, path: &str, e: &std::io::Error) -> (Value, Value) {
    ret_err_msg(lua, &format!("{e}: {path}"))
}

/// `(nil, msg)` from a ready-made message.
fn ret_err_msg(lua: &Lua, msg: &str) -> (Value, Value) {
    match lua.create_string(msg) {
        Ok(s) => (Value::Nil, Value::String(s)),
        Err(_) => (Value::Nil, Value::Nil),
    }
}

/// Map a unit `io::Result` to libuv's `true` / `(nil, err)` convention.
fn bool_or_err(lua: &Lua, path: &str, res: std::io::Result<()>) -> (Value, Value) {
    match res {
        Ok(()) => ret_ok(Value::Boolean(true)),
        Err(e) => ret_err(lua, path, &e),
    }
}
