//! Per-workspace **session** save/restore — the open files, the split layout, and the
//! cursor survive a server restart when the launch is session-scoped
//! (`workspace_session = true`, i.e. a namespaced workspace shada).
//!
//! Black-box, like `shada.rs`: spawn a server against a temp store with the session
//! flag on, open files in a split, quit (the exit flush captures the session), then
//! respawn against the same store and assert the layout came back. A second pair of
//! tests proves the gate: with the flag OFF nothing is restored.

use std::path::Path;

use nxvim_server::{RedbFileStore, ServerInit};
use nxvim_test_harness::{cursor, exec_lua, feed, lines, start_attached, temp_dir, write_temp};
use tokio::sync::mpsc::UnboundedReceiver;

/// A server persisting into `dir`. `session` turns on BOTH capture and restore (the
/// `--shada-namespace` + `--restore-session` combination the wrapper uses).
fn init(dir: &Path, file: Option<String>, session: bool) -> ServerInit {
    ServerInit {
        file,
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: session,
        restore_session: session,
        ..Default::default()
    }
}

async fn await_server_exit(mut incoming: UnboundedReceiver<nxvim_rpc::Incoming>) {
    while incoming.recv().await.is_some() {}
}

/// "name1|name2|…" of every window's buffer, sorted — a stable layout fingerprint.
async fn window_buffer_names(rpc: &nxvim_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"
        local names = {}
        for _, w in ipairs(nx.win.list()) do
          names[#names + 1] = nx.buf.name(nx.win.buf(w))
        end
        table.sort(names)
        return table.concat(names, "|")
        "#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

async fn window_count(rpc: &nxvim_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return #nx.win.list()")
        .await
        .as_i64()
        .unwrap_or(-1)
}

async fn tab_count(rpc: &nxvim_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return #nx.tabpage.list()")
        .await
        .as_i64()
        .unwrap_or(-1)
}

/// Run `set_code` (a `setqflist`/`setloclist` call, whose server-side op drains after
/// the chunk), then evaluate `read_code` in a *second* chunk so the op has drained.
async fn set_then_read(rpc: &nxvim_rpc::Rpc, set_code: &str, read_code: &str) -> String {
    exec_lua(rpc, set_code).await;
    exec_lua(rpc, read_code)
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn session_restores_split_layout_files_and_cursor() {
    let dir = temp_dir("session_store");
    let file_a = write_temp("session_a", "txt", "a1\na2\na3\n");
    let file_b = write_temp("session_b", "txt", "b1\nb2\nb3\nb4\n");
    let file_c = write_temp("session_c", "txt", "c1\nc2\nc3\n");

    // Session 1: a nested layout — A | (B over C) — with the cursor in C. Opt into
    // capture via `nx._session_save_layout`, then quit so the exit flush saves it.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>")); // A | B  (focus B)
        feed(&rpc, &format!(":split {file_c}<CR>")); // B over C in the right column (focus C)
        feed(&rpc, "2G");
        assert_eq!(window_count(&rpc).await, 3, "three windows before quit");
        assert_eq!(cursor(&rpc).await, (2, 0));
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: empty startup buffer; the restore reopens the exact 3-window layout.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            window_count(&rpc).await,
            3,
            "the nested split layout came back"
        );
        let names = window_buffer_names(&rpc).await;
        for f in [&file_a, &file_b, &file_c] {
            assert!(names.contains(f), "window for {f} restored: {names}");
        }
        // EXACT nesting (not a flat row): A is a full-height left column while B/C are
        // stacked in the right column, so A's window is taller than C's.
        let a_taller = exec_lua(
            &rpc,
            &format!(
                "local h = {{}}\n\
                 for _, w in ipairs(nx.win.list()) do h[nx.buf.name(nx.win.buf(w))] = nx.win.height(w) end\n\
                 return (h[{a:?}] or 0) > (h[{c:?}] or 0)",
                a = file_a,
                c = file_c
            ),
        )
        .await;
        assert_eq!(
            a_taller,
            rmpv::Value::Boolean(true),
            "A spans full height; B/C are stacked — nesting restored exactly"
        );
        // The focused window is the one left active (C), at the saved cursor line.
        assert_eq!(
            cursor(&rpc).await,
            (2, 0),
            "cursor restored in the active window"
        );
    }
}

#[tokio::test]
async fn session_restores_multiple_tab_pages() {
    // Two tab pages, each on its own file. The INACTIVE tab's layout is stashed off
    // `self.windows`, and the `self.window_*` accessors only see the current layer's
    // tree — so a naive capture resolves an inactive tab's window ids to nothing and
    // drops the whole tab. The capture reads each tab's own tree, so both come back.
    let dir = temp_dir("session_tabs_store");
    let file_a = write_temp("session_tab_a", "txt", "a1\na2\na3\n");
    let file_b = write_temp("session_tab_b", "txt", "b1\nb2\nb3\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        feed(&rpc, &format!(":tabnew {file_b}<CR>")); // tab 2, on file_b (focused)
        feed(&rpc, "3G");
        assert_eq!(tab_count(&rpc).await, 2, "two tabs before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(tab_count(&rpc).await, 2, "both tab pages came back");
        // The saved active tab (tab 2, on file_b) is refocused at its cursor line.
        let name = exec_lua(&rpc, "return nx.buf.name(nx.win.buf(nx.win.current()))")
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(name.contains(&file_b), "tab 2 (file_b) is focused: {name}");
        assert_eq!(cursor(&rpc).await, (3, 0), "its cursor line restored");
    }
}

#[tokio::test]
async fn session_does_not_persist_the_quickfix_list() {
    // The quickfix stack is *not* persisted: a `:make`/`:grep` result is build/search
    // state, scoped to the editing session, so a restart comes back with an empty list
    // (only the window layout rides the session).
    let dir = temp_dir("session_qf_store");
    let file_a = write_temp("session_qf_a", "txt", "a1\na2\na3\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        // Set then read back so the queued server-side op has drained into `self.qf`
        // before the exit flush runs.
        let got = set_then_read(
            &rpc,
            &format!(
                r#"vim.fn.setqflist({{}}, " ", {{ title = "build", items = {{
                     {{ filename = {file_a:?}, lnum = 2, col = 1, text = "boom" }},
                   }} }})"#,
            ),
            r#"local q = vim.fn.getqflist({ items = true, title = true })
               return string.format("%d|%s|%s", #q.items, q.title, q.items[1].text)"#,
        )
        .await;
        assert_eq!(got, "1|build|boom", "quickfix list populated before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let count = exec_lua(
            &rpc,
            r#"local q = vim.fn.getqflist({ items = true })
               return #q.items"#,
        )
        .await
        .as_i64()
        .unwrap_or(-1);
        assert_eq!(count, 0, "the quickfix list did not survive the restart");
    }
}

