use crate::support::*;

#[tokio::test]
async fn inserting_text_appears_in_the_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn invalid_utf8_startup_file_opens_named_and_resilient() {
    // A file whose bytes aren't valid UTF-8 used to refuse to open (and fall back to
    // an empty named buffer with an echoed error). Since the encoding seam landed
    // (docs/plans/2026-06-14-encoding-and-invalid-utf8.md, Phase 2) it *opens*: the
    // bytes decode through the latin1 fallback, so the buffer is named for the file,
    // non-empty, and round-trips on `:w`. The full round-trip is covered in the
    // `encoding` suite; here we just guard that startup no longer rejects such a file.
    let path = temp_path("openfail").to_string_lossy().into_owned();
    std::fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write invalid-utf8 file");
    let (rpc, _incoming) = start(Some(path.clone())).await;

    // The buffer is named after the file the user asked for, not `[No Name]`.
    let name = rpc
        .request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(name, path, "the opened file must keep its name");

    // And it actually opened with content (0xff/0xfe/0xfd → ÿ/þ/ý via latin1), rather
    // than the old empty-buffer-with-error fallback.
    assert_eq!(lines(&rpc).await, vec!["ÿþý"]);
}

#[tokio::test]
async fn opening_lines_and_navigating() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifirst<Esc>osecond<Esc>othird<Esc>");
    assert_eq!(lines(&rpc).await, vec!["first", "second", "third"]);
}

#[tokio::test]
async fn dd_deletes_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Back to the middle line and delete it.
    feed(&rpc, "kdd");
    assert_eq!(lines(&rpc).await, vec!["one", "three"]);
}

#[tokio::test]
async fn cw_changes_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    // Start of line, change first word.
    feed(&rpc, "0cwqux<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qux bar baz"]);
}

#[tokio::test]
async fn undo_reverts_the_last_change() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "ddu");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
}

#[tokio::test]
async fn undo_places_cursor_at_the_change_not_top_of_file() {
    // Undoing the first edit on a loaded buffer walks back to the root undo node.
    // The cursor must land at the change being undone, not snap to the root
    // snapshot's default (0, 0) top-of-file position.
    let path = write_n_lines("undo_cursor", 3);
    let (rpc, _incoming) = start(Some(path)).await;
    feed(&rpc, "Gx"); // jump to the last line, delete its first char
    assert_eq!(lines(&rpc).await, vec!["line1", "line2", "ine3"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "text restored"
    );
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "undo put the cursor at the changed line (1-based line 3), not the top of file"
    );
}

#[tokio::test]
async fn undo_ex_command_undoes_and_redoes_one_change() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    feed(&rpc, ":undo<CR>");
    assert_eq!(lines(&rpc).await, vec![""], ":undo undoes one change");
    feed(&rpc, ":redo<CR>");
    assert_eq!(lines(&rpc).await, vec!["abc"], ":redo restores it");
}

#[tokio::test]
async fn redo_follows_the_change_after_an_undo() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec![""]);
    feed(&rpc, "<C-r>");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo"],
        "<C-r> redoes the undone insert"
    );
}

// The defining property of a *branching* undo history: undoing and then making a
// new edit forks a branch rather than discarding the old future. The abandoned
// edit stays reachable by its sequence number via `:undo {N}` — something a
// linear two-stack undo can never do (the new edit would clear its redo stack).
#[tokio::test]
async fn undo_to_seq_reaches_an_abandoned_branch() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>"); // seq 1: "foo"
    feed(&rpc, "u"); // back to seq 0: ""
    feed(&rpc, "ibar<Esc>"); // seq 2: "bar" — forks a branch off seq 0
    assert_eq!(lines(&rpc).await, vec!["bar"]);

    feed(&rpc, ":undo 1<CR>"); // jump to the abandoned "foo" branch
    assert_eq!(
        lines(&rpc).await,
        vec!["foo"],
        ":undo 1 reaches the branch a linear undo would have dropped"
    );
    feed(&rpc, ":undo 2<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["bar"],
        ":undo 2 returns to the newer branch"
    );
    feed(&rpc, ":undo 0<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        ":undo 0 returns to the original text"
    );
}

#[tokio::test]
async fn undo_to_unknown_seq_is_reported_not_silent() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = latest_after(&rpc, &mut incoming, ":undo 99<CR>").await;
    let msg = view_str(&map, "message");
    assert!(
        msg.contains("99") && msg.contains("not found"),
        "out-of-range :undo reports E830, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await, vec!["foo"], "buffer is unchanged");
}

// `vim.fn.undotree()` projects the branching history into neovim's dict shape —
// the data the undotree visualizer plugin draws. Every state (including an
// abandoned branch) appears by seq, reachable via the spine + `alt` recursion.
#[tokio::test]
async fn undotree_fn_exposes_the_branching_tree() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>"); // seq 1
    feed(&rpc, "u"); // back to root
    feed(&rpc, "ibar<Esc>"); // seq 2, forks off root
    let code = r#"
        local t = vim.fn.undotree(0)
        local seqs = {}
        local function walk(es)
          for _, e in ipairs(es) do
            seqs[#seqs + 1] = e.seq
            if e.alt then walk(e.alt) end
          end
        end
        walk(t.entries)
        table.sort(seqs)
        return { t.seq_last, t.seq_cur, seqs }
    "#;
    let v = rpc
        .request("nvim_exec_lua", vec![Value::from(code), Value::Nil])
        .await
        .expect("undotree");
    let arr = v.as_array().expect("array result");
    assert_eq!(arr[0].as_u64(), Some(2), "seq_last is the highest state");
    assert_eq!(
        arr[1].as_u64(),
        Some(2),
        "seq_cur is the current state (bar)"
    );
    let seqs: Vec<u64> = arr[2]
        .as_array()
        .expect("seqs array")
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert_eq!(seqs, vec![1, 2], "both branches present in the tree by seq");
}

// A written state carries a `save` number, surfaced as `save`/`save_last`/
// `save_cur` — what the visualizer marks with an `S`.
#[tokio::test]
async fn undotree_fn_marks_the_saved_state() {
    let (rpc, _incoming) = start(None).await;
    let path = temp_path("undotree");
    feed(&rpc, "ifoo<Esc>");
    rpc.request(
        "btv_command",
        vec![Value::from(format!("w {}", path.display()).as_str())],
    )
    .await
    .expect("write");
    let v = rpc
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("local t = vim.fn.undotree(0); return { t.save_last, t.save_cur }"),
                Value::Nil,
            ],
        )
        .await
        .expect("undotree");
    let arr = v.as_array().expect("array result");
    assert_eq!(arr[0].as_u64(), Some(1), "save_last counts the write");
    assert_eq!(arr[1].as_u64(), Some(1), "the current state is that save");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn ex_write_persists_changes_to_disk() {
    let path = temp_path("write");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Jump to the last line, open a new one, type, leave insert, then save.
    feed(&rpc, "Gothree<Esc>");
    rpc.request("btv_command", vec![Value::from("w")])
        .await
        .expect("write");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "one\ntwo\nthree\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn ex_write_refuses_to_clobber_a_file_changed_on_disk() {
    let path = temp_path("clobber");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // An in-buffer edit, so there's something we'd be saving. Round-trip so the edit
    // is applied server-side before the external change — a *modified* buffer is not
    // autoreloaded by the file watch (only the clobber guard speaks here), but the
    // edit must land first or the watch reloads a still-unmodified buffer (a test-only
    // ordering race; a real edit precedes the external write).
    feed(&rpc, "Gomine<Esc>");
    assert!(lines(&rpc).await.contains(&"mine".to_string()));

    // Someone else rewrites the file on disk behind our back.
    std::fs::write(&path, "EXTERNALLY CHANGED\n").unwrap();

    // `:w` must refuse rather than silently clobber their changes. Target the
    // clobber frame specifically: the live buffer watch also fires a W12 conflict
    // for this same modified-and-changed file, and which message is *latest* on the
    // line is timing-dependent — so match the `:w`'s own frame rather than the tail.
    let msg = message(
        &redraw_after_matching(&rpc, &mut incoming, ":w<CR>", |m| {
            message(m).contains("changed on disk")
        })
        .await,
    );
    assert!(
        msg.contains("changed on disk") && msg.contains("add ! to override"),
        "expected a clobber warning, got: {msg:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "EXTERNALLY CHANGED\n",
        "the on-disk file must be untouched by the refused write"
    );

    // ...but `:w!` forces it through, and afterwards the buffer is in sync with
    // disk again (a second `:w` no longer trips the guard).
    rpc.request("btv_command", vec![Value::from("w!")])
        .await
        .expect("forced write");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\nmine\n",
        "`:w!` must overwrite despite the external change"
    );

    feed(&rpc, "obonus<Esc>");
    rpc.request("btv_command", vec![Value::from("w")])
        .await
        .expect("plain write after sync");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\nmine\nbonus\n",
        "after `:w!` re-synced disk state, a plain `:w` saves cleanly"
    );
    std::fs::remove_file(&path).ok();
}

// --- `:checktime` / `'autoread'` / `FileChangedShell` (the watch leg) ----------
//
// `:checktime` re-stats every loaded file-backed buffer and reconciles it with
// what bemtvi last read or wrote. The four outcomes mirror neovim exactly:
//   - file changed, buffer unmodified, `'autoread'` on  → silent reload
//   - file changed, buffer unmodified, `'autoread'` off → W11 warning, no reload
//   - file changed *and* buffer modified                → W12 conflict, no reload
//   - file vanished                                      → E211, no reload
// This is the local behavior the remote `HostWatch` push (a later slice) triggers
// over the wire; `:checktime` is both the user command and the watcher's entry.

#[tokio::test]
async fn checktime_reloads_an_unmodified_buffer_when_the_file_changed() {
    let path = temp_path("checktime-reload");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    // Someone else rewrites the file; our buffer is untouched. `'autoread'` is on
    // by default (neovim), so `:checktime` silently picks up the new content.
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    feed(&rpc, ":checktime<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "four"],
        "an unmodified buffer must autoreload the external change"
    );
    std::fs::remove_file(&path).ok();
}

