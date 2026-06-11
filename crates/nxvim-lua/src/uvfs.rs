//! The libuv **filesystem** surface on `vim.uv` / `vim.loop` (`fs_open`,
//! `fs_read`, `fs_write`, `fs_close`, `fs_stat`/`fs_lstat`/`fs_fstat`,
//! `fs_mkdir`/`fs_rmdir`, `fs_unlink`, `fs_rename`, `fs_copyfile`, `fs_utime`).
//!
//! Real plugins — `plenary.path` foremost — bind these *directly* rather than
//! going through `vim.system`, so a `vim.uv` table missing them makes the plugin
//! die with `attempt to call field 'fs_open' (a nil value)` the moment it touches
//! a file. nxvim's Lua VM is single-threaded and synchronous, so each call maps to
//! a blocking operation against the host filesystem.
//!
//! Those operations route through the [`LuaFs`](crate::LuaFs) seam (`luafs.rs`), not
//! `std::fs` directly: the default ([`StdLuaFs`](crate::StdLuaFs)) is the local disk
//! (a bare session is unchanged), while a daemon session injects a bridge so a plugin
//! reads the *remote* project (the edit-host split). Open files are referred to by an
//! integer **fd token** the seam mints in `fs_open` and looks back up in
//! `fs_read`/`fs_write`/`fs_fstat`/`fs_close` — so the daemon, not this layer, owns
//! the real `File`. Directory iteration is materialized: `fs_scandir` fetches the
//! whole listing once (one round-trip) and `fs_scandir_next` walks a *local* iterator
//! over it.
//!
//! These are the **synchronous** forms only (no trailing callback). libuv's async
//! overload is layered on top in Lua by `prelude/uv.lua`, which wraps each of these
//! to defer the callback through `vim.schedule` when one is passed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::convert::lua_int;
use crate::host::parse_mode;
use crate::luafs::{LuaDirEntry, LuaStat};
use crate::runtime::{resolve_lua_fs, Shared};

thread_local! {
    /// Live directory iterators handed out by `fs_scandir`. Each holds the
    /// already-materialized listing (fetched in one `LuaFs::scandir` call);
    /// `fs_scandir_next` pulls the next entry locally — no per-entry round-trip.
    /// The iterator is dropped when exhausted (a `next` that returns nil).
    static SCANDIRS: RefCell<ScandirTable> = RefCell::new(ScandirTable::new());
}