#[tokio::test]
async fn session_does_not_persist_a_window_location_list() {
    // A window's location list is build/search state too — it is not persisted, so the
    // reopened window comes back with an empty location list.
    let dir = temp_dir("session_loc_store");
    let file_a = write_temp("session_loc_a", "txt", "a1\na2\na3\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        let got = set_then_read(
            &rpc,
            &format!(
                r#"vim.fn.setloclist(0, {{}}, " ", {{ title = "refs", items = {{
                     {{ filename = {file_a:?}, lnum = 3, col = 1, text = "ref" }},
                   }} }})"#,
            ),
            r#"local q = vim.fn.getloclist(0, { items = true, title = true })
               return string.format("%d|%s|%s", #q.items, q.title, q.items[1].text)"#,
        )
        .await;
        assert_eq!(got, "1|refs|ref", "location list populated before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let count = exec_lua(
            &rpc,
            r#"local q = vim.fn.getloclist(0, { items = true })
               return #q.items"#,
        )
        .await
        .as_i64()
        .unwrap_or(-1);
        assert_eq!(count, 0, "the location list did not survive the restart");
    }
}

/// The name of the current window's buffer (`""` for an unnamed `[No Name]`).
async fn current_buffer_name(rpc: &nxvim_rpc::Rpc) -> String {
    exec_lua(rpc, "return nx.buf.name(nx.win.buf(nx.win.current()))")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn session_restores_a_modified_unnamed_buffer() {
    // A modified `[No Name]` buffer (typed into but never written to a file) rides the
    // session with its contents — gated on `'workspacepersistunnamed'` (default on).
    let dir = temp_dir("session_unnamed_store");

    {
        // Start with no file: the startup `[No Name]` buffer. Type into it (now modified).
        let (rpc, incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        feed(&rpc, "iscratch notes<CR>second line<Esc>");
        assert_eq!(current_buffer_name(&rpc).await, "", "buffer is unnamed");
        assert_eq!(lines(&rpc).await, vec!["scratch notes", "second line"]);
        feed(&rpc, ":qa!<CR>"); // modified → bang; the exit flush still captures
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            current_buffer_name(&rpc).await,
            "",
            "restored buffer is still unnamed"
        );
        assert_eq!(
            lines(&rpc).await,
            vec!["scratch notes", "second line"],
            "the unnamed buffer's contents came back"
        );
        // It's marked modified, so it round-trips: a second exit re-captures it.
        let modified = exec_lua(&rpc, "return vim.bo.modified").await;
        assert_eq!(
            modified,
            rmpv::Value::Boolean(true),
            "restored unnamed buffer is modified (unsaved content)"
        );
    }
}

#[tokio::test]
async fn workspacepersistunnamed_off_drops_the_unnamed_buffer() {
    // With the option off, a modified `[No Name]` buffer is NOT persisted — the inverse
    // of the test above, proving the gate.
    let dir = temp_dir("session_unnamed_off_store");

    {
        let (rpc, incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        exec_lua(&rpc, "nx.o.workspacepersistunnamed = false").await;
        feed(&rpc, "ithrowaway<Esc>");
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            lines(&rpc).await,
            vec![""],
            "the unnamed buffer was not persisted (fresh empty startup buffer)"
        );
    }
}

#[tokio::test]
async fn qa_does_not_block_on_a_persisted_unnamed_buffer() {
    // In a layout-capturing workspace with `'workspacepersistunnamed'` on, a modified
    // `[No Name]` buffer doesn't block a bang-less `:qa` — its content is saved anyway.
    let dir = temp_dir("session_qa_persist_store");

    {
        let (rpc, incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await; // a capturing session
        feed(&rpc, "iscratch survived<Esc>"); // modified, shown `[No Name]`
        feed(&rpc, ":qa<CR>"); // NO bang — must quit despite the modified buffer
                               // The await completing proves the editor quit (E37 would have kept it alive).
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            await_server_exit(incoming),
        )
        .await
        .expect("`:qa` quit without blocking on the persisted unnamed buffer");
    }

    {
        // …and it really was persisted on that bang-less exit.
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            lines(&rpc).await,
            vec!["scratch survived"],
            "the unnamed buffer's contents survived the non-bang `:qa`"
        );
    }
}