/// An autoread reload keeps the buffer's own settable options. `reload_buffer` swaps the
/// whole `Buffer` for a fresh read, and the fresh read starts from the built-in defaults
/// — so without carrying the old options across, every `:setlocal` on that bufnr (and the
/// global tier it was born from) silently reset the moment something touched the file on
/// disk. The other reload paths (`:e!`, a deferred open's fill) already carry them; this
/// is the same contract for the watch/`:checktime` leg.
#[tokio::test]
async fn an_autoread_reload_keeps_the_buffers_own_options() {
    let path = temp_path("checktime-opts");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    feed(&rpc, ":setlocal tabstop=3 autoindent<CR>");
    // Round-trip so the option write lands server-side before the external write below
    // — the file watch fires asynchronously and would otherwise race it.
    assert_eq!(
        lua_u64(&rpc, "return btv.bo[0].tabstop").await,
        Some(3),
        "precondition: the buffer-local option is set"
    );

    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    feed(&rpc, ":checktime<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three"],
        "precondition: the reload happened"
    );

    assert_eq!(
        lua_u64(&rpc, "return btv.bo[0].tabstop").await,
        Some(3),
        "a reload must not reset the buffer's `:setlocal` options"
    );
    assert_eq!(
        lua_bool(&rpc, "return btv.bo[0].autoindent").await,
        Some(true),
        "…nor any other settable one"
    );
    std::fs::remove_file(&path).ok();
}

