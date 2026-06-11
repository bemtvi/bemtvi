//! The daemon wire protocol — the **Lua-visible filesystem** leg (edit-host split,
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → *The full split*,
//! *Lua-visible filesystem semantics*).
//!
//! Companion to `daemon_system.rs` (the blocking `vim.system` shell-out). Here a real
//! editor whose `lua_fs` backend is a [`RemoteLuaFs`](nxvim_server::RemoteLuaFs) talking
//! to a [`serve_luafs_daemon`](nxvim_server::serve_luafs_daemon) over an in-process
//! duplex runs the project-facing Lua fs surface (`vim.uv.fs_*`, `vim.fn.readblob` /
//! `filereadable` / `executable`), and the contract holds:
//!
//! - The fs calls hit the **daemon**, not the edit-host's local disk: the virtual fs
//!   serves `/virtual/...` content that exists on *no* local machine, so observing it
//!   proves the call crossed the wire (the `/virtual/...` faithfulness argument the rest
//!   of the daemon suite makes).
//! - An `fs_open` mints a remote **fd token** the daemon holds; a later `fs_read` on that
//!   token reads the daemon-held `File` — the token round-trips.
//! - A **mutation** (`fs_mkdir`) lands in the daemon's store, observable on a follow-up
//!   `fs_stat`.
//! - The backend **reacts to input** (distinct paths → distinct results), and a
//!   **negative control** (no daemon → the default local `StdLuaFs`) flips every probe to
//!   a local miss — proving the daemon is what makes it work.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on the
//! `exec_lua` result the daemon produced.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;