#[tokio::test]
async fn qa_still_blocks_a_modified_unnamed_buffer_when_persist_is_off() {
    // The inverse guard: with `'workspacepersistunnamed'` off the buffer is NOT persisted,
    // so a bang-less `:qa` must still block (`E37`) — abandoning it would lose the content.
    let dir = temp_dir("session_qa_block_store");
    let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
    exec_lua(&rpc, "nx.shada.save_layout(true)").await;
    exec_lua(&rpc, "nx.o.workspacepersistunnamed = false").await;
    feed(&rpc, "ithrowaway<Esc>");
    feed(&rpc, ":qa<CR>");
    // The server is still alive (the quit was blocked) — a fresh request still answers.
    assert_eq!(
        exec_lua(&rpc, "return 40 + 2").await.as_i64(),
        Some(42),
        "`:qa` was blocked (E37): the editor is still running",
    );
}

#[tokio::test]
async fn native_session_options_round_trip_through_the_mirror() {
    // The session-control options are squashed-name globals; a set must read back through
    // the server's `nx._go_mirror` push (the mirror key has to match the canonical name,
    // not the snake_case Rust field). Guards the rename + the new option.
    let (rpc, _incoming) =
        start_attached(init(&temp_dir("session_optmirror"), None, true), 80, 25).await;
    let got = exec_lua(
        &rpc,
        r#"
        nx.o.relativesplits = false
        nx.o.relativedocks = true
        nx.o.workspacepersistunnamed = false
        nx.o.workspacecwd = false
        -- a second exec_lua chunk reads after the server pushes the mirror; do it inline
        -- via a barrier read so we exercise the round-trip, not just the write-through.
        return string.format("%s|%s|%s|%s",
          tostring(nx.o.relativesplits), tostring(nx.o.relativedocks),
          tostring(nx.o.workspacepersistunnamed), tostring(nx.o.workspacecwd))
        "#,
    )
    .await;
    // Write-through within the chunk; the cross-tick mirror is checked below.
    assert_eq!(got.as_str(), Some("false|true|false|false"));

    let after = exec_lua(
        &rpc,
        r#"return string.format("%s|%s|%s|%s",
             tostring(nx.o.relativesplits), tostring(nx.o.relativedocks),
             tostring(nx.o.workspacepersistunnamed), tostring(nx.o.workspacecwd))"#,
    )
    .await;
    assert_eq!(
        after.as_str(),
        Some("false|true|false|false"),
        "the values survive the server's mirror refresh (rename keys match)"
    );
}

