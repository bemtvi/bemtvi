//! Behavior tests for `nx.fs` — the promise-always filesystem API
//! (docs/plans/2026-06-16-nx-fs-api.md + the off-tick plan). Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state (and the real filesystem).
//!
//! `nx.fs` ops now settle OFF the editor tick: `nx._fs_op` queues the op for the
//! event-loop actor, which runs it on its blocking pool and reports the result back
//! on a LATER tick (the only way to reach the daemon on wasm, and non-blocking on
//! native). So unlike a microtask-only chain, the global an `nx.async` chain sets
//! only appears after the loop processes the result — exactly like the `nx.run`
//! process tests in `async_runtime.rs`. Each test queues its chain, then POLLS the
//! observable (a `_G` global, or the real filesystem) until the off-tick op settles.

use std::fs;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, lua_u64, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Read a `return`-style chunk as an owned `String` (`None` if not a string).
async fn lua_string(rpc: &Rpc, code: &str) -> Option<String> {
    exec_lua(rpc, code).await.as_str().map(str::to_owned)
}

/// Poll a `return`-style chunk until it yields a non-nil value (~3s), then return it.
/// An off-tick `nx.fs` op settles on a later tick, so the global its chain sets is
/// nil until the loop processes the actor's result; this gives that wall-clock room.
async fn poll_settled(rpc: &Rpc, code: &str) -> rmpv::Value {
    for _ in 0..150 {
        let v = exec_lua(rpc, code).await;
        if !matches!(v, rmpv::Value::Nil) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    exec_lua(rpc, code).await
}

/// Poll until the predicate `done` reports the chain finished (~3s). For the
/// disk-mutating ops whose only observable is the real filesystem, the chain sets
/// `_G.done = true` after its final `await`; this waits for that across-tick settle
/// before the test asserts on the disk.
async fn poll_done(rpc: &Rpc) {
    for _ in 0..150 {
        if lua_bool(rpc, "return _G.done").await == Some(true) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Lua-escape a path for embedding in a double-quoted string literal.
fn q(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

// ----- listing ----------------------------------------------------------------

#[tokio::test]
async fn readdir_returns_entries_with_kind() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_readdir");
    fs::write(dir.join("a.txt"), b"hi").unwrap();
    fs::create_dir(dir.join("sub")).unwrap();

    // Collect "name:type" pairs sorted, so the assertion is order-independent.
    exec_lua(
        &rpc,
        &format!(
            "_G.out = nil\n\
             nx.async(function()\n\
               local es = nx.await(nx.fs.readdir(\"{d}\"))\n\
               local parts = {{}}\n\
               for _, e in ipairs(es) do parts[#parts+1] = e.name .. \":\" .. e.type end\n\
               table.sort(parts)\n\
               _G.out = table.concat(parts, \",\")\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.out").await.as_str(),
        Some("a.txt:file,sub:directory")
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `nx.fs.walk` recurses through subdirectories, returns file paths relative to the
/// root, prunes `.git` + dotfiles by default, and skips directory entries. This is the
/// transport-agnostic file enumeration the `files` picker falls back to when `rg`
/// isn't available (the pure web client).
#[tokio::test]
async fn walk_recurses_and_prunes_dotfiles() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_walk");
    fs::write(dir.join("a.txt"), b"a").unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("b.txt"), b"b").unwrap();
    // A dotfile and a pruned .git/ tree must NOT appear.
    fs::write(dir.join(".hidden"), b"x").unwrap();
    fs::create_dir(dir.join(".git")).unwrap();
    fs::write(dir.join(".git").join("config"), b"x").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.out = nil\n\
             nx.async(function()\n\
               local files = nx.await(nx.fs.walk(\"{d}\"))\n\
               table.sort(files)\n\
               _G.out = table.concat(files, \",\")\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.out").await.as_str(),
        Some("a.txt,sub/b.txt")
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `nx.fs.grep` recursively substring-matches a query across a tree, returning
/// `{ path, row, col, text }` per hit (paths relative to the root, 1-based row/col),
/// skipping `.git`/dotfiles and non-matching files. This is the transport-agnostic
/// search the grep picker falls back to when `rg`/`grep` aren't available (the pure web
/// client).
#[tokio::test]
async fn grep_matches_substring_across_the_tree() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_grep");
    fs::write(dir.join("a.txt"), "alpha NEEDLE one\nbeta\n").unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("b.txt"), "no match here\nx NEEDLE y\n").unwrap();
    fs::write(dir.join("c.txt"), "nothing\n").unwrap();

    // Collect "path:row:col" for each match, sorted, so the assertion is order-free.
    exec_lua(
        &rpc,
        &format!(
            "_G.out = nil\n\
             nx.async(function()\n\
               local ms = nx.await(nx.fs.grep(\"{d}\", \"NEEDLE\"))\n\
               local parts = {{}}\n\
               for _, m in ipairs(ms) do\n\
                 parts[#parts+1] = m.path .. \":\" .. m.row .. \":\" .. m.col\n\
               end\n\
               table.sort(parts)\n\
               _G.out = table.concat(parts, \",\")\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;
    // a.txt line 1 col 7 ("alpha NEEDLE"), sub/b.txt line 2 col 3 ("x NEEDLE").
    assert_eq!(
        poll_settled(&rpc, "return _G.out").await.as_str(),
        Some("a.txt:1:7,sub/b.txt:2:3")
    );
    let _ = fs::remove_dir_all(&dir);
}

// ----- read / write round-trip ------------------------------------------------

#[tokio::test]
async fn write_then_read_round_trips() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_rw");
    let file = dir.join("note.txt");

    exec_lua(
        &rpc,
        &format!(
            "_G.txt = nil\n\
             nx.async(function()\n\
               nx.await(nx.fs.write(\"{f}\", \"hello \"))\n\
               nx.await(nx.fs.append(\"{f}\", \"world\"))\n\
               _G.txt = nx.await(nx.fs.read_text(\"{f}\"))\n\
             end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.txt").await.as_str(),
        Some("hello world")
    );
    // And it actually hit the disk.
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello world");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_text_rejects_invalid_utf8() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_badutf8");
    let file = dir.join("bin");
    fs::write(&file, [0xff, 0xfe, 0x00]).unwrap(); // not valid UTF-8

    // read_text must REJECT (EILSEQ), not silently return lossy text.
    exec_lua(
        &rpc,
        &format!(
            "_G.code = nil\n\
             nx.async(function()\n\
               local ok, err = pcall(nx.await, nx.fs.read_text(\"{f}\"))\n\
               _G.code = (not ok) and err.code or \"NO_ERROR\"\n\
             end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.code").await.as_str(),
        Some("EILSEQ")
    );
    // Raw read of the same bytes succeeds (3 bytes).
    exec_lua(
        &rpc,
        &format!(
            "_G.len = nil\n\
             nx.async(function() _G.len = #nx.await(nx.fs.read(\"{f}\")) end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(poll_settled(&rpc, "return _G.len").await.as_u64(), Some(3));
    let _ = fs::remove_dir_all(&dir);
}

// ----- stat / exists ----------------------------------------------------------

#[tokio::test]
async fn stat_reports_type_and_size() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_stat");
    fs::write(dir.join("f"), b"12345").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.kind, _G.size = nil, nil\n\
             nx.async(function()\n\
               local st = nx.await(nx.fs.stat(\"{f}\"))\n\
               _G.kind, _G.size = st.type, st.size\n\
             end)()",
            f = q(&dir.join("f"))
        ),
    )
    .await;
    // `_G.kind`/`_G.size` are set together at the end of the chain; poll the kind for
    // the off-tick settle, then the size is already present.
    assert_eq!(
        poll_settled(&rpc, "return _G.kind").await.as_str(),
        Some("file")
    );
    assert_eq!(lua_u64(&rpc, "return _G.size").await, Some(5));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn exists_resolves_bool_never_rejects() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_exists");
    fs::write(dir.join("here"), b"").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.a, _G.b = nil, nil\n\
             nx.async(function()\n\
               _G.a = nx.await(nx.fs.exists(\"{here}\"))\n\
               _G.b = nx.await(nx.fs.exists(\"{gone}\"))\n\
             end)()",
            here = q(&dir.join("here")),
            gone = q(&dir.join("nope"))
        ),
    )
    .await;
    // `_G.b` (the missing-path probe) is awaited last; once it has settled to its
    // boolean both globals are present. `exists` never rejects — a missing path
    // resolves `false`, not an error.
    assert_eq!(
        poll_settled(&rpc, "return _G.b").await.as_bool(),
        Some(false)
    );
    assert_eq!(lua_bool(&rpc, "return _G.a").await, Some(true));
    let _ = fs::remove_dir_all(&dir);
}

// ----- mkdir / rename / copy / remove -----------------------------------------

#[tokio::test]
async fn mkdir_recursive_creates_parents() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_mkdir");
    let nested = dir.join("a/b/c");

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             nx.async(function()\n\
               nx.await(nx.fs.mkdir(\"{d}\", {{ recursive = true }}))\n\
               _G.done = true\n\
             end)()",
            d = q(&nested)
        ),
    )
    .await;
    poll_done(&rpc).await;
    assert!(nested.is_dir());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn mkdir_honors_the_mode_option() {
    // `nx.fs.mkdir(path, { recursive = true, mode = 0o700 })` must create a private
    // directory, not one with umask-default (world-readable) perms — the security
    // property the removed blocking `vim.fn.mkdir(path, "p", "0700")` used to carry.
    use std::os::unix::fs::PermissionsExt;
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_mkdir_mode");
    let nested = dir.join("private/nested");

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             nx.async(function()\n\
               nx.await(nx.fs.mkdir(\"{d}\", {{ recursive = true, mode = 448 }}))\n\
               _G.done = true\n\
             end)()",
            d = q(&nested)
        ),
    )
    .await;
    poll_done(&rpc).await;
    let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "mkdir should apply the mode option (448 == 0o700)"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rename_moves_a_file() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_rename");
    let from = dir.join("old.txt");
    let to = dir.join("new.txt");
    fs::write(&from, b"x").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             nx.async(function()\n\
               nx.await(nx.fs.rename(\"{a}\", \"{b}\"))\n\
               _G.done = true\n\
             end)()",
            a = q(&from),
            b = q(&to)
        ),
    )
    .await;
    poll_done(&rpc).await;
    assert!(!from.exists() && to.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn remove_recursive_deletes_a_tree() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_remove");
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("inner")).unwrap();
    fs::write(tree.join("inner/leaf.txt"), b"x").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             nx.async(function()\n\
               nx.await(nx.fs.remove(\"{d}\", {{ recursive = true }}))\n\
               _G.done = true\n\
             end)()",
            d = q(&tree)
        ),
    )
    .await;
    poll_done(&rpc).await;
    assert!(!tree.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn copy_duplicates_a_file() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_copy");
    let src = dir.join("src.txt");
    let dst = dir.join("dst.txt");
    fs::write(&src, b"payload").unwrap();

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             nx.async(function()\n\
               nx.await(nx.fs.copy(\"{a}\", \"{b}\"))\n\
               _G.done = true\n\
             end)()",
            a = q(&src),
            b = q(&dst)
        ),
    )
    .await;
    poll_done(&rpc).await;
    assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    assert!(src.exists()); // copy, not move
    let _ = fs::remove_dir_all(&dir);
}

// ----- error convention -------------------------------------------------------

#[tokio::test]
async fn missing_path_rejects_with_enoent() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_enoent");

    // A read of a nonexistent file must REJECT with code ENOENT — never silently
    // resolve. (The whole no-silent-failure point.)
    exec_lua(
        &rpc,
        &format!(
            "_G.code = nil\n\
             nx.async(function()\n\
               local ok, err = pcall(nx.await, nx.fs.read(\"{f}\"))\n\
               _G.code = (not ok) and err.code or \"NO_ERROR\"\n\
             end)()",
            f = q(&dir.join("missing"))
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.code").await.as_str(),
        Some("ENOENT")
    );
    let _ = fs::remove_dir_all(&dir);
}

// ----- off-tick settle --------------------------------------------------------

#[tokio::test]
async fn op_settles_on_a_later_tick_than_a_same_tick_schedule() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_offtick");
    fs::write(dir.join("f"), b"x").unwrap();

    // Prove the op is OFF-TICK by observable ordering alone (no promise internals):
    // race it against a `vim.schedule`, which runs at the END of THIS tick's
    // convergence. An `nx.fs` reaction can only run once the event loop delivers the
    // actor's result on a LATER tick, so "sched" is recorded before "fs" —
    // deterministically (it is causal, not wall-clock; the input handler can't yield to
    // the loop's `FsResult` arm until it has returned, after `run_pending`). An INLINE
    // op would resolve in-chunk and queue its `:next` reaction before `vim.schedule`,
    // recording "fs" first — so this still discriminates inline from off-tick. The
    // mirror of `async_runtime.rs`'s `schedule_runs_after_direct_work_not_inline`.
    exec_lua(
        &rpc,
        &format!(
            "_G.order = {{}}\n\
             nx.fs.stat(\"{f}\"):next(function() _G.order[#_G.order + 1] = 'fs' end)\n\
             vim.schedule(function() _G.order[#_G.order + 1] = 'sched' end)",
            f = q(&dir.join("f"))
        ),
    )
    .await;
    // Wait for both reactions to have fired (the fs one needs the off-tick result).
    for _ in 0..150 {
        if lua_u64(&rpc, "return #_G.order").await == Some(2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        lua_string(&rpc, "return table.concat(_G.order, ',')")
            .await
            .as_deref(),
        Some("sched,fs"),
        "the same-tick vim.schedule must run before the off-tick nx.fs reaction"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ----- watch (Phase 2: change async-iterator) ---------------------------------

/// Poll a `return`-style numeric chunk until it reaches `want` (or ~3s elapse).
/// Watch events arrive asynchronously (notify backend thread → 10 ms coalesce →
/// actor → server loop), so the test gives the background loop wall-clock time.
async fn poll_u64_at_least(rpc: &Rpc, code: &str, want: u64) -> u64 {
    for _ in 0..150 {
        if let Some(n) = lua_u64(rpc, code).await {
            if n >= want {
                return n;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    lua_u64(rpc, code).await.unwrap_or(0)
}

#[tokio::test]
async fn watch_reports_a_change_with_paths() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_watch");

    // Arm a recursive watch on the dir, accumulating each coalesced batch.
    exec_lua(
        &rpc,
        &format!(
            "_G.evs = {{}}\n\
             _G.W = nil\n\
             nx.async(function()\n\
               local w = nx.fs.watch(\"{d}\", {{ recursive = true }})\n\
               _G.W = w\n\
               for ev in nx.await_each(w) do _G.evs[#_G.evs+1] = ev end\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;

    // Give the actor a beat to actually arm the native watcher, then make a change.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    fs::write(dir.join("new.txt"), b"hello").unwrap();

    let n = poll_u64_at_least(&rpc, "return #_G.evs", 1).await;
    assert!(n >= 1, "expected at least one watch event, saw {n}");

    // The batch names the changed file, and carries a known change kind.
    let paths = lua_string(
        &rpc,
        "local o = {} for _, ev in ipairs(_G.evs) do \
           for _, p in ipairs(ev.paths) do o[#o+1] = p end end \
         return table.concat(o, \"\\n\")",
    )
    .await
    .unwrap_or_default();
    assert!(
        paths.contains("new.txt"),
        "watch paths should name the changed file; got: {paths:?}"
    );
    let kind = lua_string(&rpc, "return _G.evs[1].kind")
        .await
        .unwrap_or_default();
    assert!(
        matches!(kind.as_str(), "create" | "modify" | "remove" | "rename"),
        "unexpected change kind: {kind:?}"
    );

    // Clean up the watch so the actor task / native watcher tears down.
    exec_lua(&rpc, "if _G.W then _G.W:stop() end").await;
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn watch_stop_ends_iteration() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_watch_stop");

    exec_lua(
        &rpc,
        &format!(
            "_G.done = false\n\
             _G.W = nil\n\
             nx.async(function()\n\
               local w = nx.fs.watch(\"{d}\")\n\
               _G.W = w\n\
               for _ in nx.await_each(w) do end\n\
               _G.done = true\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;
    // The for-loop is parked on the first pull; stopping the watch ends it cleanly.
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(false));
    exec_lua(&rpc, "_G.W:stop()").await;
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(true));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn watch_bad_path_rejects_loud() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_watch_bad");
    let missing = dir.join("does-not-exist");

    // Watching a path that can't be armed must REJECT the iteration (fail loud),
    // never sit on a dead watch.
    exec_lua(
        &rpc,
        &format!(
            "_G.werr = nil\n\
             nx.async(function()\n\
               local w = nx.fs.watch(\"{d}\")\n\
               local ok, err = pcall(function()\n\
                 for _ in nx.await_each(w) do end\n\
               end)\n\
               _G.werr = (not ok) and tostring(err) or \"NO_ERROR\"\n\
             end)()",
            d = q(&missing)
        ),
    )
    .await;
    // The arm failure comes back via the loop_events arm; poll for it.
    for _ in 0..150 {
        if lua_string(&rpc, "return _G.werr").await.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let werr = lua_string(&rpc, "return _G.werr").await.unwrap_or_default();
    assert!(
        werr != "NO_ERROR" && !werr.is_empty(),
        "watch on a missing path should reject, got: {werr:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ----- nx.hash.file (streaming digest) ----------------------------------------

#[tokio::test]
async fn hash_file_streams_a_large_file_and_matches_the_one_shot_digest() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_hashfile");
    let file = dir.join("big.bin");

    // Content larger than the 64 KiB streaming chunk, so the read loop folds several
    // chunks — the path that would break if hashing only saw the first chunk. We hash
    // the file (streamed in the server) and, independently, the SAME bytes in memory
    // via the one-shot nx.hash.sha256; the two must agree. This both proves the
    // streaming digest is correct AND that the two APIs (kept side by side, for
    // different jobs) render identical digests.
    exec_lua(
        &rpc,
        &format!(
            "_G.match = nil\n\
             nx.async(function()\n\
               local data = string.rep('nxvim-', 50000)\n\
               nx.await(nx.fs.write(\"{f}\", data))\n\
               local streamed = nx.await(nx.hash.file(\"{f}\", 'sha256'))\n\
               local one_shot = nx.hash.sha256(data)\n\
               _G.streamed = streamed\n\
               _G.match = (streamed == one_shot) and 'yes' or ('no:' .. streamed .. '/' .. one_shot)\n\
             end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.match").await.as_str(),
        Some("yes"),
        "the streamed file digest must equal the one-shot digest of the same bytes"
    );
    // Pin the actual value against an external oracle too (computed by `sha256sum`),
    // so a bug that corrupts BOTH paths identically can't pass.
    assert_eq!(
        lua_string(&rpc, "return _G.streamed").await.as_deref(),
        Some("0f486a3805eb4414fc58e2bbcd4c4fbc8b66ed33e21a7d172e2dfdc312888c61"),
        "the streamed digest matches an external sha256sum of the 300 KB file"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hash_file_defaults_to_sha256_and_rejects_unknown_algo() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_hashalgo");
    let file = dir.join("f.txt");

    exec_lua(
        &rpc,
        &format!(
            "_G.dflt = nil\n_G.code = nil\n\
             nx.async(function()\n\
               nx.await(nx.fs.write(\"{f}\", 'abc'))\n\
               -- no algo arg defaults to sha256\n\
               _G.dflt = (nx.await(nx.hash.file(\"{f}\")) == nx.hash.sha256('abc')) and 'yes' or 'no'\n\
               -- an unknown algorithm rejects (EINVAL), never a wrong digest\n\
               local ok, err = pcall(nx.await, nx.hash.file(\"{f}\", 'crc32'))\n\
               _G.code = (not ok) and err.code or 'NO_ERROR'\n\
             end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.dflt").await.as_str(),
        Some("yes"),
        "nx.hash.file defaults to sha256"
    );
    assert_eq!(
        poll_settled(&rpc, "return _G.code").await.as_str(),
        Some("EINVAL"),
        "an unknown algorithm rejects with EINVAL"
    );
    let _ = fs::remove_dir_all(&dir);
}