use nxvim_lua::{FileKind, LuaDirEntry, LuaFs, LuaStat};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteLuaFs, ServerInit};
use nxvim_test_harness::{attach, exec_lua, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// A sentinel mtime the local disk would never coincidentally produce — observing it on a
/// `/virtual/...` stat proves the value came from the daemon, not a real file.
const SENTINEL_MTIME: i64 = 1_700_000_000;

/// An in-memory **virtual filesystem** serving `/virtual/...` content that exists on no
/// real disk. Faithful, not a no-op: every reply is derived from the request — stat sizes
/// come from the stored bytes, reads return the stored bytes, a mkdir mutates the store.
struct VirtualFs {
    inner: Mutex<VfsState>,
}

struct VfsState {
    files: HashMap<String, Vec<u8>>,
    dirs: HashSet<String>,
    /// Executables on the daemon's "PATH" (for `which`/`vim.fn.executable`).
    execs: HashSet<String>,
    next_fd: i64,
    /// Open fds: token → (path, cursor). The daemon owns this — the edit-host only holds
    /// the `i64` token.
    open: HashMap<i64, (String, usize)>,
}

impl VirtualFs {
    fn seeded() -> VirtualFs {
        let mut files = HashMap::new();
        files.insert(
            "/virtual/hello.txt".to_string(),
            b"from the daemon\n".to_vec(),
        );
        files.insert("/virtual/sub/inner.txt".to_string(), b"inner".to_vec());
        let mut dirs = HashSet::new();
        dirs.insert("/virtual".to_string());
        dirs.insert("/virtual/sub".to_string());
        let mut execs = HashSet::new();
        execs.insert("daemon-only-tool".to_string());
        VirtualFs {
            inner: Mutex::new(VfsState {
                files,
                dirs,
                execs,
                next_fd: 3,
                open: HashMap::new(),
            }),
        }
    }
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("ENOENT: {path}"))
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

fn file_stat(size: u64) -> LuaStat {
    LuaStat {
        kind: FileKind::File,
        size,
        mode: 0o100644,
        mtime: Some((SENTINEL_MTIME, 0)),
        atime: Some((SENTINEL_MTIME, 0)),
        ino: 0,
        uid: 0,
        gid: 0,
        nlink: 1,
        dev: 0,
    }
}

fn dir_stat() -> LuaStat {
    LuaStat {
        kind: FileKind::Dir,
        size: 0,
        mode: 0o040755,
        mtime: Some((SENTINEL_MTIME, 0)),
        atime: Some((SENTINEL_MTIME, 0)),
        ino: 0,
        uid: 0,
        gid: 0,
        nlink: 2,
        dev: 0,
    }
}

impl LuaFs for VirtualFs {
    fn open(&self, path: &str, flags: &str, _mode: u32) -> io::Result<i64> {
        let mut s = self.inner.lock().unwrap();
        let creating = flags.starts_with('w') || flags.starts_with('a');
        if !s.files.contains_key(path) {
            if creating {
                s.files.insert(path.to_string(), Vec::new());
            } else {
                return Err(not_found(path));
            }
        }
        let fd = s.next_fd;
        s.next_fd += 1;
        s.open.insert(fd, (path.to_string(), 0));
        Ok(fd)
    }

    fn close(&self, fd: i64) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        s.open
            .remove(&fd)
            .map(|_| ())
            .ok_or_else(|| io::Error::other(format!("EBADF: fd {fd}")))
    }

    fn read(&self, fd: i64, size: usize, offset: Option<i64>) -> io::Result<Vec<u8>> {
        let mut s = self.inner.lock().unwrap();
        let (path, cursor) = s
            .open
            .get(&fd)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("EBADF: fd {fd}")))?;
        let content = s.files.get(&path).cloned().unwrap_or_default();
        let start = offset
            .filter(|o| *o >= 0)
            .map(|o| o as usize)
            .unwrap_or(cursor);
        let end = (start + size).min(content.len());
        let out = content.get(start..end).unwrap_or(&[]).to_vec();
        if let Some(entry) = s.open.get_mut(&fd) {
            entry.1 = end;
        }
        Ok(out)
    }

    fn write(&self, fd: i64, data: &[u8], _offset: Option<i64>) -> io::Result<usize> {
        let mut s = self.inner.lock().unwrap();
        let path = s
            .open
            .get(&fd)
            .map(|(p, _)| p.clone())
            .ok_or_else(|| io::Error::other(format!("EBADF: fd {fd}")))?;
        s.files.entry(path).or_default().extend_from_slice(data);
        Ok(data.len())
    }

    fn fstat(&self, fd: i64) -> io::Result<LuaStat> {
        let path = {
            let s = self.inner.lock().unwrap();
            s.open
                .get(&fd)
                .map(|(p, _)| p.clone())
                .ok_or_else(|| io::Error::other(format!("EBADF: fd {fd}")))?
        };
        self.stat(&path)
    }

    fn stat(&self, path: &str) -> io::Result<LuaStat> {
        let s = self.inner.lock().unwrap();
        if let Some(content) = s.files.get(path) {
            Ok(file_stat(content.len() as u64))
        } else if s.dirs.contains(path) {
            Ok(dir_stat())
        } else {
            Err(not_found(path))
        }
    }

    fn lstat(&self, path: &str) -> io::Result<LuaStat> {
        self.stat(path)
    }

    fn scandir(&self, path: &str) -> io::Result<Vec<LuaDirEntry>> {
        let s = self.inner.lock().unwrap();
        if !s.dirs.contains(path) {
            return Err(not_found(path));
        }
        let mut out = Vec::new();
        for (p, _) in s.files.iter() {
            if parent_of(p) == path {
                out.push(LuaDirEntry {
                    name: p.rsplit('/').next().unwrap().to_string(),
                    kind: FileKind::File,
                });
            }
        }
        for d in s.dirs.iter() {
            if d != path && parent_of(d) == path {
                out.push(LuaDirEntry {
                    name: d.rsplit('/').next().unwrap().to_string(),
                    kind: FileKind::Dir,
                });
            }
        }
        Ok(out)
    }

    fn mkdir(&self, path: &str, _mode: u32, _recursive: bool) -> io::Result<()> {
        self.inner.lock().unwrap().dirs.insert(path.to_string());
        Ok(())
    }

    fn rmdir(&self, path: &str) -> io::Result<()> {
        self.inner.lock().unwrap().dirs.remove(path);
        Ok(())
    }

    fn unlink(&self, path: &str) -> io::Result<()> {
        self.inner.lock().unwrap().files.remove(path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        if let Some(content) = s.files.remove(from) {
            s.files.insert(to.to_string(), content);
            Ok(())
        } else {
            Err(not_found(from))
        }
    }

    fn copyfile(&self, src: &str, dest: &str, _excl: bool) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        let content = s.files.get(src).cloned().ok_or_else(|| not_found(src))?;
        s.files.insert(dest.to_string(), content);
        Ok(())
    }

    fn utime(&self, _path: &str, _atime: f64, _mtime: f64) -> io::Result<()> {
        Ok(())
    }

    fn access(&self, path: &str, _modes: &str) -> bool {
        let s = self.inner.lock().unwrap();
        s.files.contains_key(path) || s.dirs.contains(path)
    }

    fn realpath(&self, path: &str) -> io::Result<String> {
        if self.access(path, "") {
            Ok(path.to_string())
        } else {
            Err(not_found(path))
        }
    }

    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        self.inner
            .lock()
            .unwrap()
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| not_found(path))
    }

    fn which(&self, name: &str) -> Option<String> {
        let s = self.inner.lock().unwrap();
        s.execs
            .contains(name)
            .then(|| format!("/remote/bin/{name}"))
    }
}