#[tokio::test]
async fn capture_requires_the_plugin_opt_in() {
    // The namespace + restore are on, but the plugin never called
    // `nx.shada.save_layout(true)` — so capture is off by default and nothing is saved.
    let dir = temp_dir("session_optin_store");
    let file_a = write_temp("session_optin_a", "txt", "a1\na2\n");
    let file_b = write_temp("session_optin_b", "txt", "b1\nb2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        assert_eq!(window_count(&rpc).await, 2);
        feed(&rpc, ":qa<CR>"); // exit flush, but no capture opt-in → no session written
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            window_count(&rpc).await,
            1,
            "no layout captured without nx.shada.save_layout(true)"
        );
    }
}

#[tokio::test]
async fn workspace_flag_captures_without_a_plugin_opt_in() {
    // The `--workspace` flag seeds `session_save_layout` ON natively (no plugin / config
    // call), so a directory session captures and restores its layout out of the box. This
    // is the inverse of `capture_requires_the_plugin_opt_in`: identical flow, but the
    // layout DOES come back because the binary opted in for us.
    let dir = temp_dir("session_ws_store");
    let file_a = write_temp("session_ws_a", "txt", "a1\na2\n");
    let file_b = write_temp("session_ws_b", "txt", "b1\nb2\n");

    // The init a `--workspace` launch builds: capture + restore + the native opt-in, and
    // NEVER calling `nx.shada.save_layout`.
    let ws_init = |file: Option<String>| ServerInit {
        file,
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: true,
        restore_session: true,
        session_save_layout: true,
        ..Default::default()
    };

    {
        let (rpc, incoming) = start_attached(ws_init(Some(file_a.clone())), 80, 25).await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        assert_eq!(window_count(&rpc).await, 2);
        // Confirm the native seed reached the runtime — no plugin call was made.
        assert_eq!(
            exec_lua(&rpc, "return nx.shada.namespace() == nil")
                .await
                .as_bool(),
            Some(true),
            "this bare init has no namespace env, yet capture is on via session_save_layout",
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(ws_init(None), 80, 25).await;
        assert_eq!(
            window_count(&rpc).await,
            2,
            "the --workspace session captured + restored the split with no opt-in call",
        );
    }
}

#[tokio::test]
async fn relative_split_sizes_scale_to_the_terminal_width() {
    let dir = temp_dir("session_relsize_store");
    let file_a = write_temp("session_rel_a", "txt", "a1\na2\n");
    let file_b = write_temp("session_rel_b", "txt", "b1\nb2\n");

    // Session 1 at width 80: a 50/50 vertical split (each window ~39 cols).
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await; // relativesplits defaults true
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2 at a WIDER terminal (120): the split is restored proportionally, so the
    // focused window is ~half of 120 (~59) — not the old ~39 it would be if sizes were
    // absolute cells.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 120, 25).await;
        let w = exec_lua(&rpc, "return nx.win.width(nx.win.current())")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert!(w > 50, "the split scaled to the wider terminal (got {w})");
    }
}

// The main window's width (the dock surface has no `is_open` query, but a left dock
// reserves columns, so an open dock shrinks the main window — an observable signal).
async fn main_win_width(rpc: &nxvim_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return nx.win.width(nx.win.current())")
        .await
        .as_i64()
        .unwrap_or(-1)
}

#[tokio::test]
async fn session_restores_open_docks() {
    let dir = temp_dir("session_dock_store");
    let file_a = write_temp("session_dock_a", "txt", "a1\na2\n");

    // Session 1: a file open + a left dock at size 22. Capture, then quit.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 22 }").await;
        feed(&rpc, "<Esc>"); // settle a tick so the dock op drains + relayout runs
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the left dock comes back — focus is the (refocused) main window, and a
    // 22-col left dock reserves it down to 80 - 22 - 1(separator) = 57.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let w = main_win_width(&rpc).await;
        assert!(
            w < 70,
            "the restored left dock shrinks the main window (got {w})"
        );
    }
}

