use crate::support::*;

#[tokio::test]
async fn inserting_text_appears_in_the_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn unreadable_startup_file_keeps_its_name_and_echoes_the_error() {
    // A file whose bytes aren't valid UTF-8 can't be read as text, so
    // `Buffer::from_file` fails. The buffer must still be bound to the path — not
    // fall through to an unnamed scratch buffer that a later `:w` would clobber a
    // stray file from — and the failure must be surfaced on the message line. (R4
    // in the 2026-06-02 review.) (A *directory* is no longer an error: it opens
    // the file explorer — see the `explorer` suite.)
    let path = temp_path("openfail").to_string_lossy().into_owned();
    std::fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write invalid-utf8 file");
    let (rpc, mut incoming) = start(Some(path.clone())).await;

    // The buffer is named after the file the user asked for, not `[No Name]`.
    let name = rpc
        .request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(name, path, "unreadable startup file must keep its name");

    // And the error is echoed, naming the file, rather than silently swallowed.
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        msg.contains(&path),
        "startup error should name the file, got {msg:?}"
    );
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
        "nvim_command",
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

/// The shipped `examples/registers/` config sources cleanly and its Lua
/// register surface actually drives core: the seeded `"h` / `"t` registers
/// paste, and the `:Stash` user command round-trips a line through `setreg` →
/// `:put`. Proves the example isn't just "loads" but works end-to-end.
#[tokio::test]
async fn registers_example_config_runs() {
    let dir = temp_dir("registers-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/registers/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    feed(&rpc, "ialpha<Esc>");
    // The seeded linewise list register `"t` pastes as its own two lines.
    feed(&rpc, ":put t<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "- buy milk", "- water plants"]
    );

    // `:Stash` writes the current line into `"s` via setreg; `:Stashed` reads it
    // back with getreg and puts it below — a full Lua round-trip through core.
    feed(&rpc, "gg:Stash<CR>");
    feed(&rpc, ":Stashed<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "alpha", "- buy milk", "- water plants"]
    );
}

#[tokio::test]
async fn ex_write_persists_changes_to_disk() {
    let path = temp_path("write");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Jump to the last line, open a new one, type, leave insert, then save.
    feed(&rpc, "Gothree<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
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
    // An in-buffer edit, so there's something we'd be saving.
    feed(&rpc, "Gomine<Esc>");

    // Someone else rewrites the file on disk behind our back.
    std::fs::write(&path, "EXTERNALLY CHANGED\n").unwrap();

    // `:w` must refuse rather than silently clobber their changes...
    let msg = message(&redraw_after(&rpc, &mut incoming, ":w<CR>").await);
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
    rpc.request("nvim_command", vec![Value::from("w!")])
        .await
        .expect("forced write");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\nmine\n",
        "`:w!` must overwrite despite the external change"
    );

    feed(&rpc, "obonus<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
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

    // Our own unsaved edit...
    feed(&rpc, "Gomine<Esc>");
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

    feed(&rpc, ":set noautoread<CR>");
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
async fn lua_vim_cmd_drives_the_editor() {
    // A Lua chunk that opens a file should change what the buffer shows.
    let path = temp_path("lua");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let (rpc, _incoming) = start(None).await;
    let chunk = format!("lua vim.cmd(\"edit {}\")", path.to_string_lossy());
    rpc.request("nvim_command", vec![Value::from(chunk.as_str())])
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
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"foo", "bar"})"#,
    )
    .await;
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
    // records. The history panel must hold the echomsg line, not the echo one.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":echo \"transient-line\"<CR>");
    feed(&rpc, ":echomsg \"kept-line\"<CR>");
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    let history = panel_lines(&map);
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
