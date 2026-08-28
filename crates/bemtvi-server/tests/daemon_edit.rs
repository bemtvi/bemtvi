//! The daemon wire protocol, filesystem half — **`:edit` over the wire** (edit-host
//! split, Phase 3f of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (initial open) and `daemon_save.rs` (save). Here a real
//! editor whose async fs is a [`RemoteHostFs`](bemtvi_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](bemtvi_server::serve_fs_daemon) over an in-process duplex opens a
//! *second* file at runtime via `:edit` — fetched **over the wire, off the editor tick**
//! through the same `HostFsAsync` + replica path the initial open uses:
//!
//! - `:e /virtual/other.txt` fills a new buffer with bytes the edit-host's local disk
//!   can't hold (the `/virtual/...` path), so they can only have crossed the wire.
//! - `:e /virtual/fresh.txt` on a not-yet-existing path opens an empty new-file buffer.
//! - `:e!` reload-in-place **refetches** over the wire (a content change on the daemon
//!   shows up after the reload — proving a real re-read, not just a local-edit discard).
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on
//! buffer lines and the buffer name.

use std::path::PathBuf;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    await_lines, buf_lines, buf_name, exec_lua, feed, lua_bool, lua_u64, poll_true,
    spawn_with_daemon_fs, spawn_with_daemon_fs_init, DaemonFs,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// [`spawn_with_daemon_fs`] with the daemon's home seeded (`ServerInit::remote_home`) —
/// what the `config_bundle` handshake carries in a real session — so a leading `~` in a
/// file argument expands against the daemon's home rather than the edit-host's local
/// `$HOME`.
async fn spawn_with_daemon_fs_home(
    fake: DaemonFs,
    file: &str,
    remote_home: Option<&str>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    spawn_with_daemon_fs_init(
        fake,
        ServerInit {
            file: Some(file.to_string()),
            remote_home: remote_home.map(PathBuf::from),
            ..Default::default()
        },
    )
    .await
}

/// A leading `~` in a file argument expands against the **daemon's** home, not the
/// edit-host's local `$HOME` — the file read lands on the daemon even though the core
/// runs on the client. The session is seeded with the daemon's home (`/daemon/home`,
/// what the `config_bundle` handshake carries in a real remote session); `:e ~/other.txt`
/// must fetch `/daemon/home/other.txt` over the wire. Expanding against the local `$HOME`
/// would name a path the daemon fs doesn't have, so the buffer would come back empty —
/// exactly the mutation this guards against.
#[tokio::test]
async fn edit_expands_tilde_against_the_daemon_home() {
    let fake = DaemonFs::default();
    fake.set("/daemon/home/note.txt", "start\n")
        .set("/daemon/home/other.txt", "daemon\nhome\nfile\n");
    let (rpc, _incoming) =
        spawn_with_daemon_fs_home(fake, "/daemon/home/note.txt", Some("/daemon/home")).await;
    await_lines(&rpc, &["start"]).await;

    feed(&rpc, ":edit ~/other.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["daemon", "home", "file"]).await,
        vec!["daemon", "home", "file"],
        "`:e ~/…` must expand against the daemon's home and fetch that file over the wire"
    );
    assert_eq!(
        buf_name(&rpc).await,
        "/daemon/home/other.txt",
        "the buffer is named for the daemon-home-expanded path, not a local $HOME expansion"
    );
}

/// `:e /virtual/other.txt` fetches a *second* file's bytes over the wire into a new
/// buffer — content the edit-host's local disk can't hold, so it crossed the wire.
#[tokio::test]
async fn edit_fetches_a_second_file_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/other.txt", "second\nfile\nhere\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/other.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["second", "file", "here"]).await,
        vec!["second", "file", "here"],
        "`:edit` must fill the buffer with the second file's bytes from over the wire"
    );
    assert_eq!(
        buf_name(&rpc).await,
        "/virtual/other.txt",
        "the buffer is named for the edited remote path"
    );
}

/// `'indentdetect'` reads the indentation off the bytes the daemon ships, exactly like a
/// local open: the detection hangs off `load_bytes_into_enc` (core-side, where the wire's
/// bytes become the replica's text), not off the local read path — so a remote session is
/// not the one place a file's own indent convention is ignored.
#[tokio::test]
async fn indent_detect_reads_the_style_off_the_wire() {
    let fake = DaemonFs::default();
    fake.set_bytes("/virtual/spaced.py", b"def f():\n  if x:\n    g()\n  h()\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/spaced.py").await;

    await_lines(&rpc, &["def f():", "  if x:", "    g()", "  h()"]).await;
    assert_eq!(
        lua_bool(&rpc, "return btv.bo[0].expandtab").await,
        Some(true),
        "a space-indented remote file must open with 'expandtab', like a local one"
    );
    assert_eq!(
        lua_u64(&rpc, "return btv.bo[0].shiftwidth").await,
        Some(2),
        "…and with the 2-space step its own lines show"
    );
}