// The widths of the two main split windows (showing `session_ds_a` / `_b`), as
// "<a>,<b>". Used to compare the live split against the restored one.
async fn split_widths(rpc: &nxvim_rpc::Rpc) -> (i64, i64) {
    let s = exec_lua(
        rpc,
        "local w = { a = -1, b = -1 }\n\
         for _, win in ipairs(nx.win.list()) do\n\
           local name = nx.buf.name(nx.win.buf(win))\n\
           if name:match('session_ds_a') then w.a = nx.win.width(win) end\n\
           if name:match('session_ds_b') then w.b = nx.win.width(win) end\n\
         end\n\
         return string.format('%d,%d', w.a, w.b)",
    )
    .await
    .as_str()
    .unwrap_or("")
    .to_string();
    let (a, b) = s.split_once(',').expect("two widths");
    (a.parse().unwrap(), b.parse().unwrap())
}

#[tokio::test]
async fn session_restores_split_sizes_under_a_dock_without_drift() {
    // A split restored alongside a dock must be laid out against the SAME
    // (dock-reduced) main area it had at save time, so it comes back at exactly its
    // saved widths. The bug: tabs were rebuilt FIRST — laying every split out at
    // FULL width — and the dock was restored afterward, so the split was rescaled a
    // second time once the dock shrank the main area, and that extra rescale drifts
    // it off its saved widths. Restoring the docks first means the split is laid out
    // once, at its real width. `relativesplits = false` keeps the saved sizes as
    // exact cells (not percentages), and both sessions run at the same width, so a
    // faithful restore reproduces the widths to the cell.
    let dir = temp_dir("session_dock_split_store");
    let file_a = write_temp("session_ds_a", "txt", "a1\na2\n");
    let file_b = write_temp("session_ds_b", "txt", "b1\nb2\n");

    // Session 1 at width 80: a vertical split of two files in the main area plus a
    // 22-col left dock. Record the live split widths, then capture + quit.
    let (live_a, live_b) = {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        exec_lua(&rpc, "nx.o.relativesplits = false").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>")); // A | B in the main area
        exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 22 }").await;
        feed(&rpc, "<Esc>"); // settle a tick so the dock op drains + relayout runs
        let live = split_widths(&rpc).await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
        live
    };
    assert!(live_a > 0 && live_b > 0, "split exists in session 1");

    // Session 2 at the same width 80: the dock + split come back at the exact saved
    // widths. With the docks restored after the tabs they drifted by a cell.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let (rest_a, rest_b) = split_widths(&rpc).await;
        assert_eq!(
            (rest_a, rest_b),
            (live_a, live_b),
            "restored split matches the saved widths exactly \
             (saved {live_a},{live_b}; restored {rest_a},{rest_b})"
        );
    }
}

#[tokio::test]
async fn relativedocks_native_option_scales_the_dock() {
    // `relativedocks` is a NATIVE editor option (`nx.o.relativedocks`), not a plugin
    // setting: any wrapper that opts a session into capture honors it. With it on, a
    // dock's size is stored as a % of the screen and re-derived against the screen the
    // restore runs at (the editor's default 80 cols, before the UI attaches) — so a dock
    // captured wide comes back proportionally smaller, not at its old cell count.
    let dir = temp_dir("session_reldock_store");
    let file_a = write_temp("session_reldock_a", "txt", "a1\na2\n");

    // Session 1 at width 160: opt in, flip the native option on via `nx.o`, open an
    // 80-col left dock (= 50% of 160). Quit so the exit flush captures it as 50%.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 160, 25).await;
        exec_lua(&rpc, "nx.shada.save_layout(true)").await;
        exec_lua(&rpc, "nx.o.relativedocks = true").await;
        exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 80 }").await;
        feed(&rpc, "<Esc>"); // settle a tick so the dock op drains + relayout runs
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2 at width 160: restore re-derives 50% of the editor's 80-col default = 40
    // cells, so the main window is 160 - 40 - 1 = ~119. Absolute cells would restore the
    // captured 80, leaving ~79; a main width over 100 proves the dock scaled with the %.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 160, 25).await;
        let w = main_win_width(&rpc).await;
        assert!(
            w > 100,
            "the restored dock used its captured % (main width {w}; absolute cells would be ~79)"
        );
    }
}
