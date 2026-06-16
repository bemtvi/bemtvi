//! Behavior tests for `nx.fs` — the promise-always filesystem API
//! (docs/plans/2026-06-16-nx-fs-api.md, Phase 1: one-shot ops). Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state (and the real filesystem).
//!
//! Each op resolves/rejects a promise; reactions run as microtasks
//! (`nx.schedule`), so they settle within the convergence the server runs after
//! each `nvim_exec_lua`. The pattern (mirroring promise.rs): run an `nx.async`
//! chain in one chunk that writes its outcome to a `_G` global, then read that
//! global back in a second chunk — by which point every microtask has flushed.

use std::fs;

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
        lua_string(&rpc, "return _G.out").await.as_deref(),
        Some("a.txt:file,sub:directory")
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
        lua_string(&rpc, "return _G.txt").await.as_deref(),
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
        lua_string(&rpc, "return _G.code").await.as_deref(),
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
    assert_eq!(lua_u64(&rpc, "return _G.len").await, Some(3));
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
    assert_eq!(
        lua_string(&rpc, "return _G.kind").await.as_deref(),
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
    assert_eq!(lua_bool(&rpc, "return _G.a").await, Some(true));
    assert_eq!(lua_bool(&rpc, "return _G.b").await, Some(false));
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
            "nx.async(function() nx.await(nx.fs.mkdir(\"{d}\", {{ recursive = true }})) end)()",
            d = q(&nested)
        ),
    )
    .await;
    assert!(nested.is_dir());
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
            "nx.async(function() nx.await(nx.fs.rename(\"{a}\", \"{b}\")) end)()",
            a = q(&from),
            b = q(&to)
        ),
    )
    .await;
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
            "nx.async(function() nx.await(nx.fs.remove(\"{d}\", {{ recursive = true }})) end)()",
            d = q(&tree)
        ),
    )
    .await;
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
            "nx.async(function() nx.await(nx.fs.copy(\"{a}\", \"{b}\")) end)()",
            a = q(&src),
            b = q(&dst)
        ),
    )
    .await;
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
        lua_string(&rpc, "return _G.code").await.as_deref(),
        Some("ENOENT")
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