/// The other half of the same contract: what the *read* decides is still decided by the
/// read. A file rewritten with CRLF endings must come back as `fileformat=dos` — the
/// options carried across a reload are the settable ones only, never the read-derived
/// `'fileformat'` / `'fileencoding'` / `'endofline'` facts about the bytes.
#[tokio::test]
async fn an_autoread_reload_still_redetects_the_fileformat() {
    let path = temp_path("checktime-ff");
    std::fs::write(&path, b"one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].fileformat").await.as_str(),
        Some("unix"),
        "precondition: opened as a unix file"
    );

    std::fs::write(&path, b"one\r\ntwo\r\nthree\r\n").unwrap();
    feed(&rpc, ":checktime<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three"],
        "precondition: the reload happened, with no stray \\r in the lines"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].fileformat").await.as_str(),
        Some("dos"),
        "the reload re-detects the line-ending style from the new bytes"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn checktime_warns_on_conflict_when_both_disk_and_buffer_changed() {
    let path = temp_path("checktime-conflict");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    // Our own unsaved edit — round-trip so it's applied server-side *before* the
    // external change, else the buffer watch could autoreload a still-unmodified
    // buffer (a test-only ordering race; real edits land before an external write).
    feed(&rpc, "Gomine<Esc>");
    assert!(lines(&rpc).await.contains(&"mine".to_string()));
    // ...colliding with an external rewrite of the same file.
    std::fs::write(&path, "EXTERNALLY REWRITTEN CONTENT\n").unwrap();

    let msg = message(&redraw_after(&rpc, &mut incoming, ":checktime<CR>").await);
    assert!(
        msg.contains("W12") && msg.contains("changed"),
        "expected a W12 conflict warning, got: {msg:?}"
    );
    // The conflict must NOT clobber our in-buffer edit.
    assert!(
        lines(&rpc).await.contains(&"mine".to_string()),
        "a conflict must leave the modified buffer untouched"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn checktime_warns_without_reloading_when_autoread_is_off() {
    let path = temp_path("checktime-noar");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    // Round-trip so `'noautoread'` is in effect server-side before the external
    // change — otherwise the startup-armed watch could autoreload under the default
    // `autoread` before the option lands (a test-only ordering race).
    feed(&rpc, ":set noautoread<CR>");
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(false)
    );
    std::fs::write(&path, "REPLACED EXTERNALLY ENTIRELY\n").unwrap();

    let msg = message(&redraw_after(&rpc, &mut incoming, ":checktime<CR>").await);
    assert!(
        msg.contains("W11"),
        "with 'autoread' off, a changed file must warn (W11), got: {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two"],
        "with 'autoread' off, `:checktime` must not reload"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn checktime_reports_a_deleted_file() {
    let path = temp_path("checktime-gone");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    std::fs::remove_file(&path).unwrap();

    let msg = message(&redraw_after(&rpc, &mut incoming, ":checktime<CR>").await);
    assert!(
        msg.contains("E211"),
        "a vanished file must report E211, got: {msg:?}"
    );
}

#[tokio::test]
async fn an_external_change_autoreloads_via_the_buffer_watch() {
    // The auto-trigger: with no explicit `:checktime`, the server's per-buffer
    // native file watch (the evloop fs-watch machinery) must notice an external
    // change and run checktime on its own — reloading under `'autoread'`.
    let path = temp_path("watch-autoreload");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    // Give the watcher actor a beat to arm the watch (armed at startup, async), so
    // the change below isn't written before the watch exists.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // External in-place change — note: NO `:checktime`.
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    // Poll until the off-tick watch event lands and autoreloads (notify has
    // platform latency, so this is a bounded wait, not a fixed sleep).
    let mut got = vec![];
    for _ in 0..100 {
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        got = lines(&rpc).await;
        if got == vec!["one", "two", "three"] {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        got,
        vec!["one", "two", "three"],
        "the buffer watch must autoreload an external change with no :checktime"
    );
    std::fs::remove_file(&path).ok();
}

// --- the internal file watch's arm-failure retry ------------------------------
//
// A watch can fail to *arm* (an absent path, a permission the kernel refuses, an
// exhausted inotify budget). That must be retried — the common cause is transient, a
// writer replacing the file so the path is briefly gone — but on a bounded, backed-off
// schedule, never by re-arming from the settle path: `sync_buffer_watches` runs at the
// end of every `run_pending`, so a settle-driven retry arms, fails, settles, and arms
// again as fast as the loop turns, pegging a core and pushing a frame every round.
//
// All three provoke a real arm failure the same way: `inotify_add_watch` needs read
// permission, so a mode-000 file can still be `stat`ed — which is all the watch KEY is —
// but can never be watched. `:w` carries an existing file's mode onto its atomic
// replacement, so the save leaves the file unreadable *and* re-stamps the buffer's disk
// snapshot, which is what changes the key and forces the arm.

/// Open `path`, make it unwatchable-but-stat-able, and `:w` so the watch key changes and
/// the server arms onto a file the kernel refuses. Returns `None` when the process can
/// read a mode-000 file anyway (running as root), which no assertion here can survive.
#[cfg(unix)]
async fn arm_onto_an_unwatchable_file(rpc: &Rpc, path: &std::path::Path) -> Option<()> {
    use std::os::unix::fs::PermissionsExt;

    // Let the startup watch arm before anything changes under it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(path).is_ok() {
        eprintln!("skip: running as root — a mode-000 file is still readable and watchable");
        return None;
    }
    feed(rpc, "ohere<Esc>:w<CR>");
    assert_eq!(
        lines(rpc).await,
        vec!["one", "here", "two"],
        "precondition: the save went through (an atomic replace needs only the directory)"
    );
    Some(())
}

/// How many `redraw` notifications the server pushed over `window` of complete idle.
#[cfg(unix)]
async fn redraws_while_idle(
    incoming: &mut UnboundedReceiver<Incoming>,
    window: std::time::Duration,
) -> usize {
    while incoming.try_recv().is_ok() {} // everything queued so far is legitimate traffic
    tokio::time::sleep(window).await;
    let mut redraws = 0;
    while let Ok(inc) = incoming.try_recv() {
        if matches!(&inc, Incoming::Notification { method, .. } if method == "redraw") {
            redraws += 1;
        }
    }
    redraws
}

/// A watch that cannot arm must not respawn itself in a loop.
///
/// The arm-failure path used to drop the buffer's `buf_watches` entry, on the theory that
/// "a later tick re-arms it" — but the very settle that handles the failure ends in
/// `emit_lifecycle_events` → `sync_buffer_watches`, which found no entry, armed, failed,
/// and settled again. Reported in the wild as: leave the editor idle, let something
/// rewrite the file in the background, come back to a pegged CPU and an editor that lags
/// on every key. The retries are paced on the timer seam now, so an idle server stays
/// idle between them.
#[cfg(unix)]
#[tokio::test]
async fn a_watch_that_cannot_arm_must_not_respawn_in_a_loop() {
    use std::os::unix::fs::PermissionsExt;
    let _g = serial_lock().lock().await;

    let path = temp_path("watch-arm-fail");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    if arm_onto_an_unwatchable_file(&rpc, &path).await.is_some() {
        // The default backoff spends only its first two waits (100ms, 300ms) inside this
        // window, so a handful of frames is the honest ceiling — against thousands when
        // the re-arm rode the settle path.
        let redraws =
            redraws_while_idle(&mut incoming, std::time::Duration::from_millis(300)).await;
        assert!(
            redraws < 50,
            "a watch that cannot arm must not re-arm-spin: {redraws} redraws in 300ms of idle"
        );
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    std::fs::remove_file(&path).ok();
}

/// …and the retry is not decoration: once the obstacle clears, the watch comes back by
/// itself and the buffer autoreloads again. Nothing else can re-arm it here — the key is
/// unchanged, so `sync_buffer_watches` leaves it alone — so an autoreload is proof the
/// backed-off retry actually re-armed.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_watch_arm_is_retried_and_recovers() {
    use std::os::unix::fs::PermissionsExt;
    let _g = serial_lock().lock().await;

    let path = temp_path("watch-arm-retry");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    if arm_onto_an_unwatchable_file(&rpc, &path).await.is_some() {
        // The obstacle clears while the first retry is still counting down.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // An external change with no `:checktime` — only a live watch can notice it.
        std::fs::write(&path, "one\nhere\ntwo\nthree\n").unwrap();

        // Bounded wait: the retries land at ~100ms, 300ms, 900ms, 2.7s.
        let mut got = vec![];
        for _ in 0..80 {
            rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
            got = lines(&rpc).await;
            if got == vec!["one", "here", "two", "three"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            got,
            vec!["one", "here", "two", "three"],
            "the retry must re-arm the watch once the file is watchable again"
        );
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    std::fs::remove_file(&path).ok();
}

/// A change that lands while a watch is still being armed is not lost.
///
/// The server dispatches an arm and the loop actor gets to it some time later, so the
/// re-check that runs alongside the arm (`checktime` in `sync_buffer_watches`) can run
/// before the watch exists. A write in that gap fires no event and slips past the
/// re-check, and the buffer kept the old text for good. Under a loaded test run that
/// is what made `a_failed_watch_arm_is_retried_and_recovers` flaky: its own write fell
/// into the gap of the retry's arm. `$BEMTVI_WATCH_ARM_DELAY` holds the gap open.
#[tokio::test]
async fn a_change_while_the_watch_is_arming_still_reloads() {
    let _g = serial_lock().lock().await;

    let path = temp_path("watch-arm-gap");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let key = path.to_string_lossy().into_owned();
    // SAFETY: serial_lock keeps the other env-touching watch tests out, and the hook
    // only ever matches this test's own path.
    unsafe { std::env::set_var("BEMTVI_WATCH_ARM_DELAY", format!("600:{key}")) };
    let (rpc, _incoming) = start(Some(key)).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    // The startup arm (and its re-check) has gone out; the actor is still holding it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let mut got = vec![];
    for _ in 0..60 {
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        got = lines(&rpc).await;
        if got == vec!["one", "two", "three"] {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    unsafe { std::env::remove_var("BEMTVI_WATCH_ARM_DELAY") };
    assert_eq!(
        got,
        vec!["one", "two", "three"],
        "a write that landed before the watch was up must still autoreload"
    );
    std::fs::remove_file(&path).ok();
}

/// A permanently unwatchable file exhausts the retry budget rather than trying forever,
/// and says so — with the watch down, nothing autoreloads that file and nothing warns
/// when it changes underneath the buffer, so leaving it looking watched would be the
/// silent-degradation the "fail loud" rule exists to prevent.
///
/// `$BEMTVI_WATCH_RETRY_BASE_MS` compresses the whole schedule (the default spends ~12s
/// getting here); `serial_lock` keeps that process-wide override off the two tests above.
#[cfg(unix)]
#[tokio::test]
async fn a_watch_arm_retry_gives_up_after_its_budget() {
    use std::os::unix::fs::PermissionsExt;
    let _g = serial_lock().lock().await;
    unsafe { std::env::set_var("BEMTVI_WATCH_RETRY_BASE_MS", "1") };

    let path = temp_path("watch-arm-giveup");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    if arm_onto_an_unwatchable_file(&rpc, &path).await.is_some() {
        let frame = wait_redraw(&mut incoming, |m| message(m).contains("cannot watch")).await;
        let msg = message(&frame);
        assert!(
            msg.contains(&path.to_string_lossy().into_owned()) && msg.contains("autoreload"),
            "the give-up must name the file and what stopped working, got: {msg:?}"
        );
        // …and it really is the end of it: no further arms, no further frames.
        let redraws =
            redraws_while_idle(&mut incoming, std::time::Duration::from_millis(300)).await;
        assert!(
            redraws < 10,
            "a spent retry budget must leave the server idle: {redraws} redraws in 300ms"
        );
    }
    unsafe { std::env::remove_var("BEMTVI_WATCH_RETRY_BASE_MS") };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
    std::fs::remove_file(&path).ok();
}

/// A [`bemtvi_core::host::HostFs`] that models a file caught **mid-write**: while
/// armed, `path` looks truncated (0 bytes, no content) to everyone; the first
/// `open_read` to observe that disarms it, so every access from then on sees the
/// finished write. Every other path goes straight to the real disk.
///
/// That is the shape of the real hazard — a writer that truncates in place and
/// completes an instant later — and it deliberately models the *file*, not a call
/// count, so the test does not have to know how many times a reload stats. Which
/// accesses land on which side of the disarm is exactly what is under test.
struct TornWriteFs {
    real: bemtvi_core::host::StdHostFs,
    path: PathBuf,
    /// Set by the test to put the file mid-write; cleared by the first read.
    torn: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TornWriteFs {
    fn mid_write(&self, p: &std::path::Path) -> bool {
        p == self.path && self.torn.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl bemtvi_core::host::HostFs for TornWriteFs {
    fn exists(&self, p: &std::path::Path) -> bool {
        // A truncated file still exists.
        self.real.exists(p)
    }
    fn open_read(&self, p: &std::path::Path) -> std::io::Result<Box<dyn std::io::Read>> {
        if self.mid_write(p) {
            // Observing the truncation is what "completes" the write behind us.
            self.torn.store(false, std::sync::atomic::Ordering::SeqCst);
            return Ok(Box::new(std::io::empty()));
        }
        self.real.open_read(p)
    }
    fn stat(&self, p: &std::path::Path) -> Option<bemtvi_core::host::FileStat> {
        let stat = self.real.stat(p);
        if self.mid_write(p) {
            // A just-truncated file: the write's mtime has landed, the bytes have not.
            return stat.map(|s| bemtvi_core::host::FileStat { size: 0, ..s });
        }
        stat
    }
    fn write_atomic(&self, p: &std::path::Path, c: &[u8]) -> std::io::Result<()> {
        self.real.write_atomic(p, c)
    }
    fn read_dir(&self, d: &std::path::Path) -> std::io::Result<Vec<bemtvi_core::host::DirEntry>> {
        self.real.read_dir(d)
    }
    fn canonicalize(&self, p: &std::path::Path) -> std::io::Result<PathBuf> {
        self.real.canonicalize(p)
    }
}

/// A reload that catches an external writer mid-write must not strand the buffer on
/// the truncated content.
///
/// Two things have to hold. The bytes and the on-disk snapshot recorded beside them
/// must describe the *same* file — pair truncated text with the finished write's
/// mtime and size and every later change check answers "unchanged", so the buffer
/// keeps the truncation for good and a `:w` from there puts it back over the real
/// file. And the reload has to notice and re-read, because nothing is guaranteed to
/// come along and try again: the watch that usually would is being re-armed across
/// exactly this window.
///
/// In the wild the tear is the truncate window of a non-atomic writer — a shell `>`
/// redirect, a build step. It is what made
/// `an_external_change_autoreloads_via_the_buffer_watch` flake: the watch fired on the
/// truncate, the reload read an empty file, and nothing ever revisited it.
#[tokio::test]
async fn a_reload_that_catches_a_torn_write_still_converges() {
    let path = temp_path("torn-write");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let torn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let init = ServerInit {
        file: Some(path.to_string_lossy().into_owned()),
        host_fs: Some(Box::new(TornWriteFs {
            real: bemtvi_core::host::StdHostFs,
            path: path.clone(),
            torn: torn.clone(),
        })),
        ..Default::default()
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);

    // The external write completes on the real disk, but the reload below is made to
    // catch the file mid-write.
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    torn.store(true, std::sync::atomic::Ordering::SeqCst);
    feed_sync(&rpc, ":checktime<CR>").await;

    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three"],
        "a reload that caught the writer mid-write must re-read rather than strand \
         the buffer on the truncated text"
    );
    std::fs::remove_file(&path).ok();
}

// --- `FileChangedShell` / `v:fcs_reason` / `v:fcs_choice` ----------------------
//
// When a changed file is *not* silently autoreloaded, the reconcile fires the
// `FileChangedShell` autocmd with `v:fcs_reason` set ("deleted"/"conflict"/"changed")
// and reads back the `v:fcs_choice` the handler set: "reload"/"edit" reloads (even a
// conflict), "ask" falls through to the W11/W12/E211 warning, and an empty choice
// means the handler took over (no warning, no reload). `FileChangedShellPost` fires
// after a handled change. (Mirrors neovim's `buf_check_timestamp`.)

#[tokio::test]
async fn file_changed_shell_reloads_a_conflict_via_fcs_choice() {
    let path = temp_path("fcs-reload");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    // A handler that records `v:fcs_reason` and redirects the reconcile to a reload —
    // even though the buffer has unsaved edits (a conflict vim would otherwise refuse
    // to clobber with W12). Registered before the external change so it's in place
    // whether the watch or `:checktime` fires first.
    exec_lua(
        &rpc,
        r#"
        vim.g.fcs_reason = ""
        vim.api.nvim_create_autocmd("FileChangedShell", {
          callback = function()
            vim.g.fcs_reason = vim.v.fcs_reason
            vim.v.fcs_choice = "reload"
          end,
        })
        "#,
    )
    .await;

    // Our own unsaved edit — round-trip so it's modified server-side before the write.
    feed(&rpc, "Gomine<Esc>");
    assert!(lines(&rpc).await.contains(&"mine".to_string()));
    std::fs::write(&path, "fresh\n").unwrap();

    // `:checktime` fires `FileChangedShell`; the handler's "reload" choice overrides
    // the W12 default and reloads despite the in-buffer edit. (Poll: the always-on
    // watch may drive the same reconcile.)
    feed(&rpc, ":checktime<CR>");
    let mut got = vec![];
    for _ in 0..100 {
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        got = lines(&rpc).await;
        if got == vec!["fresh"] {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        got,
        vec!["fresh"],
        "a 'reload' v:fcs_choice must reload despite the conflict"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.g.fcs_reason").await.as_str(),
        Some("conflict"),
        "the handler must see v:fcs_reason = 'conflict' for a modified buffer"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn file_changed_shell_handler_suppresses_the_default_warning() {
    let path = temp_path("fcs-handled");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    // 'noautoread' so a change would normally warn W11; a handler that records the
    // reason but leaves `v:fcs_choice` empty "takes over" — neovim then shows no
    // warning and reloads nothing.
    feed(&rpc, ":set noautoread<CR>");
    exec_lua(
        &rpc,
        r#"
        vim.g.fcs_reason = ""
        vim.api.nvim_create_autocmd("FileChangedShell", {
          callback = function() vim.g.fcs_reason = vim.v.fcs_reason end,
        })
        "#,
    )
    .await;
    // Round-trip the option + autocmd before the external write.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(false)
    );
    std::fs::write(&path, "REPLACED\n").unwrap();

    let msg = message(&redraw_after(&rpc, &mut incoming, ":checktime<CR>").await);
    assert!(
        !msg.contains("W11"),
        "a handler that takes over (empty v:fcs_choice) must suppress the W11 warning, got: {msg:?}"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.g.fcs_reason").await.as_str(),
        Some("changed"),
        "the handler must have run and seen v:fcs_reason = 'changed'"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two"],
        "no reload when the handler leaves the choice empty"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn file_changed_shell_post_fires_after_an_autoread_reload() {
    let path = temp_path("fcs-post");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;

    exec_lua(
        &rpc,
        r#"
        vim.g.fcs_post = false
        vim.api.nvim_create_autocmd("FileChangedShellPost", {
          callback = function() vim.g.fcs_post = true end,
        })
        "#,
    )
    .await;

    // 'autoread' is on by default → the change reloads silently (no FileChangedShell),
    // but FileChangedShellPost must still fire afterward.
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    feed(&rpc, ":checktime<CR>");
    let mut posted = Some(false);
    for _ in 0..100 {
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        if lines(&rpc).await == vec!["one", "two", "three"] {
            posted = exec_lua(&rpc, "return vim.g.fcs_post").await.as_bool();
            if posted == Some(true) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        posted,
        Some(true),
        "FileChangedShellPost must fire after an autoread reload"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn lua_vim_cmd_drives_the_editor() {
    // A Lua chunk that opens a file should change what the buffer shows.
    let path = temp_path("lua");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let (rpc, _incoming) = start(None).await;
    let chunk = format!("lua vim.cmd(\"edit {}\")", q(&path));
    rpc.request("btv_command", vec![Value::from(chunk.as_str())])
        .await
        .expect("lua command");

    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn insert_delete_at_end_of_line_joins_the_next_line() {
    // `<Del>` in insert mode is a forward delete; at the end of a line there's no
    // character ahead of the cursor, so it must delete the line break and pull the
    // next line up — the mirror of `<BS>` at column 0.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    // `A` enters insert mode at the end of "foo"; `<Del>` then has nothing ahead
    // of it on the line, so it must join "bar".
    feed(&rpc, "A<Del>");
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
    // The cursor stays put at the join column (insert mode, 0-based col 3).
    feed(&rpc, "baz<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foobazbar"]);
}

#[tokio::test]
async fn insert_delete_at_end_of_last_line_is_a_noop() {
    // At the end of the final line there is no following line to join, so a
    // forward `<Del>` must do nothing rather than eat the phantom trailing newline.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo");
    feed(&rpc, "<Del>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn vertical_motion_preserves_desired_column() {
    let (rpc, _incoming) = start(None).await;
    // Long, short, long — the classic case where j/k must remember the column.
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "ohi<Esc>");
    feed(&rpc, "ogoodbye world<Esc>");

    // Top line, move to column 8 ('r' in "hello world").
    feed(&rpc, "gg8l");
    assert_eq!(cursor(&rpc).await, (1, 8));

    // Down onto the short line: cursor clamps to its last column...
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (2, 1));

    // ...and down again onto a long line: the remembered column is restored.
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (3, 8));

    // Back up through the short line restores it too.
    feed(&rpc, "kk");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn dollar_sticks_to_end_of_line_through_j() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "oto<Esc>");
    feed(&rpc, "oomega!<Esc>");

    // `$` on the first line, then move down: each line lands on its own end.
    feed(&rpc, "gg$");
    assert_eq!(cursor(&rpc).await, (1, 4)); // "alpha" -> last col

    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (2, 1)); // "to" -> last col

    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (3, 5)); // "omega!" -> last col

    // A horizontal move clears the end-of-line stickiness.
    feed(&rpc, "gg0jj");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn view_reflects_typed_text_and_mode() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello");
    // Barrier: ensure the input (and its redraw) have been processed.
    let _ = lines(&rpc).await;

    let view = latest_view(&mut incoming).expect("a redraw view");

    let first = view_lines(&view);
    assert_eq!(first.first().map(String::as_str), Some("hello"));
    assert_eq!(view_str(&view, "mode_label"), "INSERT");
}

#[tokio::test]
async fn capital_r_enters_replace_mode_and_overwrites() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello");
    feed(&rpc, "<Esc>");

    // `R` enters Replace mode: the status line reflects it...
    feed(&rpc, "0R");
    let _ = lines(&rpc).await; // barrier
    let view = latest_view(&mut incoming).expect("a redraw view");
    assert_eq!(view_str(&view, "mode_label"), "REPLACE");

    // ...and typed characters overwrite rather than insert.
    feed(&rpc, "HE");
    assert_eq!(lines(&rpc).await, vec!["HEllo"]);

    // Leaving Replace mode returns to normal.
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await; // barrier
    let view = latest_view(&mut incoming).expect("a redraw view");
    assert_eq!(view_str(&view, "mode_label"), "NORMAL");
}

// ===== linewise change (`cc`/`S`/`Vjc`) places one fresh empty line ==========

#[tokio::test]
async fn cc_on_the_first_line_replaces_it() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>gg");
    feed(&rpc, "ccX<Esc>"); // change the first line
    assert_eq!(lines(&rpc).await, vec!["X", "b", "c"]);
}

#[tokio::test]
async fn cc_on_a_middle_line_replaces_it() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>gg");
    feed(&rpc, "jccX<Esc>"); // change the middle line
    assert_eq!(lines(&rpc).await, vec!["a", "X", "c"]);
}

#[tokio::test]
async fn cc_on_the_last_line_replaces_it_in_place() {
    // Regression: a linewise change of the buffer's final line must reopen the
    // empty line *where that line was*, not before the surviving last line.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>"); // cursor ends on the last line "c"
    feed(&rpc, "ccX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a", "b", "X"]);
}

#[tokio::test]
async fn cc_on_a_single_line_buffer_replaces_it() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ionly<Esc>"); // one line: "only"
    feed(&rpc, "ccX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["X"]);
}

#[tokio::test]
async fn visual_o_moves_cursor_to_the_other_end() {
    // `o` in visual mode jumps the cursor to the opposite end of the selection
    // (and back), leaving the span unchanged.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "v3l"); // anchor col 0, cursor col 3
    assert_eq!(cursor(&rpc).await, (1, 3), "cursor at the extended end");
    feed(&rpc, "o");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "o moved the cursor to the anchor end"
    );
    feed(&rpc, "o");
    assert_eq!(cursor(&rpc).await, (1, 3), "a second o swaps back");
}

#[tokio::test]
async fn visual_o_swaps_the_extendable_end() {
    // After `o`, extending now grows/shrinks the *former anchor* end: starting
    // anchored at col 0 with the cursor at col 2, `o` then `l` pulls the left edge
    // in by one, so the selection is cols 1..2 ("el").
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "vllold"); // v, extend to col2, swap ends, l, delete "el"
    assert_eq!(
        lines(&rpc).await,
        vec!["hlo world"],
        "o moved the active end so l shrank the selection from the left"
    );
}

#[tokio::test]
async fn visual_linewise_change_at_eof_replaces_in_place() {
    // `Vjc` over the buffer's final two lines reopens a single empty line in
    // their place, then takes the typed text.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<Esc>ggjj"); // cursor on "c", the third of four
    feed(&rpc, "VjcX<Esc>"); // change the last two lines (c, d)
    assert_eq!(lines(&rpc).await, vec!["a", "b", "X"]);
}

// ----- :echo / :echomsg / :echoerr -------------------------------------------

#[tokio::test]
async fn echo_shows_a_string_literal_on_the_message_line() {
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo \"hello world\"<CR>").await);
    assert_eq!(msg, "hello world");
}

#[tokio::test]
async fn echo_keeps_non_ascii_intact() {
    // The string lexers must slice UTF-8, not reinterpret bytes: pushing each
    // byte `as char` (Latin-1) renders `héllo` as `hÃ©llo` and `中` as `ä¸­`.
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo \"héllo 中\"<CR>").await);
    assert_eq!(msg, "héllo 中");
    // Single-quoted strings share the lexer shape (and shared the bug).
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo 'naïve'<CR>").await);
    assert_eq!(msg, "naïve");
}

