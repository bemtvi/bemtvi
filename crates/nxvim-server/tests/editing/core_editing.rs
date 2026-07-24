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
        "nx_command",
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
    rpc.request("nx_command", vec![Value::from("w")])
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
    rpc.request("nx_command", vec![Value::from("w!")])
        .await
        .expect("forced write");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\nmine\n",
        "`:w!` must overwrite despite the external change"
    );

    feed(&rpc, "obonus<Esc>");
    rpc.request("nx_command", vec![Value::from("w")])
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
// what nxvim last read or wrote. The four outcomes mirror neovim exactly:
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
    let chunk = format!("lua vim.cmd(\"edit {}\")", path.to_string_lossy());
    rpc.request("nx_command", vec![Value::from(chunk.as_str())])
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
async fn helptags_points_at_the_nxvim_help_plugin() {
    // The help system ships as the optional nxvim-help plugin; tag generation is its
    // `:NxHelptags`. With the plugin absent, `:helptags` must not fall through to the
    // unknown-command path (which can abort a plugin manager mid-install): it points
    // the user at the plugin, and leaves the buffer untouched.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let msg = message(&redraw_after(&rpc, &mut incoming, ":helptags ALL<CR>").await);
    assert!(
        msg.contains("nxvim-help") && msg.contains("NxHelptags"),
        "expected a message pointing at the plugin + :NxHelptags, got {msg:?}"
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
    // `:help` is provided by the optional nxvim-help plugin. With it absent, the
    // command points the user at the plugin instead of the bare unknown-command
    // error, and does not disturb the buffer.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let msg = message(&redraw_after(&rpc, &mut incoming, ":help motion<CR>").await);
    assert!(
        msg.contains("nxvim-help"),
        "expected a message pointing at the nxvim-help plugin, got {msg:?}"
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