/// Start a server whose `lua_fs` is a [`RemoteLuaFs`] talking to a `serve_luafs_daemon`
/// (backed by a fresh [`VirtualFs`]) over an in-process duplex. UI-attached. The
/// notification receiver is returned (not dropped — dropping it tears the client
/// connection down and stops the server).
async fn spawn_with_daemon_fs() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_luafs_daemon(
            daemon_reader,
            daemon_writer,
            Box::new(VirtualFs::seeded()),
        )
        .await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteLuaFs::connect(host_reader, host_writer);
    let init = ServerInit {
        lua_fs: Some(Box::new(remote)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// `vim.uv.fs_stat` resolves against the **daemon**: the `/virtual/...` file exists on no
/// local disk, yet its size + sentinel mtime + type come back — proving the call crossed
/// the wire (a real local stat would return nil).
#[tokio::test]
async fn fs_stat_resolves_on_the_daemon() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let result = exec_lua(
        &rpc,
        r#"
        local st = vim.uv.fs_stat("/virtual/hello.txt")
        return { st.type, st.size, st.mtime.sec }
        "#,
    )
    .await;

    let a = result.as_array().expect("stat table");
    assert_eq!(a[0].as_str(), Some("file"));
    assert_eq!(
        a[1].as_u64(),
        Some(16),
        "size came from the daemon-held bytes"
    );
    assert_eq!(
        a[2].as_i64(),
        Some(SENTINEL_MTIME),
        "the daemon's sentinel mtime — a local file would never coincide"
    );
}

/// `fs_open` + `fs_read` + `fs_close`: the open mints a remote fd token the daemon holds,
/// and the read on that token returns the daemon-held bytes — the token round-trips.
#[tokio::test]
async fn fs_open_read_close_round_trips_the_fd_token() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let content = exec_lua(
        &rpc,
        r#"
        local fd = assert(vim.uv.fs_open("/virtual/hello.txt", "r", 0))
        local data = vim.uv.fs_read(fd, 4096, 0)
        vim.uv.fs_close(fd)
        return data
        "#,
    )
    .await;

    assert_eq!(content.as_str(), Some("from the daemon\n"));
}

/// `fs_scandir` + `fs_scandir_next` enumerate the daemon's directory — names a local
/// listing of a nonexistent dir could not invent.
#[tokio::test]
async fn fs_scandir_enumerates_the_daemon_directory() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let names = exec_lua(
        &rpc,
        r#"
        local h = assert(vim.uv.fs_scandir("/virtual"))
        local out = {}
        while true do
          local name, ty = vim.uv.fs_scandir_next(h)
          if not name then break end
          out[#out + 1] = name .. ":" .. ty
        end
        table.sort(out)
        return table.concat(out, ",")
        "#,
    )
    .await;

    assert_eq!(names.as_str(), Some("hello.txt:file,sub:directory"));
}

/// The `vim.fn` fs builtins route to the daemon too: `readblob` reads the remote bytes,
/// `filereadable` sees the remote file, and `executable` resolves against the *remote's*
/// PATH (the tool is not on the local one).
#[tokio::test]
async fn vim_fn_fs_builtins_resolve_on_the_daemon() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let result = exec_lua(
        &rpc,
        r#"
        return {
          vim.fn.readblob("/virtual/sub/inner.txt"),
          vim.fn.filereadable("/virtual/hello.txt"),
          vim.fn.filereadable("/virtual/nope.txt"),
          vim.fn.executable("daemon-only-tool"),
          vim.fn.exepath("daemon-only-tool"),
        }
        "#,
    )
    .await;

    let a = result.as_array().expect("result table");
    assert_eq!(a[0].as_str(), Some("inner"), "readblob crossed the wire");
    assert_eq!(a[1].as_i64(), Some(1), "the remote file is readable");
    assert_eq!(a[2].as_i64(), Some(0), "a missing remote file is not");
    assert_eq!(
        a[3].as_i64(),
        Some(1),
        "the tool is on the remote PATH (not the local one)"
    );
    assert_eq!(a[4].as_str(), Some("/remote/bin/daemon-only-tool"));
}