#[tokio::test]
async fn echo_evaluates_concatenation_and_arithmetic() {
    // `.` concatenates; arithmetic respects precedence (1 + 2*3 = 7).
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo \"n=\" . (1 + 2 * 3)<CR>").await);
    assert_eq!(msg, "n=7");
}

#[tokio::test]
async fn echo_joins_multiple_expressions_with_a_space() {
    // Space-separated top-level expressions are joined with a single space.
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo 'a' 'b' 1<CR>").await);
    assert_eq!(msg, "a b 1");
}

#[tokio::test]
async fn echo_integer_division_truncates_like_vim() {
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo 7 / 2<CR>").await);
    assert_eq!(msg, "3", "integer division truncates toward zero");
}

/// The one integer division that overflows — `i64::MIN / -1` (and its `%`) —
/// panics in Rust even in release builds; `:echo` must answer instead of taking
/// the server down.
#[tokio::test]
async fn echo_min_int_divided_by_minus_one_does_not_panic() {
    let (rpc, mut incoming) = start(None).await;
    let min = "(-9223372036854775807 - 1)";
    let msg = message(&redraw_after(&rpc, &mut incoming, &format!(":echo {min} % -1<CR>")).await);
    assert_eq!(msg, "0");
    let msg = message(&redraw_after(&rpc, &mut incoming, &format!(":echo {min} / -1<CR>")).await);
    assert_eq!(msg, "-9223372036854775808");
}

/// The expression parser recurses per nesting level, so an absurdly nested
/// expression overflowed the server thread's stack (an abort, not a catchable
/// panic). It fails loud past a depth limit instead.
#[tokio::test]
async fn echo_of_a_pathologically_nested_expression_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.cmd("echo " .. string.rep("(", 200000) .. "1" .. string.rep(")", 200000))"#,
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "").await);
    assert!(msg.contains("E"), "expected a loud error, got {msg:?}");
    exec_lua(
        &rpc,
        r#"vim.cmd("echo " .. string.rep("-", 200000) .. "1")"#,
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "").await);
    assert!(msg.contains("E"), "expected a loud error, got {msg:?}");
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo ((1 + 2)) * -(-3)<CR>").await);
    assert_eq!(msg, "9");
}

