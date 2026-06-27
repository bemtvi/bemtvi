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
use nxvim_test_harness::{cursor, exec_lua, feed, start_attached, temp_dir, write_temp};
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
        exec_lua(&rpc, "nx.shada.save_layout(true)").await; // relative_splits defaults true
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

#[tokio::test]
async fn relative_docks_native_option_scales_the_dock() {
    // `relative_docks` is a NATIVE editor option (`nx.o.relative_docks`), not a plugin
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
        exec_lua(&rpc, "nx.o.relative_docks = true").await;
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