struct ScandirTable {
    next: i64,
    open: HashMap<i64, std::vec::IntoIter<LuaDirEntry>>,
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
/// is aliased to `vim.loop`. `shared` is captured so each closure resolves the
/// active [`LuaFs`](crate::LuaFs) backend (local disk, or the daemon bridge).
pub(crate) fn install(lua: &Lua, uv: &Table, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    // ----- stat: by path (follows symlinks), by path (no follow), by fd -------
    let sh = shared.clone();
    uv.set(
        "fs_stat",
        lua.create_function(
            move |lua, path: String| match resolve_lua_fs(&sh).stat(&path) {
                Ok(st) => Ok(ret_ok(Value::Table(stat_table(lua, &st)?))),
                Err(e) => Ok(ret_err(lua, &path, &e)),
            },
        )?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_lstat",
        lua.create_function(
            move |lua, path: String| match resolve_lua_fs(&sh).lstat(&path) {
                Ok(st) => Ok(ret_ok(Value::Table(stat_table(lua, &st)?))),
                Err(e) => Ok(ret_err(lua, &path, &e)),
            },
        )?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_fstat",
        lua.create_function(move |lua, fd: i64| match resolve_lua_fs(&sh).fstat(fd) {
            Ok(st) => Ok(ret_ok(Value::Table(stat_table(lua, &st)?))),
            Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
        })?,
    )?;

    // ----- open / close -------------------------------------------------------
    // `fs_open(path, flags, mode)`: `flags` is a libuv mode string ("r", "w",
    // "a", "r+", "w+", "a+", optionally suffixed "x" for O_EXCL); `mode` is the
    // octal permission applied on create. Returns an integer fd, or (nil, err).
    let sh = shared.clone();
    uv.set(
        "fs_open",
        lua.create_function(
            move |lua, (path, flags, mode): (String, String, Option<Value>)| match resolve_lua_fs(
                &sh,
            )
            .open(&path, &flags, parse_mode(mode))
            {
                Ok(fd) => Ok(ret_ok(Value::Integer(lua_int(fd)))),
                Err(e) => Ok(ret_err(lua, &path, &e)),
            },
        )?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_close",
        lua.create_function(move |lua, fd: i64| match resolve_lua_fs(&sh).close(fd) {
            Ok(()) => Ok(ret_ok(Value::Boolean(true))),
            Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
        })?,
    )?;

    // ----- read / write -------------------------------------------------------
    // `fs_read(fd, size, offset)`: read up to `size` bytes; `offset >= 0` seeks
    // there first, `offset == -1` (or nil) reads at the current position.
    // Returns the data string (empty at EOF), or (nil, err).
    let sh = shared.clone();
    uv.set(
        "fs_read",
        lua.create_function(move |lua, (fd, size, offset): (i64, usize, Option<i64>)| {
            match resolve_lua_fs(&sh).read(fd, size, offset) {
                Ok(buf) => Ok(ret_ok(Value::String(lua.create_string(&buf)?))),
                Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
            }
        })?,
    )?;
    // `fs_write(fd, data, offset)`: write `data` (a string) at `offset` (or the
    // current position when -1/nil). Returns the byte count, or (nil, err).
    let sh = shared.clone();
    uv.set(
        "fs_write",
        lua.create_function(
            move |lua, (fd, data, offset): (i64, mlua::String, Option<i64>)| match resolve_lua_fs(
                &sh,
            )
            .write(fd, &data.as_bytes(), offset)
            {
                Ok(n) => Ok(ret_ok(Value::Integer(lua_int(n as i64)))),
                Err(e) => Ok(ret_err(lua, &format!("fd {fd}"), &e)),
            },
        )?,
    )?;

    // ----- directory / link mutations ----------------------------------------
    // `fs_mkdir(path, mode)`: create a *single* directory level (plenary handles
    // its own recursion and branches on this returning nil for EEXIST).
    let sh = shared.clone();
    uv.set(
        "fs_mkdir",
        lua.create_function(move |lua, (path, mode): (String, Option<Value>)| {
            let res = resolve_lua_fs(&sh).mkdir(&path, parse_mode(mode), false);
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_rmdir",
        lua.create_function(move |lua, path: String| {
            let res = resolve_lua_fs(&sh).rmdir(&path);
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_unlink",
        lua.create_function(move |lua, path: String| {
            let res = resolve_lua_fs(&sh).unlink(&path);
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;
    let sh = shared.clone();
    uv.set(
        "fs_rename",
        lua.create_function(move |lua, (from, to): (String, String)| {
            let res = resolve_lua_fs(&sh).rename(&from, &to);
            Ok(bool_or_err(lua, &from, res))
        })?,
    )?;
    // `fs_copyfile(src, dest, opts)`: copy `src` to `dest`. `opts.excl == true`
    // fails when `dest` already exists (libuv's `UV_FS_COPYFILE_EXCL`).
    let sh = shared.clone();
    uv.set(
        "fs_copyfile",
        lua.create_function(
            move |lua, (src, dest, opts): (String, String, Option<Table>)| {
                let excl = opts
                    .and_then(|o| o.get::<Option<bool>>("excl").ok().flatten())
                    .unwrap_or(false);
                match resolve_lua_fs(&sh).copyfile(&src, &dest, excl) {
                    Ok(()) => Ok(ret_ok(Value::Boolean(true))),
                    Err(e) => Ok(ret_err(lua, &src, &e)),
                }
            },
        )?,
    )?;
    // `fs_utime(path, atime, mtime)`: set access/modification times (seconds,
    // possibly fractional). Used by `plenary.path:touch` to bump an existing
    // file's mtime.
    let sh = shared.clone();
    uv.set(
        "fs_utime",
        lua.create_function(move |lua, (path, atime, mtime): (String, f64, f64)| {
            let res = resolve_lua_fs(&sh).utime(&path, atime, mtime);
            Ok(bool_or_err(lua, &path, res))
        })?,
    )?;

    // ----- directory iteration -----------------------------------------------
    // `fs_access(path, mode)`: whether `path` is accessible for `mode` — a string
    // of `R`/`W`/`X` (or `F` for existence), or the libuv integer bitmask
    // (R_OK=4/W_OK=2/X_OK=1/F_OK=0). Returns a plain boolean (libuv maps the
    // access(2) result to one rather than erroring), which is what
    // `plenary.scandir` tests with `== false` to skip unreadable roots.
    let sh = shared.clone();
    uv.set(
        "fs_access",
        lua.create_function(move |_, (path, mode): (String, Value)| {
            Ok(resolve_lua_fs(&sh).access(&path, &access_modes(&mode)))
        })?,
    )?;
    // `fs_scandir(path)`: fetch the directory listing in one shot and hand back an
    // integer handle over a *local* iterator (or nil, err). `plenary.scandir`
    // drives it with `fs_scandir_next`.
    let sh = shared.clone();
    uv.set(
        "fs_scandir",
        lua.create_function(
            move |lua, path: String| match resolve_lua_fs(&sh).scandir(&path) {
                Ok(entries) => {
                    let h = SCANDIRS.with(|t| {
                        let mut t = t.borrow_mut();
                        let h = t.next;
                        t.next += 1;
                        t.open.insert(h, entries.into_iter());
                        h
                    });
                    Ok(ret_ok(Value::Integer(lua_int(h))))
                }
                Err(e) => Ok(ret_err(lua, &path, &e)),
            },
        )?,
    )?;
    // `fs_scandir_next(handle)`: the next `(name, type)` pair from the iterator,
    // or a bare `nil` when exhausted (libuv's end-of-stream signal — the iterator
    // is then dropped). `type` is the dirent kind ("file"/"directory"/"link"),
    // not followed through symlinks (matching libuv).
    uv.set(
        "fs_scandir_next",
        lua.create_function(|lua, handle: i64| Ok(scandir_next(lua, handle)))?,
    )?;

    Ok(())
}

/// Pull the next entry from scandir `handle`. Returns `(name, type)`, a bare `nil`
/// at end-of-stream (dropping the iterator), or `(nil, nil)` on a string-conversion
/// failure (vanishingly rare — a non-UTF-8 name already lossily converted).
fn scandir_next(lua: &Lua, handle: i64) -> (Value, Value) {
    let next = SCANDIRS.with(|t| t.borrow_mut().open.get_mut(&handle).map(Iterator::next));
    match next {
        // Unknown handle (already exhausted / never opened): end-of-stream.
        None | Some(None) => {
            SCANDIRS.with(|t| t.borrow_mut().open.remove(&handle));
            (Value::Nil, Value::Nil)
        }
        Some(Some(entry)) => match (
            lua.create_string(&entry.name),
            lua.create_string(entry.kind.as_str()),
        ) {
            (Ok(n), Ok(k)) => (Value::String(n), Value::String(k)),
            _ => (Value::Nil, Value::Nil),
        },
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

/// Build a libuv-shaped stat table from a [`LuaStat`]. `plenary.path` reads `type`,
/// `size`, and the unix `mode` bits; other consumers read the `{sec,nsec}` time
/// sub-tables and the `ino`/`uid`/`gid`/`nlink`/`dev` extras, so all are filled.
fn stat_table(lua: &Lua, st: &LuaStat) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("type", st.kind.as_str())?;
    t.set("size", st.size)?;
    t.set("mode", st.mode)?;
    if let Some((sec, nsec)) = st.mtime {
        t.set("mtime", time_table(lua, sec, nsec)?)?;
    }
    if let Some((sec, nsec)) = st.atime {
        t.set("atime", time_table(lua, sec, nsec)?)?;
    }
    t.set("ino", st.ino)?;
    t.set("uid", st.uid)?;
    t.set("gid", st.gid)?;
    t.set("nlink", st.nlink)?;
    t.set("dev", st.dev)?;
    Ok(t)
}

/// `{ sec, nsec }`, libuv's timespec shape.
fn time_table(lua: &Lua, sec: i64, nsec: u32) -> mlua::Result<Table> {
    let tt = lua.create_table()?;
    tt.set("sec", sec)?;
    tt.set("nsec", nsec as i64)?;
    Ok(tt)
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