/// A **mutation** lands on the daemon: `fs_mkdir` creates a remote directory, observable
/// on a follow-up `fs_stat` — the write crossed the wire and stuck.
#[tokio::test]
async fn fs_mkdir_mutates_the_daemon_store() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let kind = exec_lua(
        &rpc,
        r#"
        assert(vim.uv.fs_mkdir("/virtual/created", 493))
        return vim.uv.fs_stat("/virtual/created").type
        "#,
    )
    .await;

    assert_eq!(kind.as_str(), Some("directory"));
}

/// Distinct paths yield distinct results — the bridge relays each call's own argument, not
/// a shared/canned constant (the "reacts to input" guard against a faithful-looking no-op).
#[tokio::test]
async fn each_call_relays_its_own_path() {
    let (rpc, _incoming) = spawn_with_daemon_fs().await;

    let first = exec_lua(&rpc, r#"return vim.uv.fs_stat("/virtual/hello.txt").size"#).await;
    let second = exec_lua(
        &rpc,
        r#"return vim.uv.fs_stat("/virtual/sub/inner.txt").size"#,
    )
    .await;

    assert_eq!(first.as_u64(), Some(16));
    assert_eq!(second.as_u64(), Some(5));
}

/// **Negative control:** with no daemon injected (the default local `StdLuaFs`), the same
/// `/virtual/...` probe is a local miss — `fs_stat` returns nil. Proves the passing tests
/// above genuinely depend on the wire, not on some ambient state.
#[tokio::test]
async fn without_the_daemon_virtual_paths_miss_locally() {
    let (rpc, _incoming) = {
        let (rpc, incoming) = spawn(ServerInit::default());
        attach(&rpc, 80, 24).await;
        (rpc, incoming)
    };

    let missing = exec_lua(
        &rpc,
        r#"return vim.uv.fs_stat("/virtual/hello.txt") == nil"#,
    )
    .await;

    assert_eq!(
        missing.as_bool(),
        Some(true),
        "no daemon → the local StdLuaFs has no /virtual/... file"
    );
}

/// **Local refactor is behavior-preserving:** with no daemon (the default `StdLuaFs`), the
/// routed `vim.uv.fs_*` surface still works against the real disk — write a file via
/// `fs_open`/`fs_write`, read it back, `fs_stat` it, `fs_mkdir` a subdir, and `fs_scandir`
/// the tree. The fd token persists across calls (the default's own fd table).
#[tokio::test]
async fn local_std_lua_fs_round_trips_against_a_real_dir() {
    let (rpc, _incoming) = {
        let (rpc, incoming) = spawn(ServerInit::default());
        attach(&rpc, 80, 24).await;
        (rpc, incoming)
    };
    let dir = temp_dir("luafs_local");
    let dir = dir.to_string_lossy().into_owned();

    let result = exec_lua(
        &rpc,
        &format!(
            r#"
            local dir = "{dir}"
            local fpath = dir .. "/note.txt"
            local fd = assert(vim.uv.fs_open(fpath, "w", 420))
            vim.uv.fs_write(fd, "local bytes")
            vim.uv.fs_close(fd)

            local rfd = assert(vim.uv.fs_open(fpath, "r", 0))
            local data = vim.uv.fs_read(rfd, 4096, 0)
            vim.uv.fs_close(rfd)

            assert(vim.uv.fs_mkdir(dir .. "/subdir", 493))

            local h = assert(vim.uv.fs_scandir(dir))
            local names = {{}}
            while true do
              local name = vim.uv.fs_scandir_next(h)
              if not name then break end
              names[#names + 1] = name
            end
            table.sort(names)

            return {{ data, vim.uv.fs_stat(fpath).size, vim.uv.fs_stat(dir .. "/subdir").type,
                     table.concat(names, ",") }}
            "#
        ),
    )
    .await;

    let a = result.as_array().expect("result table");
    assert_eq!(
        a[0].as_str(),
        Some("local bytes"),
        "read back what we wrote"
    );
    assert_eq!(a[1].as_u64(), Some(11), "stat size matches the bytes");
    assert_eq!(a[2].as_str(), Some("directory"), "mkdir made a real dir");
    assert_eq!(
        a[3].as_str(),
        Some("note.txt,subdir"),
        "scandir lists the tree"
    );

    std::fs::remove_dir_all(&dir).ok();
}
