//! Behavior tests for `btv.fs` — the promise-always filesystem API
//! (docs/plans/2026-06-16-btv-fs-api.md + the off-tick plan). Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state (and the real filesystem).
//!
//! `btv.fs` ops now settle OFF the editor tick: `btv._fs_op` queues the op for the
//! event-loop actor, which runs it on its blocking pool and reports the result back
//! on a LATER tick (the only way to reach the daemon on wasm, and non-blocking on
//! native). So unlike a microtask-only chain, the global an `btv.async` chain sets
//! only appears after the loop processes the result — exactly like the `btv.run`
//! process tests in `async_runtime.rs`. Each test queues its chain, then POLLS the
//! observable (a `_G` global, or the real filesystem) until the off-tick op settles.

use std::fs;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, lua_bool, lua_u64, q, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Read a `return`-style chunk as an owned `String` (`None` if not a string).
async fn lua_string(rpc: &Rpc, code: &str) -> Option<String> {
    exec_lua(rpc, code).await.as_str().map(str::to_owned)
}

/// Poll a `return`-style chunk until it yields a non-nil value (~3s), then return it.
/// An off-tick `btv.fs` op settles on a later tick, so the global its chain sets is
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
             btv.async(function()\n\
               local es = btv.await(btv.fs.readdir(\"{d}\"))\n\
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

/// `btv.fs.walk` recurses through subdirectories, returns file paths relative to the
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
             btv.async(function()\n\
               local files = btv.await(btv.fs.walk(\"{d}\"))\n\
               table.sort(files)\n\
               _G.out = table.concat(files, \",\")\n\
             end)()",
            d = q(&dir)
        ),
    )
    .await;
    // Relative paths are spelled with the platform's separator, as `rg` prints them.
    let sub_b = std::path::Path::new("sub").join("b.txt");
    assert_eq!(
        poll_settled(&rpc, "return _G.out").await.as_str(),
        Some(format!("a.txt,{}", sub_b.display()).as_str())
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `btv.fs.grep` recursively substring-matches a query across a tree, returning
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
             btv.async(function()\n\
               local ms = btv.await(btv.fs.grep(\"{d}\", \"NEEDLE\"))\n\
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
    // a.txt line 1 col 7 ("alpha NEEDLE"), sub/b.txt line 2 col 3 ("x NEEDLE"); the
    // relative path is spelled with the platform's separator, as `rg` prints it.
    let sub_b = std::path::Path::new("sub").join("b.txt");
    assert_eq!(
        poll_settled(&rpc, "return _G.out").await.as_str(),
        Some(format!("a.txt:1:7,{}:2:3", sub_b.display()).as_str())
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
             btv.async(function()\n\
               btv.await(btv.fs.write(\"{f}\", \"hello \"))\n\
               btv.await(btv.fs.append(\"{f}\", \"world\"))\n\
               _G.txt = btv.await(btv.fs.read_text(\"{f}\"))\n\
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
             btv.async(function()\n\
               local ok, err = pcall(btv.await, btv.fs.read_text(\"{f}\"))\n\
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
             btv.async(function() _G.len = #btv.await(btv.fs.read(\"{f}\")) end)()",
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
             btv.async(function()\n\
               local st = btv.await(btv.fs.stat(\"{f}\"))\n\
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
             btv.async(function()\n\
               _G.a = btv.await(btv.fs.exists(\"{here}\"))\n\
               _G.b = btv.await(btv.fs.exists(\"{gone}\"))\n\
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
             btv.async(function()\n\
               btv.await(btv.fs.mkdir(\"{d}\", {{ recursive = true }}))\n\
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
    // `btv.fs.mkdir(path, { recursive = true, mode = 0o700 })` must create a private
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
             btv.async(function()\n\
               btv.await(btv.fs.mkdir(\"{d}\", {{ recursive = true, mode = 448 }}))\n\
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
             btv.async(function()\n\
               btv.await(btv.fs.rename(\"{a}\", \"{b}\"))\n\
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
             btv.async(function()\n\
               btv.await(btv.fs.remove(\"{d}\", {{ recursive = true }}))\n\
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
             btv.async(function()\n\
               btv.await(btv.fs.copy(\"{a}\", \"{b}\"))\n\
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
             btv.async(function()\n\
               local ok, err = pcall(btv.await, btv.fs.read(\"{f}\"))\n\
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
    // convergence. An `btv.fs` reaction can only run once the event loop delivers the
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
             btv.fs.stat(\"{f}\"):next(function() _G.order[#_G.order + 1] = 'fs' end)\n\
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
        "the same-tick vim.schedule must run before the off-tick btv.fs reaction"
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
             btv.async(function()\n\
               local w = btv.fs.watch(\"{d}\", {{ recursive = true }})\n\
               _G.W = w\n\
               for ev in btv.await_each(w) do _G.evs[#_G.evs+1] = ev end\n\
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
             btv.async(function()\n\
               local w = btv.fs.watch(\"{d}\")\n\
               _G.W = w\n\
               for _ in btv.await_each(w) do end\n\
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
             btv.async(function()\n\
               local w = btv.fs.watch(\"{d}\")\n\
               local ok, err = pcall(function()\n\
                 for _ in btv.await_each(w) do end\n\
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

/// A consumer that stops pulling buffers a bounded number of change batches: past the
/// cap the watch delivers one `overflow` batch in place of the rest (loud, not a silent
/// drop), then resumes buffering once the consumer has pulled past it.
#[tokio::test]
async fn watch_buffer_is_capped_with_an_overflow_batch() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_watch_overflow");
    exec_lua(&rpc, &format!("_G.W = btv.fs.watch(\"{d}\")", d = q(&dir))).await;
    // Feed changes through the same entry the server delivers them on, with no pull
    // outstanding, then drain what the watch buffered. A buffered batch settles its
    // `:next()` promise synchronously, so the drain reads each settled value in place.
    let kinds = lua_string(
        &rpc,
        "for i = 1, 3000 do\n\
           btv._run_fs_watch(_G.W._id, { kind = 'modify', paths = { '/f' .. i } })\n\
         end\n\
         local n, overflows = 0, 0\n\
         for _ = 1, 3000 do\n\
           local ev = _G.W:next()._value\n\
           n = n + 1\n\
           if ev.kind == 'overflow' then overflows = overflows + 1 break end\n\
         end\n\
         btv._run_fs_watch(_G.W._id, { kind = 'create', paths = { '/after' } })\n\
         local after = _G.W:next()._value\n\
         return n .. ':' .. overflows .. ':' .. (after and after.kind or 'none')",
    )
    .await;
    assert_eq!(
        kinds.as_deref(),
        Some("1025:1:create"),
        "1024 buffered batches, then one overflow marker, then buffering resumes"
    );
    exec_lua(&rpc, "_G.W:stop()").await;
    let _ = fs::remove_dir_all(&dir);
}

// ----- btv.hash.file (streaming digest) ----------------------------------------

#[tokio::test]
async fn hash_file_streams_a_large_file_and_matches_the_one_shot_digest() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_hashfile");
    let file = dir.join("big.bin");

    // Content larger than the 64 KiB streaming chunk, so the read loop folds several
    // chunks — the path that would break if hashing only saw the first chunk. We hash
    // the file (streamed in the server) and, independently, the SAME bytes in memory
    // via the one-shot btv.hash.sha256; the two must agree. This both proves the
    // streaming digest is correct AND that the two APIs (kept side by side, for
    // different jobs) render identical digests.
    exec_lua(
        &rpc,
        &format!(
            "_G.match = nil\n\
             btv.async(function()\n\
               local data = string.rep('bemtvi-', 50000)\n\
               btv.await(btv.fs.write(\"{f}\", data))\n\
               local streamed = btv.await(btv.hash.file(\"{f}\", 'sha256'))\n\
               local one_shot = btv.hash.sha256(data)\n\
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
        Some("6729992fd4b5d58814905d650ac20c2c21b2a8d4417609f8b8ca0bf538b54201"),
        "the streamed digest matches an external sha256sum of the 350 KB file"
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
             btv.async(function()\n\
               btv.await(btv.fs.write(\"{f}\", 'abc'))\n\
               -- no algo arg defaults to sha256\n\
               _G.dflt = (btv.await(btv.hash.file(\"{f}\")) == btv.hash.sha256('abc')) and 'yes' or 'no'\n\
               -- an unknown algorithm rejects (EINVAL), never a wrong digest\n\
               local ok, err = pcall(btv.await, btv.hash.file(\"{f}\", 'crc32'))\n\
               _G.code = (not ok) and err.code or 'NO_ERROR'\n\
             end)()",
            f = q(&file)
        ),
    )
    .await;
    assert_eq!(
        poll_settled(&rpc, "return _G.dflt").await.as_str(),
        Some("yes"),
        "btv.hash.file defaults to sha256"
    );
    assert_eq!(
        poll_settled(&rpc, "return _G.code").await.as_str(),
        Some("EINVAL"),
        "an unknown algorithm rejects with EINVAL"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `btv.fs.gitignore` compiles one ignore file with ripgrep's semantics: the last
/// matching line decides, `!` whitelists, a trailing `/` matches only a directory, and
/// a line it cannot compile is skipped and reported rather than failing the file.
#[tokio::test]
async fn gitignore_matcher_follows_rgs_semantics() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        r#"local m, errs = btv.fs.gitignore("*.log\n!keep.log\nbuild/\n/top.txt\na{b\n")
           local function s(v) return tostring(v) end
           return table.concat({
             s(m:match("debug.log")), s(m:match("keep.log")), s(m:match("a/b/x.log")),
             s(m:match("build", true)), s(m:match("build", false)),
             s(m:match("top.txt")), s(m:match("sub/top.txt")), s(m:match("main.rs")),
             s(#errs),
           }, ",")"#,
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("ignore,whitelist,ignore,ignore,nil,ignore,nil,nil,1")
    );
}

/// A repository exercising every rule the `.gitignore`-aware walk composes; returns the
/// root. See `walk_with_gitignore_prunes_what_rg_would` for what each file proves.
fn ignore_fixture(tag: &str) -> std::path::PathBuf {
    let root = temp_dir(tag);
    let w = |rel: &str, body: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    w(".git/info/exclude", "secret.txt\n");
    w(".gitignore", "*.log\n!keep.log\n/build/\ngen/\n");
    w(".ignore", "notes.md\n!b.log\n");
    for f in [
        "a.log",
        "b.log",
        "keep.log",
        "build/out.txt",
        "src/build/x.txt",
        "src/gen/y.txt",
        "src/z.tmp",
        "src/c.log",
        "z.tmp",
        "secret.txt",
        "notes.md",
        "main.rs",
        ".hidden",
    ] {
        w(f, "x\n");
    }
    w("src/.gitignore", "*.tmp\n/anch.txt\n");
    w("src/anch.txt", "x\n");
    w("src/deep/anch.txt", "x\n");
    root
}

async fn walk_sorted(rpc: &Rpc, dir: impl AsRef<std::path::Path>) -> String {
    let dir = dir.as_ref();
    exec_lua(
        rpc,
        &format!(
            "_G.out = nil\n\
             btv.async(function()\n\
               local ok, files = pcall(btv.await, btv.fs.walk(\"{d}\", {{ hidden = true, gitignore = true }}))\n\
               if not ok then _G.out = 'ERR: ' .. tostring(type(files) == 'table' and files.message or files) return end\n\
               for i, f in ipairs(files) do files[i] = f:gsub('\\\\', '/') end\n\
               table.sort(files)\n\
               _G.out = table.concat(files, \",\")\n\
             end)()",
            d = q(dir)
        ),
    )
    .await;
    poll_settled(rpc, "return _G.out")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// `btv.fs.walk{ gitignore = true }` prunes exactly what `rg --files --hidden` does:
///
/// - `a.log` by `*.log`; `keep.log` back by `!keep.log`; `b.log` back by `.ignore`'s
///   `!b.log`, because `.ignore` outranks `.gitignore` whatever the depth;
/// - `build/` only at the root (`/build/` is anchored), so `src/build/x.txt` stays;
///   `gen/` at any depth;
/// - `src/.gitignore`'s `*.tmp` takes `src/z.tmp` but not the root's `z.tmp`, and its
///   anchored `/anch.txt` takes `src/anch.txt` but not `src/deep/anch.txt`;
/// - `secret.txt` by `.git/info/exclude`, `notes.md` by `.ignore`.
#[tokio::test]
async fn walk_with_gitignore_prunes_what_rg_would() {
    let (rpc, _incoming) = start().await;
    let root = ignore_fixture("fs_walk_gitignore");
    let want =
        ".gitignore,.hidden,.ignore,b.log,keep.log,main.rs,src/.gitignore,src/build/x.txt,src/deep/anch.txt,z.tmp";
    assert_eq!(walk_sorted(&rpc, &root).await, want);

    // The gold standard, where it is installed: rg's own listing of the same tree.
    if let Ok(out) = std::process::Command::new("rg")
        .args(["--files", "--hidden", "-g", "!.git"])
        .current_dir(&root)
        .output()
    {
        let mut rg: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.replace('\\', "/"))
            .collect();
        rg.sort();
        assert_eq!(rg.join(","), want, "rg lists the same set");
    }
}

/// Walking a subdirectory still honours the repository's ignore files ABOVE it, each
/// against the path from its own directory: the root's `*.log` takes `src/c.log`,
/// while its anchored `/build/` does not reach `src/build`.
#[tokio::test]
async fn walk_with_gitignore_honours_the_files_above_the_walked_dir() {
    let (rpc, _incoming) = start().await;
    let root = ignore_fixture("fs_walk_gitignore_sub");
    assert_eq!(
        walk_sorted(&rpc, &root.join("src")).await,
        ".gitignore,build/x.txt,deep/anch.txt"
    );
}

/// Outside a repository a `.gitignore` is not honoured — rg needs a repository for
/// one — while a `.ignore` still is.
#[tokio::test]
async fn walk_with_gitignore_outside_a_repo_honours_only_dot_ignore() {
    let (rpc, _incoming) = start().await;
    let root = temp_dir("fs_walk_gitignore_norepo");
    fs::write(root.join(".gitignore"), "a.log\n").unwrap();
    fs::write(root.join(".ignore"), "b.log\n").unwrap();
    fs::write(root.join("a.log"), "x").unwrap();
    fs::write(root.join("b.log"), "x").unwrap();
    assert_eq!(walk_sorted(&rpc, &root).await, ".gitignore,.ignore,a.log");
}

/// Write `body` at `root/rel`, creating the parents.
fn put(root: &std::path::Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A nested repository (a submodule, a vendored checkout) is its own world, as in rg
/// and git: the outer `.gitignore` and `.git/info/exclude` stop at its root, so
/// `vendor/lib/x.log` and `vendor/lib/y.tmp` are listed although the outer repo ignores
/// `*.log` and excludes `*.tmp`.
#[tokio::test]
async fn walk_with_gitignore_stops_outer_rules_at_a_nested_repo() {
    let (rpc, _incoming) = start().await;
    let root = temp_dir("fs_walk_gitignore_nested");
    put(&root, ".git/info/exclude", "*.tmp\n");
    put(&root, ".gitignore", "*.log\n");
    for f in [
        "a.log",
        "a.tmp",
        "main.rs",
        "vendor/lib/x.log",
        "vendor/lib/y.tmp",
    ] {
        put(&root, f, "x\n");
    }
    put(&root, "vendor/lib/.git/HEAD", "ref: refs/heads/main\n");
    let want = ".gitignore,main.rs,vendor/lib/x.log,vendor/lib/y.tmp";
    assert_eq!(walk_sorted(&rpc, &root).await, want);
    if let Ok(out) = std::process::Command::new("rg")
        .args(["--files", "--hidden", "-g", "!.git"])
        .current_dir(&root)
        .output()
    {
        let mut rg: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.replace('\\', "/"))
            .collect();
        rg.sort();
        assert_eq!(rg.join(","), want, "rg lists the same set");
    }
}

/// A project that is its own repository inside another one (a home directory kept as
/// a dotfiles repo, ignoring everything with `*`): neither the outer `.gitignore` nor
/// the outer `.git/info/exclude` reaches into the project, whether the walk starts at
/// the project root or below it.
#[tokio::test]
async fn walk_with_gitignore_ignores_an_enclosing_repos_rules() {
    let (rpc, _incoming) = start().await;
    let home = temp_dir("fs_walk_gitignore_dotfiles");
    put(&home, ".git/info/exclude", "*\n");
    put(&home, ".gitignore", "*\n");
    put(&home, "proj/.git/HEAD", "ref: refs/heads/main\n");
    put(&home, "proj/src/main.rs", "x\n");
    assert_eq!(walk_sorted(&rpc, home.join("proj")).await, "src/main.rs");
    assert_eq!(walk_sorted(&rpc, home.join("proj/src")).await, "main.rs");
}

/// A first line starting with a UTF-8 byte-order mark (as some Windows editors write
/// it) is read as git and rg read it: the BOM is not part of the pattern.
#[tokio::test]
async fn gitignore_matcher_skips_a_leading_bom() {
    let (rpc, _incoming) = start().await;
    let got = exec_lua(
        &rpc,
        r#"return tostring(btv.fs.gitignore("\239\187\191a.log\n"):match("a.log"))"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("ignore"));
}

/// A walk root spelled with a trailing separator resolves the directories above it
/// the same as without: the parent's anchored `/proj/gen/` still prunes `gen/`.
#[tokio::test]
async fn walk_with_gitignore_trailing_separator_on_the_root() {
    let (rpc, _incoming) = start().await;
    let w = temp_dir("fs_walk_gitignore_trailing");
    put(&w, ".ignore", "/proj/gen/\n");
    put(&w, "proj/gen/out.txt", "x\n");
    put(&w, "proj/main.rs", "x\n");
    let dir = format!("{}/", w.join("proj").display());
    assert_eq!(walk_sorted(&rpc, &dir).await, "main.rs");
}

/// In a linked worktree (`git worktree add`) `.git` is a FILE pointing into the main
/// repository, and the `info/exclude` that applies is the main repository's (the
/// worktree's `commondir`), as rg resolves it.
#[tokio::test]
async fn walk_with_gitignore_reads_a_worktrees_common_exclude() {
    let (rpc, _incoming) = start().await;
    let base = temp_dir("fs_walk_gitignore_worktree");
    put(&base, "main/.git/info/exclude", "secret.txt\n");
    put(&base, "main/.git/worktrees/wt/commondir", "../..\n");
    let gitdir = base.join("main/.git/worktrees/wt");
    put(&base, "wt/.git", &format!("gitdir: {}\n", gitdir.display()));
    put(&base, "wt/secret.txt", "x\n");
    put(&base, "wt/main.rs", "x\n");
    assert_eq!(walk_sorted(&rpc, base.join("wt")).await, ".git,main.rs");
}

/// An ignore file the walk cannot read is skipped with a warning naming it — as rg
/// warns and walks on — not silently, and without failing the whole walk over it.
#[cfg(unix)]
#[tokio::test]
async fn walk_with_gitignore_warns_and_goes_on_past_an_unreadable_ignore_file() {
    use std::os::unix::fs::PermissionsExt;
    let (rpc, _incoming) = start().await;
    let root = temp_dir("fs_walk_gitignore_unreadable");
    put(&root, ".ignore", "a.log\n");
    put(&root, "a.log", "x\n");
    let ig = root.join(".ignore");
    fs::set_permissions(&ig, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&ig).is_ok() {
        eprintln!("skip: running with permission to read a mode-000 file");
        return;
    }
    exec_lua(
        &rpc,
        "_G.notes = {}\nbtv.notify = function(m, l) table.insert(_G.notes, tostring(m) .. '@' .. tostring(l)) end",
    )
    .await;
    let got = walk_sorted(&rpc, &root).await;
    fs::set_permissions(&ig, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        got, ".ignore,a.log",
        "the walk goes on without the file's rules"
    );
    let notes = exec_lua(&rpc, "return table.concat(_G.notes, '|')").await;
    let notes = notes.as_str().unwrap_or_default().to_string();
    assert!(
        notes.contains(".ignore") && notes.contains("skipping"),
        "a warning names the skipped file: {notes:?}"
    );
}

/// An ignore-file line the `.gitignore` walk cannot compile is skipped (as rg and git
/// skip it) but not silently: the walk warns once, naming the file, as rg warns on
/// stderr — so a rule that never applies does not just look like a bug in the walk.
#[tokio::test]
async fn walk_with_gitignore_warns_once_about_a_line_it_cannot_compile() {
    let (rpc, _incoming) = start().await;
    let root = temp_dir("fs_walk_gitignore_badglob");
    fs::write(root.join(".ignore"), "a{b\nskip.txt\n").unwrap();
    fs::write(root.join("skip.txt"), "x").unwrap();
    fs::write(root.join("keep.txt"), "x").unwrap();
    exec_lua(
        &rpc,
        "_G.notes = {}\nbtv.notify = function(m, l) table.insert(_G.notes, tostring(m) .. '@' .. tostring(l)) end",
    )
    .await;
    assert_eq!(walk_sorted(&rpc, &root).await, ".ignore,keep.txt");
    assert_eq!(walk_sorted(&rpc, &root).await, ".ignore,keep.txt");
    let notes = exec_lua(&rpc, "return table.concat(_G.notes, '|')").await;
    let notes = notes.as_str().unwrap_or_default();
    assert!(
        notes.contains(".ignore") && notes.contains("a{b") && notes.matches('@').count() == 1,
        "one warning naming the file and its bad line: {notes:?}"
    );
}

/// An ignore file that is not a regular file — a FIFO (`.ignore` itself, or the
/// repository's `info/exclude` symlinked to one) — is skipped unread with a warning
/// naming it, instead of parking an fs worker forever on a read no writer will ever
/// answer (or slurping `/dev/zero` until the editor runs out of memory). A checkout
/// unpacked from an archive can carry either.
#[cfg(unix)]
#[tokio::test]
async fn walk_with_gitignore_skips_an_ignore_file_that_is_not_a_regular_file() {
    let (rpc, _incoming) = start().await;
    let mkfifo = |p: &std::path::Path| {
        assert!(std::process::Command::new("mkfifo")
            .arg(p)
            .status()
            .expect("mkfifo")
            .success());
    };
    // Unblock a reader parked on `fifo` (the pre-fix behavior), so the test can end.
    let release = |p: std::path::PathBuf| {
        std::thread::spawn(move || drop(fs::OpenOptions::new().write(true).open(p)));
    };
    exec_lua(
        &rpc,
        "_G.notes = {}\nbtv.notify = function(m, l) table.insert(_G.notes, tostring(m) .. '@' .. tostring(l)) end",
    )
    .await;
    let notes = || async {
        let n = exec_lua(&rpc, "return table.concat(_G.notes, '|')").await;
        n.as_str().unwrap_or_default().to_string()
    };

    let root = temp_dir("fs_walk_gitignore_fifo");
    fs::write(root.join("a.txt"), "x").unwrap();
    mkfifo(&root.join(".ignore"));
    let got = walk_sorted(&rpc, &root).await;
    if got.starts_with("ERR") {
        release(root.join(".ignore"));
    }
    assert_eq!(got, ".ignore,a.txt", "a FIFO .ignore is skipped unread");
    let n = notes().await;
    assert!(
        n.contains(".ignore") && n.contains("regular"),
        "a warning names the FIFO .ignore: {n:?}"
    );

    let repo = temp_dir("fs_walk_gitignore_fifo_exclude");
    fs::create_dir_all(repo.join(".git/info")).unwrap();
    fs::write(repo.join("a.txt"), "x").unwrap();
    let pipe = temp_dir("fs_walk_gitignore_fifo_target").join("pipe");
    mkfifo(&pipe);
    std::os::unix::fs::symlink(&pipe, repo.join(".git/info/exclude")).unwrap();
    let got = walk_sorted(&rpc, &repo).await;
    if got.starts_with("ERR") {
        release(pipe);
    }
    assert_eq!(
        got, "a.txt",
        "an info/exclude symlinked to a FIFO is skipped unread"
    );
    let n = notes().await;
    assert!(
        n.contains("exclude") && n.contains("regular"),
        "a warning names the FIFO info/exclude: {n:?}"
    );
}