/// A non-UTF-8 file fetched **over the wire** decodes through the same encoding seam
/// as a local open: the daemon path used to `from_utf8_lossy` (silently mangling the
/// bytes), now it routes the raw bytes through `decode_to_rope` exactly like
/// `Buffer::from_file` — so latin1's 0xe9 shows as `é` and the fileencoding is `latin1`,
/// matching the local `encoding` suite. This is the local↔daemon-agree guarantee.
#[tokio::test]
async fn nonutf8_file_decodes_over_the_wire_like_local() {
    let fake = DaemonFs::default();
    fake.set_bytes("/virtual/latin1.txt", b"caf\xe9\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/latin1.txt").await;

    assert_eq!(
        await_lines(&rpc, &["café"]).await,
        vec!["café"],
        "0xe9 must decode to é over the wire (no from_utf8_lossy mangling)"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.fileencoding")
            .await
            .as_str()
            .unwrap_or_default(),
        "latin1",
        "the remote replica carries the detected fileencoding, like a local open"
    );
}

/// `:e ++enc=<encoding>` forces the decode of a **remote** file too. The decode runs
/// core-side (`load_bytes_into`) on the bytes the daemon ships, so the forced encoding
/// only has to reach that call — it rides the deferred open through the edit-host's
/// in-flight-fetch bookkeeping (`forced_fetch_enc`). A Shift_JIS file mis-detects as
/// latin1 under the default `'fileencodings'`; `:e ++enc=shift_jis` over the wire fixes
/// it to 日本語 with `fileencoding=shift_jis`, exactly as the local `encoding` suite proves.
#[tokio::test]
async fn edit_plusplus_enc_forces_the_decode_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set_bytes("/virtual/sjis.txt", b"\x93\xfa\x96\x7b\x8c\xea\n"); // 日本語\n in Shift_JIS
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/sjis.txt").await;

    // Wait for the initial (default-detected) open to land over the wire...
    for _ in 0..100 {
        let l = buf_lines(&rpc, 0).await;
        if !l.is_empty() && l != vec![""] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // ...which mis-decodes it as latin1 — NOT the Japanese text.
    assert_ne!(
        buf_lines(&rpc, 0).await,
        vec!["日本語"],
        "default 'fileencodings' detection should garble the shift_jis file"
    );

    // Force the decode over the wire (the current file, so an explicit path reloads it).
    feed(&rpc, ":edit ++enc=shift_jis /virtual/sjis.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["日本語"]).await,
        vec!["日本語"],
        "++enc must force the shift_jis decode of the remote file over the wire"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.fileencoding")
            .await
            .as_str()
            .unwrap_or_default(),
        "shift_jis",
        "the forced encoding is recorded on the remote replica, so `:w` round-trips it"
    );
}

/// `:e /virtual/fresh.txt` on a path the daemon doesn't have opens an empty new-file
/// buffer (not an error), with its name bound for a later `:w`.
#[tokio::test]
async fn edit_missing_path_opens_a_new_file_buffer() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/fresh.txt<CR>");
    // Wait for the name to bind (the off-tick open), then assert the buffer is empty.
    for _ in 0..100 {
        if buf_name(&rpc).await == "/virtual/fresh.txt" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        buf_name(&rpc).await,
        "/virtual/fresh.txt",
        "a missing remote file still binds its name (a new-file buffer)"
    );
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec![""],
        "a new-file buffer is empty, not an error or stale content"
    );
}

/// `:e!` reload-in-place **refetches** over the wire: a content change made on the
/// daemon after the file was opened shows up after the reload — proving a real
/// re-read, not merely a discard of local edits.
#[tokio::test]
async fn edit_reload_refetches_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "original\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["original"]).await;

    // Make a local edit (so the buffer is modified)...
    feed(&rpc, "ggIlocal <Esc>");
    assert_eq!(
        await_lines(&rpc, &["local original"]).await,
        vec!["local original"]
    );

    // ...and meanwhile the file changed on the daemon. A `:e!` naming the current file
    // must refetch *that* (the bare form reloads the same way — see
    // `reload_over_the_wire_fires_bufwinenter`).
    fake.set("/virtual/note.txt", "changed on the daemon\n");
    feed(&rpc, ":edit! /virtual/note.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["changed on the daemon"]).await,
        vec!["changed on the daemon"],
        "`:e!` must refetch the file over the wire (the daemon's new content), \
         not just discard the local edit"
    );
}