#[tokio::test]
async fn echo_is_transient_but_echomsg_is_recorded() {
    // `:echo` shows on the message line without joining `:messages`; `:echomsg`
    // records. The `:messages` listing must hold the echomsg line, not the echo one.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":echo \"transient-line\"<CR>");
    feed(&rpc, ":echomsg \"kept-line\"<CR>");
    feed(&rpc, ":messages<CR>");
    let history = lines(&rpc).await;
    assert!(
        history.contains(&"kept-line".to_string()),
        ":echomsg is recorded; history was {history:?}"
    );
    assert!(
        !history.contains(&"transient-line".to_string()),
        ":echo must not be recorded; history was {history:?}"
    );
}

#[tokio::test]
async fn echo_of_an_undefined_variable_fails_loud() {
    // Variables/functions aren't evaluable in core; rather than echoing an empty
    // string (making a typo look fine), :echo reports the unevaluable reference.
    let (rpc, mut incoming) = start(None).await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":echo somevar<CR>").await);
    assert!(
        msg.contains("E121") && msg.contains("somevar"),
        "expected an E121 naming the variable, got {msg:?}"
    );
}

#[tokio::test]
async fn helptags_points_at_the_bemtvi_help_plugin() {
    // The help system ships as the optional bemtvi-help plugin; tag generation is its
    // `:BtvHelptags`. With the plugin absent, `:helptags` must not fall through to the
    // unknown-command path (which can abort a plugin manager mid-install): it points
    // the user at the plugin, and leaves the buffer untouched.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let msg = message(&redraw_after(&rpc, &mut incoming, ":helptags ALL<CR>").await);
    assert!(
        msg.contains("bemtvi-help") && msg.contains("BtvHelptags"),
        "expected a message pointing at the plugin + :BtvHelptags, got {msg:?}"
    );
    assert!(
        !msg.contains("E492") && !msg.contains("Not an editor command"),
        "must be recognized, not reported as an unknown command: {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "helptags must not disturb the buffer"
    );
}

#[tokio::test]
async fn help_without_the_plugin_points_at_it() {
    // `:help` is provided by the optional bemtvi-help plugin. With it absent, the
    // command points the user at the plugin instead of the bare unknown-command
    // error, and does not disturb the buffer.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let msg = message(&redraw_after(&rpc, &mut incoming, ":help motion<CR>").await);
    assert!(
        msg.contains("bemtvi-help"),
        "expected a message pointing at the bemtvi-help plugin, got {msg:?}"
    );
    assert!(
        !msg.contains("E492") && !msg.contains("Not an editor command"),
        "must be recognized, not reported as an unknown command: {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "help must not disturb the buffer"
    );
}

// ===== inclusive motions never swallow a line break ==========================
// vim's rule: a charwise inclusive motion whose end sits where no character
// exists (`$`/`g$` on an empty line, `e` stopped at the buffer's final newline)
// has nothing to include — the line break is never part of the range. Verified
// against nvim: `d$`/`y$` on an empty line are no-ops, `c$`/`cl`/`cw` there
// still enter Insert without joining, `de`/`ye` at the buffer end act on the
// word alone.

#[tokio::test]
async fn d_dollar_on_an_empty_line_is_a_noop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggd$");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "", "b"],
        "d$ on an empty line must not delete the line break"
    );
}

#[tokio::test]
async fn y_dollar_on_an_empty_line_yanks_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggy$p");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "", "b"],
        "y$ on an empty line yanks nothing, so p pastes nothing"
    );
}

#[tokio::test]
async fn c_dollar_on_an_empty_line_enters_insert_without_joining() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggc$XY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a", "XY", "b"]);
}

#[tokio::test]
async fn c_l_on_an_empty_line_enters_insert() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggclXY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a", "XY", "b"]);
}

#[tokio::test]
async fn c_w_on_an_empty_line_enters_insert_without_joining() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggcwXY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a", "XY", "b"]);
}

#[tokio::test]
async fn a_shift_over_an_empty_motion_still_shifts_the_line() {
    // vim's `op_shift` works on the operated lines even when the motion did not
    // move: `>h` / `>0` in column 0, `>{` / `>(` at the top of the buffer.
    let (rpc, _incoming) = start(None).await;
    exec_lua(&rpc, "btv.o.indentdetect = false").await;
    command(&rpc, "set sw=8 ts=8 noet").await;
    for keys in [">h", ">0", ">{", ">("] {
        assert_eq!(
            edit_probe(&rpc, &["\tfoo", "x"], (1, 0), keys).await,
            vec!["\t\tfoo", "x"],
            "{keys}"
        );
    }
    assert_eq!(
        edit_probe(&rpc, &["\tfoo", "x"], (1, 0), "<h").await,
        vec!["foo", "x"]
    );
}

#[tokio::test]
async fn a_case_operator_made_linewise_leaves_the_cursor_where_its_text_began() {
    // `gU(` from a blank line reaches back to a sentence starting in the indent:
    // the exclusive motion becomes linewise, and the cursor lands where the
    // motion's text began, as the charwise operator would have left it.
    let (rpc, _incoming) = start(None).await;
    edit_probe(&rpc, &["First.", "  third end.", "", "x"], (3, 0), "gU(").await;
    assert_eq!(lines(&rpc).await, vec!["First.", "  THIRD END.", "", "x"]);
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn visual_y_leaves_the_cursor_at_the_start_of_the_first_line() {
    let (rpc, _incoming) = start(None).await;
    edit_probe(&rpc, &["one two", "three four"], (2, 4), "vlY").await;
    assert_eq!(cursor(&rpc).await, (2, 0));
    edit_probe(&rpc, &["one two", "three four"], (2, 4), "vkY").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn line_motions_plus_minus_underscore_bar_and_screen_lines() {
    let (rpc, _incoming) = start(None).await;
    let text = ["  aa bb cc", "bbbbbbbb x", "  cc dd", "dd ee"];
    for (at, keys, want) in [
        ((2, 6), "+", (3, 2)),
        ((2, 6), "<CR>", (3, 2)),
        ((2, 6), "-", (1, 2)),
        ((3, 4), "2-", (1, 2)),
        ((2, 6), "_", (2, 0)),
        ((2, 6), "2_", (3, 2)),
        ((2, 6), "5|", (2, 4)),
        ((2, 6), "H", (1, 2)),
        ((2, 6), "M", (2, 0)),
        ((2, 6), "L", (4, 0)),
        ((4, 3), "2L", (3, 2)),
        ((1, 0), "2H", (2, 0)),
    ] {
        edit_probe(&rpc, &text, at, keys).await;
        assert_eq!(cursor(&rpc).await, want, "{keys:?} from {at:?}");
    }
    // As operator targets: `+`/`-`/`_`/`H`/`L` are linewise, `|` exclusive.
    assert_eq!(
        edit_probe(&rpc, &text, (2, 6), "d+").await,
        vec!["  aa bb cc", "dd ee"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (2, 6), "d-").await,
        vec!["  cc dd", "dd ee"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (2, 6), "d_").await,
        vec!["  aa bb cc", "  cc dd", "dd ee"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (2, 6), "d5|").await,
        vec!["  aa bb cc", "bbbbbb x", "  cc dd", "dd ee"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (2, 6), "dL").await,
        vec!["  aa bb cc"]
    );
    // `+` on the last line and `-` on the first fail.
    edit_probe(&rpc, &text, (4, 1), "+").await;
    assert_eq!(cursor(&rpc).await, (4, 1));
    assert_eq!(edit_probe(&rpc, &text, (4, 1), "d2_").await, text.to_vec());
    assert_eq!(edit_probe(&rpc, &text, (1, 3), "d-").await, text.to_vec());
}

#[tokio::test]
async fn a_linewise_yank_or_fold_leaves_the_cursor_at_the_operated_start() {
    // vim puts the cursor at the start of the operated text: for a linewise
    // motion that is the earlier of the cursor and the motion's landing spot
    // (`gg` / `G` / `N%` land on the first non-blank); `zf` over a charwise
    // motion keeps the charwise start.
    let (rpc, _incoming) = start(None).await;
    let text = ["  aa bb cc", "bbbbbbbb x", "  cc dd", "dd ee"];
    for (at, keys, want) in [
        ((2, 6), "ygg", (1, 2)),
        ((4, 1), "yG", (4, 0)),
        ((2, 6), "y3%", (1, 2)),
        ((2, 6), "yk", (1, 6)),
        ((2, 6), "yj", (2, 6)),
        ((2, 6), "yy", (2, 6)),
        ((2, 6), "2yy", (2, 6)),
        ((2, 6), "y_", (2, 6)),
        ((3, 4), "y2_", (3, 4)),
        ((2, 6), "zfj", (2, 6)),
        ((2, 6), "zfk", (1, 6)),
        ((3, 4), "zf{", (1, 0)),
        ((2, 6), "zfap", (1, 0)),
        ((2, 6), "zfiw", (2, 0)),
        ((1, 2), "zf}", (1, 2)),
    ] {
        edit_probe(&rpc, &text, at, keys).await;
        assert_eq!(cursor(&rpc).await, want, "{keys:?} from {at:?}");
        command(&rpc, "normal! zE").await;
    }
}

#[tokio::test]
async fn x_upper_counts_characters_not_bytes() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(edit_probe(&rpc, &["aé b"], (1, 3), "2X").await, vec![" b"]);
    edit_probe(&rpc, &["aé b"], (1, 3), "X").await;
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn s_on_the_last_char_inserts_where_the_char_was() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["abc", "x"], (1, 0), "$s=<Esc>").await,
        vec!["ab=", "x"]
    );
    assert_eq!(
        edit_probe(&rpc, &["abc", "x"], (1, 1), "5s=<Esc>").await,
        vec!["a=", "x"]
    );
}

#[tokio::test]
async fn cw_on_the_last_char_of_a_word_changes_only_that_char() {
    let (rpc, _incoming) = start(None).await;
    // vim's `cw` is `ce` that stops at the end of the word it starts on.
    assert_eq!(
        edit_probe(&rpc, &["foo bar"], (1, 2), "cwX<Esc>").await,
        vec!["foX bar"]
    );
    assert_eq!(
        edit_probe(&rpc, &["ab !", "next"], (1, 3), "cWX<Esc>").await,
        vec!["ab X", "next"]
    );
    assert_eq!(
        edit_probe(&rpc, &["café", "naïve"], (1, 3), "cwX<Esc>").await,
        vec!["cafX", "naïve"]
    );
    // A count still reaches the end of the next word.
    assert_eq!(
        edit_probe(&rpc, &["foo bar baz"], (1, 2), "c2wX<Esc>").await,
        vec!["foX baz"]
    );
}

#[tokio::test]
async fn de_at_the_buffer_end_deletes_the_word_not_the_line_break() {
    // `e` from the buffer's last word stops at the final newline; the inclusive
    // delete takes the word but never the line break (vim leaves the empty line).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "Gde");
    assert_eq!(lines(&rpc).await, vec!["a", "", ""]);
}

#[tokio::test]
async fn ye_at_the_buffer_end_yanks_without_the_line_break() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "GyeP");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "", "bb"],
        "ye yanks the bare word (charwise, no newline), so P doubles it in place"
    );
}

#[tokio::test]
async fn visual_d_on_an_empty_line_deletes_the_line_break() {
    // The visual selection on an empty line *does* cover the line break (vim:
    // `vd` there joins) — unlike the operator-motion `d$`, which must not.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR><CR>b<Esc>");
    feed(&rpc, "2ggvd");
    assert_eq!(lines(&rpc).await, vec!["a", "b"]);
}

// ---- `i_CTRL-O`: one Normal command from Insert, then resume Insert ----------

#[tokio::test]
async fn insert_ctrl_o_runs_one_motion_then_resumes_insert() {
    // `<C-o>` from Insert drops to Normal for exactly one command; here a `$`
    // motion, which resumes Insert *after* the last char (ready to append).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>0i"); // insert at column 0 of "hello"
    feed(&rpc, "<C-o>$");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, vec!["hello!"]);
    assert_eq!(
        mode(&rpc).await,
        "i",
        "we are back in Insert after the one command"
    );
}

#[tokio::test]
async fn insert_ctrl_o_reports_ni_insert_until_the_command_completes() {
    // While the one-shot is pending, `mode()` reports `niI` (Normal-for-one,
    // resuming Insert) — not a plain `n`; the command settles it back to `i`.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>A"); // append at end of "ab"
    feed(&rpc, "<C-o>");
    assert_eq!(mode(&rpc).await, "niI", "insert-normal reports niI");
    feed(&rpc, "0"); // the one command (a motion to column 0)
    assert_eq!(mode(&rpc).await, "i", "resumed Insert once the command ran");
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, vec!["Xab"]);
}

#[tokio::test]
async fn insert_ctrl_o_edit_deletes_then_resumes_the_same_session() {
    // A one-shot *edit* (`x`) runs and then hands input straight back to the
    // interrupted Insert session at the cursor's new position.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>0i"); // insert at column 0
    feed(&rpc, "<C-o>x"); // delete the 'h', back to Insert at column 0
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, vec!["Zello"]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn insert_ctrl_o_spans_an_operator_motion() {
    // The "one command" is a whole operator+motion (`dw`), which unfolds across
    // two keystrokes before Insert resumes — the flag stays armed until it settles.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>0i"); // insert at column 0 of "foo bar"
    feed(&rpc, "<C-o>dw"); // delete "foo ", resume Insert at column 0
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, vec!["Xbar"]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn insert_ctrl_o_dd_deletes_the_line_then_resumes_insert() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>obar<Esc>I"); // two lines, insert at start of "bar"
    feed(&rpc, "<C-o>dd"); // delete "bar", land on "foo", resume Insert
    feed(&rpc, "X");
    assert_eq!(lines(&rpc).await, vec!["Xfoo"]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn insert_ctrl_o_open_line_stays_in_insert_without_a_second_shot() {
    // When the one-shot command enters Insert on its own (`o`), the pending
    // return is simply consumed — you keep typing, and a later Normal command
    // does *not* wrongly bounce back into Insert.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>A"); // append at end of "foo"
    feed(&rpc, "<C-o>obar"); // open a line below and type into it
    assert_eq!(lines(&rpc).await, vec!["foo", "bar"]);
    assert_eq!(mode(&rpc).await, "i");
    // Leaving Insert and running a plain Normal command must stay in Normal —
    // proving the one-shot flag was not left dangling.
    feed(&rpc, "<Esc>x");
    assert_eq!(mode(&rpc).await, "n");
    assert_eq!(lines(&rpc).await, vec!["foo", "ba"]);
}

#[tokio::test]
async fn insert_ctrl_o_ex_command_resumes_insert_and_keeps_the_next_space() {
    // `<C-o>:{cmd}<CR>` is one command too: Insert resumes, and the very next key
    // types — including a space, which used to vanish.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<C-o>:nohlsearch<CR>");
    assert_eq!(mode(&rpc).await, "i", "back in Insert after the ex command");
    feed(&rpc, " bar");
    assert_eq!(lines(&rpc).await, vec!["foo bar"]);
}

#[tokio::test]
async fn insert_ctrl_o_cancelled_command_line_resumes_insert() {
    // Abandoning the line with `<Esc>` is the end of the one command as well.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<C-o>:abc<Esc>");
    assert_eq!(mode(&rpc).await, "i", "back in Insert after <Esc>");
    feed(&rpc, " bar");
    assert_eq!(lines(&rpc).await, vec!["foo bar"]);
}

#[tokio::test]
async fn insert_ctrl_o_normal_runs_all_its_keys_before_insert_resumes() {
    // `:normal` keys are Normal commands to the last one: Insert resumes after
    // the ex command, not after its first key.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>0i");
    feed(&rpc, "<C-o>:normal! xx<CR>");
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, vec!["Zllo"]);
}

#[tokio::test]
async fn insert_ctrl_o_write_resumes_insert() {
    // `<C-o>:w` saves and goes back to Insert, as vim does.
    let path = temp_path("ctrl-o-write").to_string_lossy().into_owned();
    std::fs::write(&path, "").expect("write temp file");
    let (rpc, _incoming) = start(Some(path.clone())).await;
    feed(&rpc, "ifoo<C-o>:w<CR>");
    assert_eq!(mode(&rpc).await, "i", "back in Insert after :w");
    feed(&rpc, " bar");
    assert_eq!(lines(&rpc).await, vec!["foo bar"]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn replace_ctrl_o_resumes_replace_not_insert() {
    // From Replace mode, `<C-o>` returns to Replace (reported `niR`), not Insert.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>0R"); // Replace mode at column 0
    assert_eq!(mode(&rpc).await, "R");
    feed(&rpc, "<C-o>");
    assert_eq!(
        mode(&rpc).await,
        "niR",
        "insert-normal from Replace reports niR"
    );
    feed(&rpc, "l"); // one motion command
    assert_eq!(mode(&rpc).await, "R", "resumed Replace, not Insert");
    feed(&rpc, "X"); // overtypes rather than inserts
    assert_eq!(lines(&rpc).await, vec!["hXllo"]);
}

#[tokio::test]
async fn big_word_motions_span_punctuation() {
    // `W`/`B`/`E` treat a run of non-blank chars as one WORD, so they skip the
    // punctuation their small-word siblings (`w`/`b`/`e`) stop at.
    let (rpc, _incoming) = start(None).await;
    // cols: f0 o1 o2 .3 b4 a5 r6 <sp>7 b8 a9 z10
    feed(&rpc, "ifoo.bar baz<Esc>");

    // `w` stops at the `.` inside `foo.bar`; `W` skips the whole WORD to `baz`.
    feed(&rpc, "0w");
    assert_eq!(cursor(&rpc).await, (1, 3), "w stops at punctuation");
    feed(&rpc, "0W");
    assert_eq!(cursor(&rpc).await, (1, 8), "W jumps past foo.bar to baz");

    // `e` ends on `foo`; `E` ends on the whole `foo.bar` WORD (the `r`).
    feed(&rpc, "0e");
    assert_eq!(cursor(&rpc).await, (1, 2), "e ends foo");
    feed(&rpc, "0E");
    assert_eq!(cursor(&rpc).await, (1, 6), "E ends foo.bar");

    // From `baz`, `b` walks back only to `bar`; `B` walks back over the whole WORD.
    feed(&rpc, "0Wb");
    assert_eq!(cursor(&rpc).await, (1, 4), "b stops at bar");
    feed(&rpc, "0WB");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "B jumps back to start of foo.bar"
    );
}

// ===== insert-mode <Home> / <End> ============================================

/// `<Home>` / `<End>` move to the first / last column *without leaving Insert*,
/// the way every other editor (and vim's `i_<Home>` / `i_<End>`) behaves. They
/// used to fall through Insert's key match unhandled, so the cursor never moved.
#[tokio::test]
async fn insert_home_and_end_move_within_the_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world");
    // Mid-line: `<Home>` lands on column 0 and typed text prepends.
    feed(&rpc, "<Home>X");
    assert_eq!(lines(&rpc).await, vec!["Xhello world"]);
    assert_eq!(cursor(&rpc).await, (1, 1));

    // `<End>` goes *past* the last character (the insert-mode append column), so
    // typing there lands at the end of the line rather than before its last char.
    feed(&rpc, "<End>Y");
    assert_eq!(lines(&rpc).await, vec!["Xhello worldY"]);

    // Still in Insert the whole time — no `<Esc>` was implied.
    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "<Esc>");
}

/// `<Home>` / `<End>` act on the cursor's own line, not the buffer, and survive
/// a vertical move — the plain per-line semantics.
#[tokio::test]
async fn insert_home_and_end_act_on_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    feed(&rpc, "ggji"); // Insert on line 2 ("two")
    feed(&rpc, "<End>!");
    assert_eq!(lines(&rpc).await, vec!["one", "two!", "three"]);
    feed(&rpc, "<Home>?");
    assert_eq!(lines(&rpc).await, vec!["one", "?two!", "three"]);
    assert_eq!(cursor(&rpc).await, (2, 1));
    feed(&rpc, "<Esc>");
}