/// `:tabnew /virtual/other.txt` opens the remote file in a **new tab**, fetched over the
/// wire — `:tabnew` was the last user-command `from_file` site that bypassed the off-tick
/// path (Phase 3h unifies it onto the shared open kernel). The `/virtual/...` path can't
/// be read from the edit-host's local disk, so the new tab's content crossed the wire.
#[tokio::test]
async fn tabnew_fetches_a_file_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/other.txt", "tab\ncontent\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":tabnew /virtual/other.txt<CR>");
    // The new (now-current) tab's buffer fills with the remote file's bytes...
    assert_eq!(
        await_lines(&rpc, &["tab", "content"]).await,
        vec!["tab", "content"],
        "`:tabnew` fills the new tab's buffer with the remote file's bytes over the wire"
    );
    // ...there really are two tab pages now (not an in-place `:edit`)...
    let tab_count = exec_lua(&rpc, "return #vim.api.nvim_list_tabpages()")
        .await
        .as_u64()
        .unwrap_or(0);
    assert_eq!(tab_count, 2, "`:tabnew` opened a second tab page");
    // ...and the new tab's buffer is named for the remote path.
    assert_eq!(buf_name(&rpc).await, "/virtual/other.txt");
}

/// An **existing** remote file opened off-tick fires `BufReadPost` when its content lands —
/// the daemon stats the file at read and ships the stat, so the replica gets a `disk`
/// baseline (without it `buffer_is_new_file` is true and the content-load fires `BufNewFile`
/// too, so a config that seeds diagnostics / attaches an LSP on `BufReadPost` never runs).
/// The diagnostic the handler seeds therefore lands on the buffer. (The off-tick `:edit`
/// creates the empty buffer before the fetch returns, so a placeholder `BufNewFile` precedes
/// the content-load `BufReadPost` — a pre-existing artifact of the off-tick split, not this
/// fix's concern; the regression guard is that the load now fires `BufReadPost`.) A genuinely
/// new remote path still fires only `BufNewFile`, proving the distinction is real.
#[tokio::test]
async fn edit_existing_remote_file_fires_bufreadpost_not_bufnewfile() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/lint.txt", "needs linting\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    // Tap both events (pattern matches either remote file), then seed a diagnostic from the
    // BufReadPost handler — exactly the shape of a real `btv.diagnostic.set`-on-read config.
    exec_lua(
        &rpc,
        r#"
        _G.events = {}
        local ns = btv.ns.create("daemon-read-test")
        btv.autocmd.create({ "BufReadPost" }, { pattern = "*lint.txt", callback = function(a)
          _G.events[#_G.events + 1] = "BufReadPost"
          btv.diagnostic.set(ns, a.buf, { { lnum = 0, col = 0, severity = 1, message = "lint!" } })
        end })
        btv.autocmd.create({ "BufNewFile" }, { pattern = "*lint.txt", callback = function()
          _G.events[#_G.events + 1] = "BufNewFile"
        end })
        return 1
        "#,
    )
    .await;

    // Open the existing remote file (fetched + stat'd over the wire, off-tick).
    feed(&rpc, ":edit /virtual/lint.txt<CR>");
    await_lines(&rpc, &["needs linting"]).await;

    let events = exec_lua(&rpc, "return table.concat(_G.events, ',')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        events.split(',').next_back() == Some("BufReadPost"),
        "the content-load of an existing remote file must fire BufReadPost (got {events:?})"
    );

    // End-to-end: the BufReadPost handler's diagnostic landed on the opened buffer.
    assert_eq!(
        exec_lua(&rpc, "return #btv.diagnostic.get(0)")
            .await
            .as_u64(),
        Some(1),
        "the BufReadPost handler seeded its diagnostic on the remote file"
    );

    // The control: a genuinely-new remote path still fires BufNewFile.
    exec_lua(&rpc, "_G.events = {}").await;
    feed(&rpc, ":edit /virtual/fresh-lint.txt<CR>");
    // Wait for the *event*, not for the buffer name: an off-tick `:edit` names its
    // buffer the moment it is created and only learns the path doesn't exist when the
    // fetch lands, so the name is true long before `BufNewFile` fires. Polling the name
    // reads `_G.events` while it is still empty — the flake this used to show under load.
    poll_true(&rpc, "return #_G.events > 0").await;
    let events = exec_lua(&rpc, "return table.concat(_G.events, ',')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        events.split(',').next_back() == Some("BufNewFile") && !events.contains("BufReadPost"),
        "a missing remote path is still a new file (fires BufNewFile, never BufReadPost; got {events:?})"
    );
}

/// The gated read chain works identically over the wire — the tier-1 rule applied to the
/// async event model (`docs/plans/2026-07-26-async-event-model.md`, phases 4–5).
///
/// A remote open takes the off-tick path: the buffer is created empty, the bytes land a
/// tick or more later, and only *then* is the buffer announced. Both halves of the model
/// must survive that split — the chain's ordering (`FileType` waits for an async
/// `BufReadPost` handler to settle) and replay (a handler registered during the async
/// tail still receives the event). Ungated, `FileType` fires the moment `BufReadPost`
/// returns, i.e. between `read:start` and `read:done`, which is the mutation this pins.
#[tokio::test]
async fn the_gated_read_chain_orders_and_replays_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/chain.rs", "fn over_the_wire() {}\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    exec_lua(
        &rpc,
        r#"
        _G.log = {}
        -- An async BufReadPost handler that reads the loaded content: it can only see
        -- "over the wire" if the fetched bytes really landed before the announce.
        btv.autocmd.create("BufReadPost", { pattern = "*chain.rs", callback = function(a)
          _G.log[#_G.log + 1] = "read:start"
          return btv.promise.delay(30):next(function()
            _G.log[#_G.log + 1] = "read:done:" .. (btv.buf.lines(a.buf, 0, 1)[1] or "")
          end)
        end })
        -- Pattern-less, so EVERY FileType this buffer fires is visible — including one
        -- that jumped the gate. It then goes async itself and registers a LATE
        -- subscriber, which must still receive the same event.
        btv.autocmd.create("FileType", { callback = function(a)
          _G.log[#_G.log + 1] = "ft:" .. a.match
          return btv.promise.delay(5):next(function()
            btv.autocmd.create("FileType", { callback = function(x)
              _G.log[#_G.log + 1] = "late:" .. x.match
            end })
          end)
        end })
        return 1
        "#,
    )
    .await;

    feed(&rpc, ":edit /virtual/chain.rs<CR>");
    await_lines(&rpc, &["fn over_the_wire() {}"]).await;

    let mut log = String::new();
    for _ in 0..200 {
        log = exec_lua(&rpc, "return table.concat(_G.log, ',')")
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        if log.contains("late:") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        log, "read:start,read:done:fn over_the_wire() {},ft:rust,late:rust",
        "over the wire: the async read handler saw the fetched content, FileType waited \
         for it to settle rather than firing between start and done, and the late \
         subscriber still got the event"
    );
}

/// `BufWinEnter` fires on a reload **over the wire**, exactly as it does locally: a
/// re-read of a displayed buffer keeps its bufnr in the same window, so nothing about the
/// window changed — the fire hangs off the *read*, and the off-tick read lands in a
/// different place than the synchronous one. Tier-1: the remote session is not a degraded
/// mode, so a `:e!` that refetches over the wire owes the same per-window re-init a local
/// `:e!` does.
#[tokio::test]
async fn reload_over_the_wire_fires_bufwinenter() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "original\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["original"]).await;

    // Register *after* the startup fire, so the count is the reload's alone.
    exec_lua(
        &rpc,
        "_G.bwe = 0\n\
         btv.on('BufWinEnter', function() _G.bwe = _G.bwe + 1 end)\n\
         return 1",
    )
    .await;

    // Take the daemon's file watch out of the count *before* touching the file. The
    // fixture write below is a real external change, so the watch pushes an `fs_changed`
    // and 'autoread' turns it into a second, off-tick re-read of the same file — which
    // owes its own `BufWinEnter` just as legitimately as the `:e!` does. Whether that
    // watch reload lands before or after the assertion is pure timing, so the count was
    // 1 or 2 depending on load. Left on, this test measures `:e!` *plus* an autoreload;
    // off, it measures the `:e!` it is about. Round-tripped (`vim.o.autoread` read back)
    // so the option is set server-side before the write races it — the convention every
    // disk-change test here follows.
    feed(&rpc, ":set noautoread<CR>");
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(false),
        "'noautoread' must land before the fixture write, or the watch reloads too"
    );

    fake.set("/virtual/note.txt", "changed on the daemon\n");
    feed(&rpc, ":e!<CR>");
    await_lines(&rpc, &["changed on the daemon"]).await;

    assert_eq!(
        exec_lua(&rpc, "return _G.bwe").await.as_u64(),
        Some(1),
        "the off-tick re-read must fire BufWinEnter once, like the local `:e!`"
    );

    // The other half of "once": opening a *different* remote file moves the window off
    // this buffer, which the window diff sees on its own. The read landing must not fire
    // a second time on top of it.
    fake.set("/virtual/other.txt", "other\n");
    feed(&rpc, ":e /virtual/other.txt<CR>");
    await_lines(&rpc, &["other"]).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.bwe").await.as_u64(),
        Some(2),
        "a remote open fires BufWinEnter exactly once — the window's own change and the \
         read landing are the same display, not two"
    );
}