#[tokio::test]
async fn replace_mode_backspace_restores_the_overtyped_text() {
    // `R` overtypes; `<BS>` must give the original characters back (vim's replace
    // stack), not delete them. Multibyte replacements restore their ASCII originals
    // and characters appended past the end are simply removed.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>0Rxyz<BS><BS><Esc>");
    assert_eq!(lines(&rpc).await, vec!["xbc"]);
    feed(&rpc, "ccabc<Esc>0R中文xyz<BS><BS><BS><BS><Esc>");
    assert_eq!(lines(&rpc).await, vec!["中bc"]);
    // Backspacing past where the Replace began only moves the cursor.
    feed(&rpc, "ccabc<Esc>$Rx<BS><BS>y<Esc>");
    assert_eq!(lines(&rpc).await, vec!["ayc"]);
}

#[tokio::test]
async fn insert_ctrl_w_and_ctrl_u_delete_instead_of_typing_a_letter() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab cd  <C-w>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["ab X"]);
    feed(&rpc, "ccfoo.bar<C-w><C-w>Y<Esc>");
    assert_eq!(lines(&rpc).await, vec!["fooY"]);
    feed(&rpc, "ccab cd<C-u>Z<Esc>");
    assert_eq!(lines(&rpc).await, vec!["Z"]);
    // An unbound Ctrl chord inserts nothing.
    feed(&rpc, "ccq<C-b>r<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qr"]);
}

#[tokio::test]
async fn ctrl_h_j_m_i_are_backspace_enter_and_tab_in_insert_replace_and_cmdline() {
    let (rpc, _incoming) = start(None).await;
    // Insert: `i_CTRL-H` rubs out, `i_CTRL-J` / `i_CTRL-M` break the line,
    // `i_CTRL-I` types a tab.
    assert_eq!(
        edit_probe(&rpc, &["ab"], (1, 1), "ixy<C-h>z<Esc>").await,
        vec!["axzb"]
    );
    assert_eq!(
        edit_probe(&rpc, &["ab"], (1, 1), "i<C-j>c<C-m>d<Esc>").await,
        vec!["a", "c", "db"]
    );
    assert_eq!(
        edit_probe(&rpc, &["ab"], (1, 1), "i<C-i><Esc>").await,
        vec!["a\tb"]
    );
    // Replace: `<C-h>` is Replace mode's `<BS>`, restoring what was overtyped.
    assert_eq!(
        edit_probe(&rpc, &["abcd"], (1, 0), "Rxy<C-h><Esc>").await,
        vec!["xbcd"]
    );
    // Command line: `c_CTRL-H` deletes, `c_CTRL-J` / `c_CTRL-M` execute.
    assert_eq!(
        edit_probe(&rpc, &["x"], (1, 0), ":s/x/yz<C-h>Q<C-m>").await,
        vec!["yQ"]
    );
    assert_eq!(
        edit_probe(&rpc, &["x"], (1, 0), ":s/x/w<C-j>").await,
        vec!["w"]
    );
    // A user's own mapping on the chord still wins.
    exec_lua(&rpc, "btv.keymap.set('i', '<C-h>', 'MAPPED')").await;
    assert_eq!(
        edit_probe(&rpc, &["ab"], (1, 1), "i<C-h><Esc>").await,
        vec!["aMAPPEDb"]
    );
}

#[tokio::test]
async fn visual_change_on_the_last_empty_line_keeps_the_cursor_in_the_buffer() {
    // `vc` on a trailing empty line deletes its line break, so the line is gone:
    // the Insert cursor must land on the new last line, not on the phantom line
    // past the end, where the typed `X` vanished.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<CR><CR><Esc>vcX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello", "X"]);
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn ctrl_o_restarts_the_insert_where_its_command_left_the_cursor() {
    // vim re-enters Insert after an `i_CTRL-O` command as a fresh insert at the
    // cursor, so `<C-u>` stops there. The stale start (byte 5, from `A`) used to
    // fall inside the second `中` typed after `<C-o>0` — the server panicked.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab\u{4e2d}<Esc>A<C-o>0\u{4e2d}\u{4e2d}<C-u>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["Xab\u{4e2d}"]);
}

#[tokio::test]
async fn replace_backspace_past_the_start_then_ctrl_u_survives_multibyte() {
    // `R` on `_`, `<BS>` back over the 2-byte `é` (only moves: it is before the
    // session's start), then overtype with a decomposed `e` + U+0301: the start of
    // the session must follow the cursor back, or `<C-u>` slices the line at the
    // stale byte 2 — inside the combining mark — and the server panics.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i\u{e9}_<Esc>R<BS>e\u{301}<C-u>X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["X"]);
}

#[tokio::test]
async fn huge_counts_neither_hang_nor_crash_the_server() {
    // Each motion stops at the buffer boundary instead of spinning through the
    // whole count, and a count past `u64::MAX` saturates instead of overflowing.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc def<CR>ghi<Esc>gg0");
    for keys in [
        "99999999999l",
        "99999999999w",
        "gg99999999999e",
        "99999999999b",
        "99999999999W",
        "gg99999999999E",
        "99999999999B",
        "99999999999h",
        "99999999999d99999999999l",
        "u",
    ] {
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            feed_sync(&rpc, keys).await;
        })
        .await;
        assert!(r.is_ok(), "`{keys}` must finish promptly");
    }
    assert_eq!(cursor(&rpc).await.0, 1, "the server is still responsive");
}

#[tokio::test]
async fn helix_count_past_u64_saturates_instead_of_crashing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>gg0:helix<CR>");
    feed_sync(&rpc, "99999999999999999999999999j").await;
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "an overflowing count clamps to the last line"
    );
}

#[tokio::test]
async fn paste_with_an_oversized_count_fails_loud_instead_of_aborting() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iab<Esc>0yl");
    let msg = message_after(&rpc, &mut incoming, "99999999999P").await;
    assert!(msg.contains("E1240"), "got {msg:?}");
    assert_eq!(lines(&rpc).await, vec!["ab"], "nothing was pasted");
    feed(&rpc, "3P");
    assert_eq!(lines(&rpc).await, vec!["aaaab"]);
}

#[tokio::test]
async fn a_single_put_of_a_register_larger_than_the_count_cap_still_lands() {
    let (rpc, _incoming) = start(None).await;
    // 65 MiB of text — more than the cap on a counted put, which guards the
    // multiplication, not the size of a register that already exists.
    exec_lua(
        &rpc,
        "btv.reg.set('a', string.rep(string.rep('x', 1023) .. '\\n', 65 * 1024))",
    )
    .await;
    feed(&rpc, "\"aP");
    assert_eq!(
        exec_lua(&rpc, "return btv.buf.line_count(0)").await,
        Value::from(65 * 1024 + 1)
    );
}

/// A count typed with more digits than fit a machine word saturates instead of
/// overflowing the `count * 10 + digit` accumulation — a
/// debug-build panic that took the server down.
#[tokio::test]
async fn an_absurdly_long_count_saturates_instead_of_overflowing() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>def<Esc>gg0");
    feed(&rpc, "99999999999999999999999x");
    assert_eq!(lines(&rpc).await, vec!["", "def"]);
    feed(&rpc, "d99999999999999999999999j");
    assert_eq!(lines(&rpc).await, vec![""]);
}

/// `S` is vim's synonym for `cc`: change whole line(s), keeping the indent
/// rules of `cc`. It used to be unbound, so `SX<Esc>` ran `X` in Normal mode.
#[tokio::test]
async fn capital_s_changes_whole_lines_like_cc() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");
    feed(&rpc, "SX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["X", "bbb", "ccc"]);
    feed(&rpc, "j2SY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["X", "Y"]);
}

/// `{count}go` goes to byte `{count}` of the buffer (1-based, newlines counted;
/// a bare `go` is byte 1). It used to fall through to `o` and open a line.
#[tokio::test]
async fn count_go_jumps_to_a_byte_offset() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>def<CR>ghi<Esc>");
    feed(&rpc, "6go");
    assert_eq!(lines(&rpc).await, vec!["abc", "def", "ghi"]);
    assert_eq!(cursor(&rpc).await, (2, 1));
    feed(&rpc, "go");
    assert_eq!(cursor(&rpc).await, (1, 0));
    feed(&rpc, "999go");
    assert_eq!(cursor(&rpc).await, (3, 2));
}

/// A `g`-prefixed key bemtvi does not implement is a dead end, never the
/// un-prefixed command: `gX` must not run `X`, nor `gD` run `D`.
#[tokio::test]
async fn an_unimplemented_g_command_does_not_fall_through_to_its_suffix() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>def<Esc>gg");
    feed(&rpc, "gX");
    assert_eq!(lines(&rpc).await, vec!["abc", "def"]);
    feed(&rpc, "gD");
    assert_eq!(lines(&rpc).await, vec!["abc", "def"]);
}

/// The escaped-trailing-space rule (`:set fillchars=eob:\ `) keeps the one
/// whitespace char after an odd backslash run. It kept one *byte*: a multibyte
/// whitespace char there (U+3000, an IME's full-width space) was sliced through
/// the middle — a char-boundary panic on a typed command line.
#[tokio::test]
async fn an_escaped_multibyte_trailing_space_does_not_split_a_char() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    feed(&rpc, ":echo 'x'\\\u{3000}<CR>");
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}

#[tokio::test]
async fn an_insert_count_lands_the_typed_text_count_times() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "3ix<Esc>");
    assert_eq!(lines(&rpc).await, vec!["xxx"]);
    feed(&rpc, "2Ayz<Esc>");
    assert_eq!(lines(&rpc).await, vec!["xxxyzyz"]);
    // Replace overtypes each copy.
    feed(&rpc, "03Rab<Esc>");
    assert_eq!(lines(&rpc).await, vec!["abababz"]);
    // One undo takes the whole counted change back; `.` repeats it with its count.
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["xxxyzyz"]);
    feed(&rpc, "ccq<Esc>03iw<Esc>.");
    assert_eq!(lines(&rpc).await, vec!["wwwwwwq"]);
    feed(&rpc, "cc<Esc>2ia<CR>b<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a", "ba", "b"]);
    // A count too big to build is refused loudly rather than freezing the editor.
    feed(&rpc, "ggcc<Esc>99999999ix<Esc>");
    assert_eq!(lines(&rpc).await[0], "x");
}

// ----- vim ground truth (nvim --clean): joins, counted D/C, Y ----------------

#[tokio::test]
async fn join_adds_no_space_after_a_blank_before_a_paren_or_around_an_empty_line() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["foo  ", "   bar"], (1, 0), "J").await,
        vec!["foo  bar"]
    );
    assert_eq!(
        edit_probe(&rpc, &["foo", ")bar"], (1, 0), "J").await,
        vec!["foo)bar"]
    );
    assert_eq!(
        edit_probe(&rpc, &["foo", ""], (1, 0), "J").await,
        vec!["foo"]
    );
    assert_eq!(
        edit_probe(&rpc, &["", "foo"], (1, 0), "J").await,
        vec!["foo"]
    );
    assert_eq!(
        edit_probe(&rpc, &["a", "  b  ", "c)", "", "d"], (1, 0), "5J").await,
        vec!["a b  c) d"]
    );
    // The blank that suppresses the space is the end of the line being joined,
    // not of the text joined so far: past an empty line the space comes back.
    assert_eq!(
        edit_probe(&rpc, &["baz  ", "", "  (paren"], (1, 0), "3J").await,
        vec!["baz   (paren"]
    );
    // A line ending in a multi-byte character does not end in a blank.
    assert_eq!(
        edit_probe(&rpc, &["café", "naïve"], (1, 0), "J").await,
        vec!["café naïve"]
    );
}

#[tokio::test]
async fn gj_joins_lines_verbatim_and_visual_j_joins_the_selection() {
    let (rpc, _incoming) = start(None).await;
    let text = ["a", "  b  ", "c)", "", "d", "e"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "gJ").await,
        vec!["a  b  ", "c)", "", "d", "e"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "3gJ").await,
        vec!["a  b  c)", "", "d", "e"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "vjjJ").await,
        vec!["a b  c)", "", "d", "e"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (3, 0), "VGJ").await,
        vec!["a", "  b  ", "c) d e"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (5, 0), "vgJ").await,
        vec!["a", "  b  ", "c)", "", "de"]
    );
    // On the last line there is nothing to join: the cursor stays put.
    edit_probe(&rpc, &["one", "last line"], (2, 5), "vJ").await;
    assert_eq!(cursor(&rpc).await, (2, 5));
}

#[tokio::test]
async fn change_to_eol_mid_line_inserts_where_the_text_was_cut() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["alpha beta"], (1, 5), "CQ<Esc>").await,
        vec!["alphaQ"]
    );
    assert_eq!(
        edit_probe(&rpc, &["  indented line"], (1, 2), "CQ<Esc>").await,
        vec!["  Q"]
    );
}

#[tokio::test]
async fn counted_d_and_c_run_to_the_end_of_a_later_line() {
    let (rpc, _incoming) = start(None).await;
    let text = ["alpha beta", "gamma delta", "eps zeta", "eta"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "2D").await,
        vec!["alpha ", "eps zeta", "eta"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('1')").await,
        Value::from("beta\ngamma delta")
    );
    assert_eq!(edit_probe(&rpc, &text, (1, 6), "9D").await, vec!["alpha "]);
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "2CX<Esc>").await,
        vec!["alpha X", "eps zeta", "eta"]
    );
    // From the indent, a `D` over several lines takes them whole (vim's
    // `op_delete` rule for `d$`), leaving no empty line behind.
    assert_eq!(
        edit_probe(&rpc, &text, (2, 0), "2D").await,
        vec!["alpha beta", "eta"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.gettype('1')").await,
        Value::from("V")
    );
    assert_eq!(
        edit_probe(&rpc, &text, (3, 0), "9D").await,
        vec!["alpha beta", "gamma delta"]
    );
    // A single line, or `C`, stays charwise.
    assert_eq!(
        edit_probe(&rpc, &text, (2, 0), "D").await,
        vec!["alpha beta", "", "eps zeta", "eta"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (2, 0), "2CX<Esc>").await,
        vec!["alpha beta", "X", "eta"]
    );
}

#[tokio::test]
async fn y_yanks_to_the_end_of_the_line() {
    let (rpc, _incoming) = start(None).await;
    let text = ["alpha beta", "gamma delta"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "Yp").await,
        vec!["alpha bbetaeta", "gamma delta"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "2Y").await,
        vec!["alpha beta", "gamma delta"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('0')").await,
        Value::from("beta\ngamma delta")
    );
}

#[tokio::test]
async fn an_operator_over_w_stops_at_the_end_of_the_line() {
    let (rpc, _incoming) = start(None).await;
    let text = ["alpha beta", "gamma delta", "  eps zeta", "eta"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "dw").await,
        vec!["alpha ", "gamma delta", "  eps zeta", "eta"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('-')").await,
        Value::from("beta")
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "d3w").await,
        vec!["alpha ", "  eps zeta", "eta"]
    );
    edit_probe(&rpc, &text, (1, 6), "yw").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('0')").await,
        Value::from("beta")
    );
    // Starting in the indent, a multi-line `w` delete turns linewise.
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "d4w").await,
        vec!["  eps zeta", "eta"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.gettype('1')").await,
        Value::from("V")
    );
}

#[tokio::test]
async fn ctrl_w_and_ctrl_u_stop_once_at_the_start_of_the_insert() {
    let (rpc, _incoming) = start(None).await;
    let text = ["  alpha beta", "gamma"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "Afoo bar<C-u>Q<Esc>").await,
        vec!["  alpha betaQ", "gamma"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "Afoo bar<C-u><C-u>Q<Esc>").await,
        vec!["Q", "gamma"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "A x<C-w>y<Esc>").await,
        vec!["  alpha beta y", "gamma"]
    );
}

#[tokio::test]
async fn a_counted_o_opens_that_many_lines() {
    let (rpc, _incoming) = start(None).await;
    let text = ["  alpha beta", "gamma"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "3oab<Esc>").await,
        vec!["  alpha beta", "ab", "ab", "ab", "gamma"]
    );
    assert_eq!(cursor(&rpc).await, (4, 1));
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "2Oab<Esc>").await,
        vec!["ab", "ab", "  alpha beta", "gamma"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "3o<Esc>").await,
        vec!["  alpha beta", "", "", "", "gamma"]
    );
}

#[tokio::test]
async fn a_cursor_key_cancels_the_insert_count() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["  alpha beta"], (1, 4), "3ia<Left>b<Esc>").await,
        vec!["  albapha beta"]
    );
}

#[tokio::test]
async fn enter_without_autoindent_keeps_the_split_texts_leading_blanks() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["foo   bar"], (1, 3), "i<CR>x<Esc>").await,
        vec!["foo", "x   bar"]
    );
    // Replace mode's `<BS>` over the break then restores the line exactly.
    assert_eq!(
        edit_probe(
            &rpc,
            &["héllo wörld"],
            (1, 3),
            "Rxyz<CR>ab<BS><BS><BS><BS>q<Esc>"
        )
        .await,
        vec!["héxyq wörld"]
    );
}

/// Blockwise visual is not implemented: `<C-v>` from Visual (and an unmapped one
/// from Normal) must say so rather than silently do nothing.
#[tokio::test]
async fn blockwise_visual_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione two<Esc>0");
    let msg = message_after(&rpc, &mut incoming, "v<C-v>").await;
    assert!(
        msg.contains("blockwise visual (<C-v>) is not implemented"),
        "message: {msg:?}"
    );
    // The charwise selection it could not turn blockwise is still up.
    assert_eq!(mode(&rpc).await, "v");
    feed(&rpc, "<Esc>");
    exec_lua(&rpc, "btv.keymap.del('n', '<C-v>')").await;
    let msg = message_after(&rpc, &mut incoming, "<C-v>").await;
    assert!(
        msg.contains("blockwise visual (<C-v>) is not implemented"),
        "message: {msg:?}"
    );
}

/// Forcing a motion's type (`dvj`, `dVw`) is not implemented: the operator must
/// fail loud instead of being dropped while the motion runs as a bare move.
#[tokio::test]
async fn forced_motion_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione two<CR>three<Esc>gg0");
    let msg = message_after(&rpc, &mut incoming, "dv").await;
    assert!(
        msg.contains("o_v / o_V / o_CTRL-V) is not implemented"),
        "message: {msg:?}"
    );
    let msg = message_after(&rpc, &mut incoming, "dV").await;
    assert!(msg.contains("is not implemented"), "message: {msg:?}");
    assert_eq!(lines(&rpc).await, vec!["one two", "three"]);
}

/// The sentence, paragraph and bracket motions — `(` `)` `{` `}` `%` — move the
/// cursor where vim's do (expected values from `nvim --headless --clean`;
/// positions are 1-based line, 0-based column).
#[tokio::test]
async fn sentence_paragraph_and_bracket_motions_move_like_vim() {
    let (rpc, _incoming) = start(None).await;
    let text = [
        "  Hello there. This is (a test!) of it.",
        "Second line? yes",
        "",
        "",
        "Para (two",
        "spans) lines.  End.",
        "last",
    ];
    for (at, keys, want) in [
        ((1, 0), ")", (1, 15)),
        ((1, 15), ")", (1, 33)),
        ((1, 33), ")", (2, 0)),
        ((1, 33), "2)", (2, 13)),
        ((2, 0), "(", (1, 33)),
        ((2, 0), "2(", (1, 15)),
        ((6, 15), "(", (5, 0)),
        ((1, 3), "}", (3, 0)),
        ((1, 3), "2}", (7, 3)),
        ((5, 3), "}", (7, 3)),
        ((6, 7), "{", (4, 0)),
        ((1, 14), "%", (1, 31)),
        ((5, 3), "%", (6, 5)),
        ((6, 2), "%", (5, 5)),
    ] {
        edit_probe(&rpc, &text, at, keys).await;
        assert_eq!(cursor(&rpc).await, want, "{keys:?} from {at:?}");
    }
    // A `(` count the buffer's start cannot satisfy fails without moving, and an
    // operator over it does nothing (vim's `findsent` "didn't actually move").
    let top = ["", "  Alpha beta. Gamma!", "delta."];
    edit_probe(&rpc, &top, (2, 2), "3(").await;
    assert_eq!(cursor(&rpc).await, (2, 2));
    assert_eq!(edit_probe(&rpc, &top, (2, 2), "d3(").await, top.to_vec());
    edit_probe(&rpc, &top, (2, 2), "2(").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
}
