//! Per-workspace **session** save/restore — the open files, the split layout, and the
//! cursor survive a server restart when the launch is session-scoped
//! (`workspace_session = true`, i.e. a namespaced workspace shada).
//!
//! Black-box, like `shada.rs`: spawn a server against a temp store with the session
//! flag on, open files in a split, quit (the exit flush captures the session), then
//! respawn against the same store and assert the layout came back. A second pair of
//! tests proves the gate: with the flag OFF nothing is restored.

use std::path::Path;

use bemtvi_server::{RedbFileStore, ServerInit};
use bemtvi_test_harness::{
    await_server_exit, cursor, exec_lua, feed, lines, poll_true, q, start_attached, temp_dir,
    write_temp,
};

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

/// "name1|name2|…" of every window's buffer, sorted — a stable layout fingerprint.
async fn window_buffer_names(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"
        local names = {}
        for _, w in ipairs(btv.win.list()) do
          names[#names + 1] = btv.buf.name(btv.win.buf(w))
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

async fn window_count(rpc: &bemtvi_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return #btv.win.list()")
        .await
        .as_i64()
        .unwrap_or(-1)
}

async fn tab_count(rpc: &bemtvi_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return #btv.tabpage.list()")
        .await
        .as_i64()
        .unwrap_or(-1)
}

/// Run `set_code` (a `setqflist`/`setloclist` call, whose server-side op drains after
/// the chunk), then evaluate `read_code` in a *second* chunk so the op has drained.
async fn set_then_read(rpc: &bemtvi_rpc::Rpc, set_code: &str, read_code: &str) -> String {
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
    // capture via `btv._session_save_layout`, then quit so the exit flush saves it.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
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
                 for _, w in ipairs(btv.win.list()) do h[btv.buf.name(btv.win.buf(w))] = btv.win.height(w) end\n\
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

/// The "|"-joined, sorted names of every listed buffer (`nvim_list_bufs`), for asserting
/// which buffers survive a restart regardless of whether they are shown in a window.
async fn buffer_names(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"
        local out = {}
        for _, b in ipairs(vim.api.nvim_list_bufs()) do
          out[#out + 1] = vim.api.nvim_buf_get_name(b)
        end
        table.sort(out)
        return table.concat(out, "|")
        "#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

#[tokio::test]
async fn session_restores_hidden_buffers() {
    // A workspace session must restore buffers that are LOADED but not shown in any window
    // (you `:edit` a second file, leaving the first hidden in the buffer list). The capture
    // walked only the window layout, so hidden buffers were dropped on restart — they must
    // come back in the buffer list (windowless), reachable via `:bnext` / `:ls`.
    let dir = temp_dir("session_hidden_store");
    let file_a = write_temp("session_hidden_a", "txt", "a1\na2\na3\n");
    let file_b = write_temp("session_hidden_b", "txt", "b1\nb2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        // Park the cursor on line 3 of file_a, then open file_b in the same window — file_a
        // stays loaded but hidden (the alternate), with its view saved.
        feed(&rpc, "3G");
        feed(&rpc, &format!(":edit {file_b}<CR>"));
        let names = buffer_names(&rpc).await;
        assert!(
            names.contains(&file_a) && names.contains(&file_b),
            "precondition: file_a is hidden-but-listed alongside the shown file_b: {names}"
        );
        assert_eq!(
            window_count(&rpc).await,
            1,
            "only one window (file_b shown)"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        // Only file_b is windowed, but BOTH files are back in the buffer list.
        assert_eq!(window_count(&rpc).await, 1, "the single window came back");
        let names = buffer_names(&rpc).await;
        assert!(
            names.contains(&file_a),
            "the hidden buffer file_a was restored to the buffer list: {names}"
        );
        assert!(
            names.contains(&file_b),
            "the windowed buffer file_b came back too: {names}"
        );
        // Switch to the restored hidden buffer: its real contents are loaded and its saved
        // view (cursor on line 3) came back too.
        feed(&rpc, &format!(":buffer {file_a}<CR>"));
        assert_eq!(
            lines(&rpc).await,
            vec!["a1", "a2", "a3"],
            "hidden buffer's bytes"
        );
        assert_eq!(
            cursor(&rpc).await,
            (3, 0),
            "hidden buffer's saved cursor restored"
        );
    }
}

#[tokio::test]
async fn session_does_not_save_an_open_panel() {
    // Quitting with a panel surface open (`:messages`, `:ls`, `:registers`, …) must NOT save
    // it into the layout: a panel buffer is named (`[Messages]`), so the layout capture used
    // to treat it as a file and restore a split with an empty buffer named for the panel. A
    // panel is not a document (`:ls` hides it) — drop its window leaf so the split collapses.
    let dir = temp_dir("session_panel_store");
    let file_a = write_temp("session_panel_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, ":messages<CR>"); // opens the [Messages] panel as a bottom split
        assert_eq!(
            window_count(&rpc).await,
            2,
            "file window + the messages panel"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        assert_eq!(
            window_count(&rpc).await,
            1,
            "only the file window came back — the panel split was not saved"
        );
        let names = buffer_names(&rpc).await;
        assert!(
            !names.contains("[Messages]"),
            "the panel was not restored as an (empty) buffer: {names}"
        );
        assert!(
            names.contains(&file_a),
            "the real file did come back: {names}"
        );
    }
}

#[tokio::test]
async fn session_drops_ephemeral_views() {
    // A view created WITHOUT `persist` is ephemeral: it does not ride the session, so the
    // restore leaves no pending claim (and its slot collapses, as today).
    let dir = temp_dir("session_view_eph_store");
    let file_a = write_temp("session_view_eph_a", "txt", "a1\na2\na3\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local v = btv.view.create{ name = "TV", filetype = "btvview" }  -- no persist
            v:set_lines({ "x" })
            v:mount{ split = "vsplit" }
            "#,
        )
        .await;
        assert_eq!(window_count(&rpc).await, 2, "file_a | ephemeral view");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let n = exec_lua(&rpc, "return #btv.view.pending_restores()")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert_eq!(n, 0, "an ephemeral view leaves no pending restore");
        assert_eq!(
            window_count(&rpc).await,
            1,
            "the ephemeral view's slot collapsed; only file_a remains"
        );
    }
}

/// The on_restore handler a restoring session registers at boot: it reads the view's saved
/// lines back from its own plugin-shada (keyed by the persist id) and adopts the reserved
/// slot. Stashes the rebuilt handle at `btv._test_restored_view` so the test can read its
/// content regardless of which layer (dock / split) the slot is in. Registered with an
/// explicit namespace because `client_init_lua` attributes to no runtimepath entry.
const RESTORE_HANDLER: &str = r#"
btv.view.on_restore(function(id, place)
  local data = btv.shada.plugin("treens"):get("view:" .. id) or {}
  local v = btv.view.create{
    name = "Tree", filetype = "btvview", persist = id, namespace = "treens",
  }
  v:set_lines(data)
  btv._test_restored_view = v
  place(v)
end, "treens")
"#;

/// Read back the lines of the view the restore handler rebuilt, as "l1|l2|…".
async fn restored_view_content(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"
        local v = btv._test_restored_view
        if not v or not v:bufnr() then return "<none>" end
        return table.concat(btv.buf.lines(v:bufnr(), 0, -1, false), "|")
        "#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

#[tokio::test]
async fn session_restores_a_persisted_view_in_a_dock_through_on_restore() {
    // The full round trip: a plugin mounts a persisted view in a left dock and stashes its
    // content in its own plugin-shada keyed by the view's persist id. After a restart, the
    // restore reserves the dock slot, the plugin's `on_restore` handler rebuilds the view
    // from its shada and adopts the reserved window — content and dock geometry both back.
    let dir = temp_dir("session_view_rt_dock_store");

    {
        let (rpc, incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local lines = { "root", "  a.txt", "  b.txt" }
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines(lines)
            v:mount{ dock = "left", size = 30 }
            btv.shada.plugin("treens"):set("view:main", lines)
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await; // quit from main, so the width probe below
                                                  // measures the main window (not the dock)
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(RESTORE_HANDLER.to_string());
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        let pending = exec_lua(&rpc, "return #btv.view.pending_restores()")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert_eq!(pending, 0, "the persisted view's slot was adopted");
        assert_eq!(
            restored_view_content(&rpc).await,
            "root|  a.txt|  b.txt",
            "the plugin rebuilt the view's content from its own shada"
        );
        // The left dock came back (it shrinks the main window below full width).
        let w = main_win_width(&rpc).await;
        assert!(
            w < 70,
            "the restored view's left dock shrinks the main (got {w})"
        );
    }
}

#[tokio::test]
async fn session_restores_a_persisted_view_in_a_split_through_on_restore() {
    // The Layer::Main adoption path: a persisted view mounted in a main-area split comes
    // back in a real main window (visible to `btv.win.list()`), adopted from the reserved
    // slot alongside the restored file window.
    let dir = temp_dir("session_view_rt_split_store");
    let file_a = write_temp("session_view_rt_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local lines = { "alpha", "beta" }
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines(lines)
            v:mount{ split = "vsplit" }
            btv.shada.plugin("treens"):set("view:main", lines)
            "#,
        )
        .await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let mut si = init(&dir, Some(file_a.clone()), true);
        si.client_init_lua = Some(RESTORE_HANDLER.to_string());
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        assert_eq!(
            window_count(&rpc).await,
            2,
            "file_a window + the adopted view window"
        );
        assert_eq!(
            restored_view_content(&rpc).await,
            "alpha|beta",
            "the view's content was rebuilt into the adopted main-area window"
        );
        let names = window_buffer_names(&rpc).await;
        assert!(
            names.contains(&file_a),
            "file_a restored alongside: {names}"
        );
    }
}

#[tokio::test]
async fn session_collapses_a_persisted_view_with_no_handler() {
    // When the owning plugin is gone (no `on_restore` registered), the reserved slot is an
    // orphan: it collapses, exactly like a restored file window whose file vanished. The
    // dock does not come back; only the restored file window remains.
    let dir = temp_dir("session_view_orphan_store");
    let file_a = write_temp("session_view_orphan_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines({ "root" })
            v:mount{ split = "vsplit" }
            "#,
        )
        .await;
        assert_eq!(window_count(&rpc).await, 2, "file_a | view before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        // No client_init_lua: nothing registers `on_restore`, so the slot is unclaimed.
        let (rpc, _incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        let pending = exec_lua(&rpc, "return #btv.view.pending_restores()")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert_eq!(pending, 0, "the unclaimed slot was drained by the collapse");
        assert_eq!(
            window_count(&rpc).await,
            1,
            "the orphan view's slot collapsed; only file_a remains"
        );
    }
}

/// Write a throwaway plugin dir at `<base>` whose `lua/<name>/init.lua` exposes a `setup`
/// that registers an `btv.view.on_restore` handler (rebuilding a persisted view from the
/// plugin's own shada). Declared to `btv.plugins` with `name = <name>`, so the manager keys
/// its namespace on `<name>` — matching the slot the first session recorded. Returns the dir.
fn write_restore_plugin(base: &Path, name: &str) -> std::path::PathBuf {
    let luadir = base.join("lua").join(name);
    std::fs::create_dir_all(&luadir).unwrap();
    std::fs::write(
        luadir.join("init.lua"),
        format!(
            r#"local M = {{}}
function M.setup()
  btv.view.on_restore(function(id, place)
    local data = btv.shada.plugin("{name}"):get("view:" .. id) or {{}}
    local v = btv.view.create{{
      name = "Tree", filetype = "btvview", persist = id, namespace = "{name}",
    }}
    v:set_lines(data)
    btv._test_restored_view = v
    place(v)
  end)
  _G.async_setup_ran = true
end
return M
"#
        ),
    )
    .unwrap();
    base.to_path_buf()
}

#[tokio::test]
async fn session_restores_a_persisted_view_when_the_owning_plugin_loads_async() {
    // The async-load race: a plugin loaded via `btv.plugins({ config = ... })` registers its
    // `btv.view.on_restore` handler on a LATER tick — its `config` runs in a fire-and-forget
    // async load that only completes after the boot restore dispatch. The reserved slot must
    // survive until that late registration claims it, instead of collapsing at boot (the bug:
    // the slot was gone before the plugin ever got a chance to adopt it).
    let dir = temp_dir("session_view_async_plugin_store");
    let mroot = temp_dir("session_view_async_plugin_root");
    let root = mroot.join("install");
    let plugdir = write_restore_plugin(&temp_dir("session_view_async_plugin_dir"), "treens");

    // Session 1: create + mount a persisted view (via the RPC escape hatch, ns "treens") in a
    // left dock, stash its content in the plugin's own shada, quit.
    {
        let (rpc, incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local lines = { "root", "  a.txt", "  b.txt" }
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines(lines)
            v:mount{ dock = "left", size = 30 }
            btv.shada.plugin("treens"):set("view:main", lines)
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await; // quit from main
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the owning plugin is declared to `btv.plugins` and loads EAGERLY but
    // asynchronously — its `config` (registering `on_restore`) runs after boot, so the fix's
    // deferred collapse + pull-on-register are what let it claim the slot.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(format!(
            "btv.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
             btv.plugins {{ {{ name = \"treens\", dir = \"{plug}\",\n\
               config = function() require(\"treens\").setup() end }} }}",
            root = q(&root),
            cfg = q(&mroot.join("config")),
            plug = q(&plugdir),
        ));
        let (rpc, _incoming) = start_attached(si, 80, 25).await;

        // The async load must complete and its `on_restore` claim the reserved slot.
        assert!(
            poll_true(
                &rpc,
                "return _G.async_setup_ran == true and #btv.view.pending_restores() == 0",
            )
            .await,
            "the async plugin loaded and its on_restore claimed the reserved slot"
        );
        assert_eq!(
            restored_view_content(&rpc).await,
            "root|  a.txt|  b.txt",
            "the async plugin rebuilt the view's content from its own shada"
        );
        // The left dock came back (it shrinks the main window below full width).
        let w = main_win_width(&rpc).await;
        assert!(
            w < 70,
            "the restored view's left dock shrinks the main (got {w})"
        );
    }
}

#[tokio::test]
async fn session_restores_a_persisted_view_when_the_owning_plugin_is_lazy() {
    // A LAZY plugin owns the restored sidebar. Its declared triggers (`cmd`/`keys`/`event`)
    // are things the *user* does, and a session restore does none of them — so nothing ever
    // loads the plugin, its `on_restore` is never registered, and the slot the restore
    // reserved for its sidebar collapses: the sidebar you quit with silently doesn't come
    // back. A reserved slot naming the plugin IS the missing trigger, so the restore wakes it.
    let dir = temp_dir("session_view_lazy_plugin_store");
    let mroot = temp_dir("session_view_lazy_plugin_root");
    let root = mroot.join("install");
    let plugdir = write_restore_plugin(&temp_dir("session_view_lazy_plugin_dir"), "lazytree");

    // The same lazy declaration on both runs: it loads only on `:LazyTree`, which no test
    // (and no restore) ever types.
    let decl = format!(
        "btv.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
         btv.plugins {{ {{ name = \"lazytree\", dir = \"{plug}\", cmd = \"LazyTree\",\n\
           config = function() require(\"lazytree\").setup() end }} }}",
        root = q(&root),
        cfg = q(&mroot.join("config")),
        plug = q(&plugdir),
    );

    // Session 1: the plugin's persisted view is mounted in a left dock (via the RPC escape
    // hatch, ns "lazytree" — the plugin itself stays unloaded), its content stashed in the
    // plugin's own shada. Quit.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl.clone());
        let (rpc, incoming) = start_attached(si, 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local lines = { "root", "  a.txt", "  b.txt" }
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "lazytree",
            }
            v:set_lines(lines)
            v:mount{ dock = "left", size = 30 }
            btv.shada.plugin("lazytree"):set("view:main", lines)
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await; // quit from main
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: nothing presses the `cmd` trigger — the reserved slot must wake the plugin.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl);
        let (rpc, _incoming) = start_attached(si, 80, 25).await;

        assert!(
            poll_true(
                &rpc,
                "return _G.async_setup_ran == true and #btv.view.pending_restores() == 0",
            )
            .await,
            "the reserved slot woke the lazy plugin, whose on_restore claimed it"
        );
        assert!(
            exec_lua(&rpc, r#"return btv.plugins.list()"#)
                .await
                .as_array()
                .map(|l| l.iter().any(|p| {
                    let m = p.as_map().unwrap();
                    let f = |k: &str| {
                        m.iter()
                            .find(|(a, _)| a.as_str() == Some(k))
                            .map(|(_, b)| b.clone())
                    };
                    f("name").and_then(|v| v.as_str().map(str::to_string))
                        == Some("lazytree".into())
                        && f("loaded").and_then(|v| v.as_bool()) == Some(true)
                }))
                .unwrap_or(false),
            "the manager reports the lazy plugin as loaded"
        );
        assert_eq!(
            restored_view_content(&rpc).await,
            "root|  a.txt|  b.txt",
            "the woken plugin rebuilt the view's content from its own shada"
        );
        let w = main_win_width(&rpc).await;
        assert!(
            w < 70,
            "the restored view's left dock shrinks the main (got {w})"
        );
    }
}

/// Write a plugin dir whose `setup` mounts a persistent `btv.view.component` in a left dock —
/// the framework path (no hand-written `on_restore`). Loaded via `btv.plugins`, its namespace
/// is the manager `name`, so `ctx.store` and the reserved slot line up across sessions.
fn write_component_plugin(base: &Path, name: &str) -> std::path::PathBuf {
    let luadir = base.join("lua").join(name);
    std::fs::create_dir_all(&luadir).unwrap();
    std::fs::write(
        luadir.join("init.lua"),
        r#"local M = {}
function M.setup()
  local C = btv.view.component({
    setup = function(ctx)
      _G.cplug_buf = ctx.bufnr()
      return ctx.reactive({ lines = ctx.store:get("view:" .. ctx.persist_id) or { "default" } })
    end,
    render = function(s) return { lines = s.lines } end,
  })
  C.mount({ persist = "notes", dock = "left", size = 30 })
  _G.cplug_setup_ran = true
end
return M
"#,
    )
    .unwrap();
    base.to_path_buf()
}

#[tokio::test]
async fn session_restores_a_persisted_component_when_the_owning_plugin_loads_async() {
    // The framework path under an async load: a plugin loaded via `btv.plugins({ config = … })`
    // mounts a persistent `btv.view.component` on a later tick. Its restore router registers
    // (via `on_restore`) after the boot dispatch, so the reserved slot must survive and the
    // router's pull-on-register must adopt it — rebuilding content from `ctx.store`.
    let dir = temp_dir("session_component_async_store");
    let mroot = temp_dir("session_component_async_root");
    let root = mroot.join("install");
    let plugdir = write_component_plugin(&temp_dir("session_component_async_dir"), "notes");

    let decl = format!(
        "btv.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
         btv.plugins {{ {{ name = \"notes\", dir = \"{plug}\",\n\
           config = function() require(\"notes\").setup() end }} }}",
        root = q(&root),
        cfg = q(&mroot.join("config")),
        plug = q(&plugdir),
    );

    // Session 1: the plugin mounts fresh (default), then we seed its store and quit.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl.clone());
        let (rpc, incoming) = start_attached(si, 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        assert!(
            poll_true(&rpc, "return _G.cplug_setup_ran == true").await,
            "the async plugin loaded and mounted its component fresh"
        );
        exec_lua(
            &rpc,
            r#"btv.shada.plugin("notes"):set("view:notes", { "r1", "r2" })"#,
        )
        .await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the plugin loads async again; its component router adopts the reserved slot.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl);
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        assert!(
            poll_true(
                &rpc,
                r#"local b = _G.cplug_buf
                   if not b then return false end
                   return table.concat(btv.buf.lines(b, 0, -1, false), "|") == "r1|r2"
                     and #btv.view.pending_restores() == 0"#,
            )
            .await,
            "the async component adopted the reserved slot and rebuilt from ctx.store"
        );
        assert_eq!(
            window_count(&rpc).await,
            2,
            "main + the restored component dock"
        );
    }
}

#[tokio::test]
async fn session_restores_a_persisted_component_when_the_owning_plugin_is_lazy() {
    // The framework path (`btv.view.component` + `mount{ persist= }`) under a LAZY owner: the
    // reserved slot wakes the plugin, its `setup` mounts the component, and the component's
    // restore router adopts the slot instead of opening a second, fresh sidebar.
    let dir = temp_dir("session_component_lazy_store");
    let mroot = temp_dir("session_component_lazy_root");
    let root = mroot.join("install");
    let plugdir = write_component_plugin(&temp_dir("session_component_lazy_dir"), "notes");

    let decl = format!(
        "btv.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
         btv.plugins {{ {{ name = \"notes\", dir = \"{plug}\", cmd = \"Notes\",\n\
           config = function() require(\"notes\").setup() end }} }}",
        root = q(&root),
        cfg = q(&mroot.join("config")),
        plug = q(&plugdir),
    );

    // Session 1: press the `cmd` trigger once so the component mounts and the layout
    // captures its slot; seed the store, quit.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl.clone());
        let (rpc, incoming) = start_attached(si, 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, ":Notes<CR>");
        assert!(
            poll_true(&rpc, "return _G.cplug_setup_ran == true").await,
            "the cmd trigger loaded the lazy plugin and mounted its component"
        );
        exec_lua(
            &rpc,
            r#"btv.shada.plugin("notes"):set("view:notes", { "r1", "r2" })"#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await; // quit from main
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: nothing types `:Notes` — the reserved slot alone must wake the plugin.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl);
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        assert!(
            poll_true(
                &rpc,
                r#"local b = _G.cplug_buf
                   if not b then return false end
                   return table.concat(btv.buf.lines(b, 0, -1, false), "|") == "r1|r2"
                     and #btv.view.pending_restores() == 0"#,
            )
            .await,
            "the woken lazy plugin's component adopted the slot and rebuilt from ctx.store"
        );
        assert_eq!(
            window_count(&rpc).await,
            2,
            "main + the restored component dock — the slot was adopted, not doubled"
        );
    }
}

#[tokio::test]
async fn session_collapses_the_slot_when_the_woken_lazy_plugin_claims_nothing() {
    // The wake-up must not be able to wedge a restore: a lazy plugin woken by a reserved
    // slot that then registers no `on_restore` (the feature was dropped, or it only mounts
    // on demand) leaves the slot unclaimed — and the orphan collapse, which now waits for
    // that wake-up load, must still fire once the load settles rather than leaving an empty
    // placeholder window behind forever.
    let dir = temp_dir("session_view_lazy_noclaim_store");
    let mroot = temp_dir("session_view_lazy_noclaim_root");
    let root = mroot.join("install");
    let plugdir = temp_dir("session_view_lazy_noclaim_dir");

    let decl = format!(
        "btv.plugins.setup_manager({{ root = \"{root}\", config = \"{cfg}\" }})\n\
         btv.plugins {{ {{ name = \"quiet\", dir = \"{plug}\", cmd = \"Quiet\",\n\
           config = function() _G.quiet_loaded = true end }} }}",
        root = q(&root),
        cfg = q(&mroot.join("config")),
        plug = q(&plugdir),
    );

    // Session 1: a persisted view owned by `quiet` is mounted in a left dock; quit.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl.clone());
        let (rpc, incoming) = start_attached(si, 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local v = btv.view.create{
              name = "Quiet", filetype = "btvview", persist = "main", namespace = "quiet",
            }
            v:set_lines({ "x" })
            v:mount{ dock = "left", size = 30 }
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the slot wakes the plugin, which claims nothing — the slot must collapse.
    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(decl);
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        assert!(
            poll_true(
                &rpc,
                "return _G.quiet_loaded == true and #btv.view.pending_restores() == 0",
            )
            .await,
            "the slot woke the plugin and then drained"
        );
        assert_eq!(
            window_count(&rpc).await,
            1,
            "the unclaimed slot collapsed after the wake-up load settled"
        );
    }
}

/// A persistent **component** (`btv.view.component` + `mount{ persist=}`) the framework
/// drives end-to-end: it resolves the owner namespace once, threads it into the backing
/// view + `ctx.store`, and on a restore its built-in router adopts the reserved slot and
/// re-runs `setup` (which rebuilds content from `ctx.store`). Sourced via `client_init_lua`
/// (which attributes to no rtp entry), so it passes an explicit `namespace = "notes"` — the
/// escape hatch, the same on both runs.
const NOTES_COMPONENT: &str = r#"
btv.shada.save_layout(true)
local Notes = btv.view.component({
  setup = function(ctx)
    _G.notes_buf = ctx.bufnr()
    return ctx.reactive({ lines = ctx.store:get("view:" .. ctx.persist_id) or { "default" } })
  end,
  render = function(s)
    return { lines = s.lines } -- reactive list returned directly; the backend materializes it
  end,
})
Notes.mount({ persist = "notes", namespace = "notes", dock = "left", size = 30 })
"#;

/// The persistent component's backing-buffer lines as "l1|l2|…" (or a sentinel), read off
/// the `_G.notes_buf` the component stashes in `setup`.
async fn notes_lines(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"
        local b = _G.notes_buf
        if not b then return "<nobuf>" end
        return table.concat(btv.buf.lines(b, 0, -1, false), "|")
        "#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

/// Pump barriers until the component's buffer shows `want` (its async mount/restore has
/// settled), or give up. Returns whether it landed.
async fn pump_until_notes(rpc: &bemtvi_rpc::Rpc, want: &str) -> bool {
    for _ in 0..200 {
        if notes_lines(rpc).await == want {
            return true;
        }
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }
    false
}

#[tokio::test]
async fn session_restores_a_persisted_view_component() {
    // Full round trip through the component framework (no hand-written on_restore): mount a
    // persistent component, seed its own store, quit; respawn and assert the component's
    // router adopted the reserved slot and rebuilt the content from `ctx.store`.
    let dir = temp_dir("session_view_component_rt");

    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(NOTES_COMPONENT.to_string());
        let (rpc, incoming) = start_attached(si, 80, 25).await;
        // The fresh-fallback mount settles to the default content.
        assert!(
            pump_until_notes(&rpc, "default").await,
            "the persistent component mounted fresh and rendered its default"
        );
        assert_eq!(window_count(&rpc).await, 2, "main + the Notes dock");
        // Persist known content into the component's OWN store (keyed by its persist id).
        exec_lua(
            &rpc,
            r#"btv.shada.plugin("notes"):set("view:notes", { "r1", "r2" })"#,
        )
        .await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(NOTES_COMPONENT.to_string());
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        assert!(
            pump_until_notes(&rpc, "r1|r2").await,
            "the component's restore router adopted the slot and rebuilt from ctx.store"
        );
        let pending = exec_lua(&rpc, "return #btv.view.pending_restores()")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert_eq!(pending, 0, "the reserved slot was adopted, not collapsed");
        assert_eq!(window_count(&rpc).await, 2, "the Notes dock came back");
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
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
        let name = exec_lua(&rpc, "return btv.buf.name(btv.win.buf(btv.win.current()))")
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
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
async fn current_buffer_name(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(rpc, "return btv.buf.name(btv.win.buf(btv.win.current()))")
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(&rpc, "btv.o.workspacepersistunnamed = false").await;
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await; // a capturing session
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
    exec_lua(&rpc, "btv.shada.save_layout(true)").await;
    exec_lua(&rpc, "btv.o.workspacepersistunnamed = false").await;
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
    // the server's `btv._go_mirror` push (the mirror key has to match the canonical name,
    // not the snake_case Rust field). Guards the rename + the new option.
    let (rpc, _incoming) =
        start_attached(init(&temp_dir("session_optmirror"), None, true), 80, 25).await;
    let got = exec_lua(
        &rpc,
        r#"
        btv.o.relativesplits = false
        btv.o.relativedocks = true
        btv.o.workspacepersistunnamed = false
        -- a second exec_lua chunk reads after the server pushes the mirror; do it inline
        -- via a barrier read so we exercise the round-trip, not just the write-through.
        return string.format("%s|%s|%s",
          tostring(btv.o.relativesplits), tostring(btv.o.relativedocks),
          tostring(btv.o.workspacepersistunnamed))
        "#,
    )
    .await;
    // Write-through within the chunk; the cross-tick mirror is checked below.
    assert_eq!(got.as_str(), Some("false|true|false"));

    let after = exec_lua(
        &rpc,
        r#"return string.format("%s|%s|%s",
             tostring(btv.o.relativesplits), tostring(btv.o.relativedocks),
             tostring(btv.o.workspacepersistunnamed))"#,
    )
    .await;
    assert_eq!(
        after.as_str(),
        Some("false|true|false"),
        "the values survive the server's mirror refresh (rename keys match)"
    );
}

#[tokio::test]
async fn capture_requires_the_plugin_opt_in() {
    // The namespace + restore are on, but the plugin never called
    // `btv.shada.save_layout(true)` — so capture is off by default and nothing is saved.
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
            "no layout captured without btv.shada.save_layout(true)"
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
    // NEVER calling `btv.shada.save_layout`.
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
            exec_lua(&rpc, "return btv.shada.namespace() == nil")
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await; // relativesplits defaults true
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2 at a WIDER terminal (120): the split is restored proportionally, so the
    // focused window is ~half of 120 (~59) — not the old ~39 it would be if sizes were
    // absolute cells.
    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 120, 25).await;
        let w = exec_lua(&rpc, "return btv.win.width(btv.win.current())")
            .await
            .as_i64()
            .unwrap_or(-1);
        assert!(w > 50, "the split scaled to the wider terminal (got {w})");
    }
}

// The main window's width (the dock surface has no `is_open` query, but a left dock
// reserves columns, so an open dock shrinks the main window — an observable signal).
async fn main_win_width(rpc: &bemtvi_rpc::Rpc) -> i64 {
    exec_lua(rpc, "return btv.win.width(btv.win.current())")
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(&rpc, "btv.dock.open{ side = 'left', size = 22 }").await;
        exec_lua(&rpc, "btv.layer.main()").await; // quit from the main area (focus rides along)
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
async fn session_restores_focus_left_in_a_dock() {
    // The focused LAYER rides the session: quit with the cursor in a dock and the restore
    // lands focus back in that dock, not the main area. The layout records which *window* was
    // active within each region, but the focused-layer keyword is what says the cursor sat in
    // the dock rather than the main editor.
    let dir = temp_dir("session_focus_dock_store");
    let file_a = write_temp("session_fdock_a", "txt", "a1\na2\n");
    let file_b = write_temp("session_fdock_b", "txt", "b1\nb2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(&rpc, "btv.dock.open{ side = 'left', size = 22 }").await; // focuses the dock
        feed(&rpc, &format!(":edit {file_b}<CR>")); // the dock now shows file_b; focus stays
        feed(&rpc, "<Esc>");
        assert!(
            current_buffer_name(&rpc).await.contains(&file_b),
            "precondition: focus is the dock (showing file_b) at quit"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let name = current_buffer_name(&rpc).await;
        assert!(
            name.contains(&file_b),
            "focus restored to the dock (file_b), not the main area: got {name:?}"
        );
    }
}

/// An `btv.view.on_restore` handler that — like a real file-tree sidebar — FOCUSES its dock as
/// it re-adopts the reserved window. Reproduces the focus-theft that used to strand the cursor
/// in the dock after a restore.
const FOCUS_GRAB_RESTORE_HANDLER: &str = r#"
btv.view.on_restore(function(id, place)
  local v = btv.view.create{
    name = "Tree", filetype = "btvview", persist = id, namespace = "treens",
  }
  v:set_lines({ "root", "  a.txt" })
  place(v)
  v:focus()  -- a sidebar plugin grabbing focus as it re-adopts its dock (nvim-tree-style)
end, "treens")
"#;

#[tokio::test]
async fn session_restores_main_focus_even_when_a_dock_plugin_grabs_it() {
    // The reported annoyance, exactly: a left dock (a file-tree plugin's persisted view) is
    // open but the cursor is in the MAIN editor at `:qa`. On restart the plugin re-adopts its
    // dock and focuses it — which stranded the cursor in the dock. The captured focus layer is
    // re-asserted AFTER the plugin restore + VimEnter, so focus lands back in the main area.
    let dir = temp_dir("session_focus_main_grab_store");
    let file_a = write_temp("session_fmain_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines({ "root", "  a.txt" })
            v:mount{ dock = "left", size = 30 }  -- mounting focuses the dock
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await; // cross back to the main editor before quit
        assert!(
            current_buffer_name(&rpc).await.contains(&file_a),
            "precondition: focus is the main file before quit"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(FOCUS_GRAB_RESTORE_HANDLER.to_string());
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        let name = current_buffer_name(&rpc).await;
        assert!(
            name.contains(&file_a),
            "focus returned to the main editor, not the dock the plugin grabbed: got {name:?}"
        );
    }
}

// The widths of the two main split windows (showing `session_ds_a` / `_b`), as
// "<a>,<b>". Used to compare the live split against the restored one.
async fn split_widths(rpc: &bemtvi_rpc::Rpc) -> (i64, i64) {
    let s = exec_lua(
        rpc,
        "local w = { a = -1, b = -1 }\n\
         for _, win in ipairs(btv.win.list()) do\n\
           local name = btv.buf.name(btv.win.buf(win))\n\
           if name:match('session_ds_a') then w.a = btv.win.width(win) end\n\
           if name:match('session_ds_b') then w.b = btv.win.width(win) end\n\
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
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(&rpc, "btv.o.relativesplits = false").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>")); // A | B in the main area
        exec_lua(&rpc, "btv.dock.open{ side = 'left', size = 22 }").await;
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
    // `relativedocks` is a NATIVE editor option (`btv.o.relativedocks`), not a plugin
    // setting: any wrapper that opts a session into capture honors it. With it on, a
    // dock's size is stored as a % of the screen and re-derived against the screen the
    // restore runs at (the editor's default 80 cols, before the UI attaches) — so a dock
    // captured wide comes back proportionally smaller, not at its old cell count.
    let dir = temp_dir("session_reldock_store");
    let file_a = write_temp("session_reldock_a", "txt", "a1\na2\n");

    // Session 1 at width 160: opt in, flip the native option on via `btv.o`, open an
    // 80-col left dock (= 50% of 160). Quit so the exit flush captures it as 50%.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 160, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(&rpc, "btv.o.relativedocks = true").await;
        exec_lua(&rpc, "btv.dock.open{ side = 'left', size = 80 }").await;
        exec_lua(&rpc, "btv.layer.main()").await; // measure the MAIN window after restore
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

/// "[name]line/line||[name]…" over every (main + dock) window's buffer — a layout dump
/// that spans docks (`btv.win.list` enumerates dock windows too), used to assert the
/// restored unnamed content came back regardless of which layer holds it.
async fn window_content_dump(rpc: &bemtvi_rpc::Rpc) -> String {
    exec_lua(
        rpc,
        r#"local out = {}
           for _, w in ipairs(btv.win.list()) do
             local b = btv.win.buf(w)
             out[#out + 1] = "[" .. (btv.buf.name(b) or "") .. "]" ..
               table.concat(btv.buf.lines(b, 0, -1), "/")
           end
           return table.concat(out, "||")"#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

#[tokio::test]
async fn qa_persists_an_unnamed_buffer_shown_in_a_dock() {
    // Regression: a modified `[No Name]` buffer parked in an edge **dock** rides the
    // workspace session just like one in a main window — and a bang-less `:qa` doesn't
    // block on it. Two bugs were in play: (1) the dock capture hard-coded
    // `allow_unnamed = false`, so a dock's unnamed content was never saved; (2) `:qa`'s
    // exemption used `window_showing`, which scans only the main tabs (and, while a dock
    // is focused, reads the *dock* tree for the active main tab), so it missed both a
    // dock-shown buffer AND the main-shown one — surfacing `E37` and yanking focus to
    // the main layer instead of quitting.
    let dir = temp_dir("session_qa_dock_unnamed");
    let file_a = write_temp("session_qa_dock_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        // Main area: file_a alongside an unnamed edited buffer in a vsplit.
        feed(&rpc, ":vnew<CR>");
        feed(&rpc, "imain scratch<Esc>");
        // A bottom dock holding its own unnamed edited buffer (focus is now the dock).
        exec_lua(&rpc, "btv.dock.open{ side = 'bottom', size = 6 }").await;
        feed(&rpc, "idock scratch<Esc>");
        // `:qa` (no bang) WHILE THE DOCK IS FOCUSED must quit — every modified buffer is
        // unnamed and persisted, so none blocks. The await completing proves it quit.
        feed(&rpc, ":qa<CR>");
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            await_server_exit(incoming),
        )
        .await
        .expect("`:qa` from a focused dock quit without an E37 nag");
    }

    {
        let (rpc, _incoming) = start_attached(init(&dir, None, true), 80, 25).await;
        let dump = window_content_dump(&rpc).await;
        assert!(
            dump.contains("main scratch"),
            "the main-window unnamed buffer came back: {dump}"
        );
        assert!(
            dump.contains("dock scratch"),
            "the dock unnamed buffer's content came back: {dump}"
        );
    }
}

#[tokio::test]
async fn qa_from_main_does_not_block_on_a_dock_unnamed_buffer() {
    // The mirror of the above: `:qa` issued from the MAIN layer must not block on a
    // modified unnamed buffer living in a dock — it is captured, so quitting is safe.
    let dir = temp_dir("session_qa_main_then_dock");
    let file_a = write_temp("session_qa_main_a", "txt", "a1\na2\n");

    let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
    exec_lua(&rpc, "btv.shada.save_layout(true)").await;
    exec_lua(&rpc, "btv.dock.open{ side = 'bottom', size = 6 }").await;
    feed(&rpc, "idock scratch<Esc>");
    // Cross back up out of the bottom dock into the main window, then quit — the dock's
    // modified unnamed buffer must not keep us here.
    feed(&rpc, "<C-w>k");
    feed(&rpc, ":qa<CR>");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        await_server_exit(incoming),
    )
    .await
    .expect("`:qa` from the main layer quit despite the dock's unnamed buffer");
}

/// A restore handler that focuses its dock on a DEFERRED tick — `btv.on_next_tick` (twice),
/// past `VimEnter` — the way a real sidebar finishes its async build and focuses itself well
/// after startup. The one-shot re-assert can't catch this; the held re-pin does.
const DEFERRED_GRAB_RESTORE_HANDLER: &str = r#"
btv.view.on_restore(function(id, place)
  local v = btv.view.create{
    name = "Tree", filetype = "btvview", persist = id, namespace = "treens",
  }
  v:set_lines({ "root", "  a.txt" })
  place(v)
  btv.on_next_tick(function()
    btv.on_next_tick(function()
      v:focus() -- async self-focus, several ticks into startup
    end)
  end)
end, "treens")
"#;

#[tokio::test]
async fn session_holds_main_focus_against_a_deferred_dock_grab() {
    // The durable guarantee behind the reported annoyance: even when a sidebar plugin grabs
    // its dock on a DEFERRED tick (after VimEnter), the restored main focus is held until the
    // user acts — and released the moment they do.
    let dir = temp_dir("session_deferred_grab_store");
    let file_a = write_temp("session_defgrab_a", "txt", "a1\na2\n");

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        exec_lua(
            &rpc,
            r#"
            local v = btv.view.create{
              name = "Tree", filetype = "btvview", persist = "main", namespace = "treens",
            }
            v:set_lines({ "root", "  a.txt" })
            v:mount{ dock = "left", size = 30 }
            "#,
        )
        .await;
        exec_lua(&rpc, "btv.layer.main()").await;
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    {
        let mut si = init(&dir, None, true);
        si.client_init_lua = Some(DEFERRED_GRAB_RESTORE_HANDLER.to_string());
        let (rpc, _incoming) = start_attached(si, 80, 25).await;
        // Pump idle ticks so the deferred self-focus fires; the hold re-pins each time.
        for _ in 0..30 {
            rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        }
        assert!(
            current_buffer_name(&rpc).await.contains(&file_a),
            "main focus held against the deferred dock grab (no user input yet)"
        );
        // The user takes over: a keypress releases the hold, so an explicit cross into the
        // dock now sticks (the focused buffer is the dock's "Tree" view, not file_a).
        feed(&rpc, "<Esc>");
        exec_lua(&rpc, "btv.layer.focus('left')").await;
        let nm = current_buffer_name(&rpc).await;
        assert!(
            !nm.contains(&file_a),
            "after the user acts the hold is released — focusing the dock sticks (got {nm:?})"
        );
    }
}

// ----- restored buffers get the full read lifecycle ---------------------------

/// `init` plus startup Lua sourced *before* the lifecycle seed — the hook a session
/// manager or plugin would register its autocmds from, so they are live when the
/// restored layout announces.
fn init_probed(dir: &Path, file: Option<String>, session: bool, lua: &str) -> ServerInit {
    let mut i = init(dir, file, session);
    i.client_init_lua = Some(lua.to_string());
    i
}

/// Record every buffer-lifecycle event, tagged with the basename it fired for.
const EVENT_PROBE: &str = r#"
_G.evs = {}
for _, e in ipairs({ "BufReadPost", "BufNewFile", "FileType", "BufEnter", "BufWinEnter" }) do
  btv.autocmd.create({ e }, { callback = function(a)
    local base = tostring(a.file):match("([^/]+)$") or tostring(a.file)
    _G.evs[#_G.evs + 1] = e .. ":" .. base
  end })
end
"#;

/// Events recorded for `basename`, in fire order, joined with `,`.
async fn events_for(rpc: &bemtvi_rpc::Rpc, basename: &str) -> String {
    exec_lua(
        rpc,
        &format!(
            "local out = {{}}\n\
             for _, e in ipairs(_G.evs) do\n\
             \x20 if e:sub(-#{name:?}) == {name:?} then out[#out+1] = e:match('^([^:]+)') end\n\
             end\n\
             return table.concat(out, ',')",
            name = basename
        ),
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

#[tokio::test]
async fn restoring_a_session_announces_every_restored_buffer_not_just_the_focused_one() {
    // The defect this closes: `emit_lifecycle_events` was a CURRENT-buffer diff, so a
    // restore — which fills background windows without entering them — fired
    // `BufWinEnter` for them and nothing else. Everything `FileType`-driven (LSP
    // attach, treesitter, buffer-local maps) stayed inert until the user happened to
    // focus that window.
    //
    // Neovim announces all three, because its session script `:buffer`s each file into
    // its window and entering an unloaded buffer loads it. Measured on nvim 0.12.2:
    //
    //   BufReadPost:c.txt  FileType:c.txt  BufEnter:c.txt  BufWinEnter:c.txt
    //   BufReadPost:b.py   FileType:b.py   BufEnter:b.py   BufWinEnter:b.py
    //   BufReadPost:a.lua  FileType:a.lua  BufEnter:a.lua  BufWinEnter:a.lua
    //
    // We match that on BufReadPost/FileType/BufWinEnter. `BufEnter` is deliberately
    // NOT fired for the background two: it means "this buffer became current", which
    // is true only of the focused one. (Neovim fires it because its script really does
    // enter each window in turn; our restore builds the layout directly.)
    let dir = temp_dir("session_lifecycle");
    let file_a = write_temp("sess_life_a", "lua", "return 1\n");
    let file_b = write_temp("sess_life_b", "py", "x = 1\n");
    let file_c = write_temp("sess_life_c", "rs", "fn main() {}\n");
    let base = |p: &str| p.rsplit('/').next().unwrap().to_string();
    let (a, b, c) = (base(&file_a), base(&file_b), base(&file_c));

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        feed(&rpc, &format!(":split {file_c}<CR>"));
        assert_eq!(window_count(&rpc).await, 3, "three windows before quit");
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    let (rpc, _incoming) = start_attached(init_probed(&dir, None, true, EVENT_PROBE), 80, 25).await;
    assert_eq!(window_count(&rpc).await, 3, "the layout came back");

    for name in [&a, &b, &c] {
        let evs = events_for(&rpc, name).await;
        assert!(
            evs.contains("BufReadPost"),
            "{name} was read-announced on restore, got {evs:?}"
        );
        assert!(
            evs.contains("FileType"),
            "{name} got its FileType (this is what drives LSP attach / treesitter / \
             buffer-local maps), got {evs:?}"
        );
        assert!(
            evs.starts_with("BufReadPost,FileType"),
            "and in neovim's per-buffer order, got {evs:?}"
        );
        assert!(
            evs.contains("BufWinEnter"),
            "{name} is displayed, got {evs:?}"
        );
    }
}

#[tokio::test]
async fn a_restored_buffer_is_not_announced_twice_when_it_is_later_focused() {
    // The announce is fire-once per buffer (`announced`). Before this phase a
    // background buffer announced lazily on first focus; now it announces at restore,
    // and focusing it later must NOT re-fire — otherwise every plugin keyed on
    // BufReadPost/FileType does its setup twice.
    let dir = temp_dir("session_lifecycle_once");
    let file_a = write_temp("sess_once_a", "lua", "return 1\n");
    let file_b = write_temp("sess_once_b", "py", "x = 1\n");
    let base = |p: &str| p.rsplit('/').next().unwrap().to_string();
    let b = base(&file_b);

    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        feed(&rpc, "<C-w>h"); // focus A, so B is restored in a background window
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    let (rpc, _incoming) = start_attached(init_probed(&dir, None, true, EVENT_PROBE), 80, 25).await;
    let before = events_for(&rpc, &b).await;
    assert!(
        before.contains("BufReadPost") && before.contains("FileType"),
        "the background buffer announced at restore, got {before:?}"
    );

    // Now focus it: BufEnter is expected, a second announce is not.
    exec_lua(
        &rpc,
        &format!(
            "for _, w in ipairs(btv.win.list()) do\n\
             \x20 if btv.buf.name(btv.win.buf(w)):sub(-#{b:?}) == {b:?} then btv.win.set_current(w) end\n\
             end"
        ),
    )
    .await;
    exec_lua(&rpc, "return 1").await;
    let after = events_for(&rpc, &b).await;
    assert_eq!(
        after.matches("BufReadPost").count(),
        1,
        "BufReadPost fired exactly once across restore + focus, got {after:?}"
    );
    assert_eq!(
        after.matches("FileType").count(),
        1,
        "and FileType exactly once, got {after:?}"
    );
    assert!(
        after.contains("BufEnter"),
        "while BufEnter did fire on the actual entry, got {after:?}"
    );
}

/// A config's window-local `vim.opt` settings survive a session restore. The restore
/// mints a fresh window per saved leaf, so it runs AFTER `init.lua` is sourced and each
/// window inherits the configured startup window (`scrolloff` / `signcolumn` / …),
/// exactly as a `:split` does. Restoring before the config (as the boot once did) left
/// every restored window at the built-in defaults — the config's window options silently
/// lost — and addressed the config's writes to window ids the restore had already retired.
#[tokio::test]
async fn session_restore_keeps_config_window_options() {
    let dir = temp_dir("session_opts_store");
    let cfg = temp_dir("session_opts_cfg");
    std::fs::write(
        cfg.join("init.lua"),
        "vim.opt.scrolloff = 8\nvim.opt.signcolumn = \"yes:2\"\nvim.opt.cursorline = true\n",
    )
    .expect("write init.lua");
    let file_a = write_temp("session_opt_a", "txt", "a1\na2\na3\n");
    let file_b = write_temp("session_opt_b", "txt", "b1\nb2\nb3\n");

    let with_cfg = |file: Option<String>| ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        ..init(&dir, file, true)
    };

    // Every window's window-local options, as "so=<n> scl=<s> cul=<b>" joined by "|".
    const READ_WIN_OPTS: &str = r#"
        local out = {}
        for _, w in ipairs(btv.win.list()) do
          out[#out + 1] = "so=" .. tostring(btv.wo[w].scrolloff)
            .. " scl=" .. tostring(btv.wo[w].signcolumn)
            .. " cul=" .. tostring(btv.wo[w].cursorline)
        end
        return table.concat(out, "|")
    "#;

    // Session 1: two windows in a vsplit, captured by the exit flush.
    {
        let (rpc, incoming) = start_attached(with_cfg(Some(file_a.clone())), 80, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, &format!(":vsplit {file_b}<CR>"));
        assert_eq!(window_count(&rpc).await, 2, "two windows before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the restored layout comes back wearing the config's window options.
    {
        let (rpc, _incoming) = start_attached(with_cfg(None), 80, 25).await;
        assert_eq!(window_count(&rpc).await, 2, "the layout came back");
        let opts = exec_lua(&rpc, READ_WIN_OPTS)
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            opts, "so=8 scl=yes:2 cul=true|so=8 scl=yes:2 cul=true",
            "every restored window carries init.lua's window options"
        );
    }
}

/// A restored session must not come back **horizontally scrolled** into empty space.
/// The layout is rebuilt during startup, before the client's `btv_ui_attach` hands over
/// the real terminal size — so the restore's `ensure_visible` computes `leftcol` against
/// the boot placeholder width (80). Reattaching at a width where the whole line fits left
/// that stale `leftcol` in place: the buffer painted scrolled sideways with the cursor at
/// the right buffer column, and nothing to scroll back to (the wheel's own clamp already
/// says a window whose lines all fit has `max_leftcol == 0`).
#[tokio::test]
async fn session_restores_unscrolled_when_the_line_fits_the_real_width() {
    let dir = temp_dir("session_leftcol_store");
    // One line far wider than the 80-column boot default, but well inside the 200-column
    // terminal the client attaches with.
    let long = "x".repeat(150);
    let file = write_temp("session_leftcol", "txt", &format!("{long}\nshort\n"));

    // Session 1: park the cursor at the end of the long line, then quit.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file.clone()), true), 200, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, "$");
        assert_eq!(cursor(&rpc).await, (1, 149), "cursor at the line end");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: same 200-column terminal — the line fits, so the view is unscrolled.
    {
        let (rpc, mut incoming) = start_attached(init(&dir, None, true), 200, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 149), "cursor column restored");
        let map = bemtvi_test_harness::redraw_after(&rpc, &mut incoming, "").await;
        assert_eq!(
            bemtvi_test_harness::window0_field(&map, "leftcol").and_then(rmpv::Value::as_u64),
            Some(0),
            "a 150-column line inside a 200-column window is never scrolled sideways"
        );
    }
}

/// The same stale-`leftcol` restore bug as above, but for a tab that is **not** the
/// active one. The clamp that fixes the focused window runs on `resize`, which can only
/// measure windows that are laid out — an inactive tab's tree is parked off `self.windows`
/// and has no rect, so it was skipped. Switching to that tab then restores its stashed
/// `leftcol` verbatim: the tab paints scrolled sideways into empty space with nothing to
/// scroll back to.
#[tokio::test]
async fn session_restores_an_inactive_tab_unscrolled_too() {
    let dir = temp_dir("session_leftcol_tab_store");
    // Both lines are far wider than the 80-column boot default, but fit the 200-column
    // terminal the client attaches with.
    let long = "x".repeat(150);
    let file_a = write_temp("session_leftcol_tab_a", "txt", &format!("{long}\nshort\n"));
    let file_b = write_temp("session_leftcol_tab_b", "txt", &format!("{long}\nshort\n"));

    // Session 1: two tabs, each parked at the end of its long line.
    {
        let (rpc, incoming) = start_attached(init(&dir, Some(file_a.clone()), true), 200, 25).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, "$");
        feed(&rpc, &format!(":tabnew {file_b}<CR>"));
        feed(&rpc, "$");
        assert_eq!(tab_count(&rpc).await, 2, "two tabs before quit");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the restored active tab is unscrolled (the resize clamp) — and so is
    // the other tab once it is switched to.
    {
        let (rpc, mut incoming) = start_attached(init(&dir, None, true), 200, 25).await;
        assert_eq!(tab_count(&rpc).await, 2, "both tab pages came back");
        let map = bemtvi_test_harness::redraw_after(&rpc, &mut incoming, "").await;
        assert_eq!(
            bemtvi_test_harness::window0_field(&map, "leftcol").and_then(rmpv::Value::as_u64),
            Some(0),
            "the active tab is not scrolled sideways"
        );
        let map = bemtvi_test_harness::redraw_after(&rpc, &mut incoming, "gT").await;
        assert_eq!(
            bemtvi_test_harness::window0_field(&map, "leftcol").and_then(rmpv::Value::as_u64),
            Some(0),
            "a 150-column line inside a 200-column window is never scrolled sideways, \
             whichever tab it is in"
        );
    }
}

/// The same restore over an **off-tick** filesystem (a daemon session — and the web build,
/// which shares this path): the cursor must come back where it was left, not at line 1.
///
/// Natively the restore's leaf reads are synchronous, so by the time
/// `install_restored_tree` clamps the focused window's cursor the buffer already holds the
/// file. Off-tick, `open_buffer_for_restore` only *enqueues* a replica open, so the buffer
/// is empty at that moment and the clamp snaps the saved line to the top — silently, since
/// the text then arrives a tick later and looks perfectly restored. The core already models
/// this exactly (`Editor::pending_open_cursor` / `settle_loaded_cursor`, what a jump into a
/// not-yet-fetched buffer uses); the restore just wasn't using it.
///
/// The daemon fs is the same seam the browser uses (`host_fs_offtick`), so this covers the
/// web session-restore path too — a tier-1 feature must behave identically over the wire.
#[tokio::test]
async fn session_restores_the_cursor_over_off_tick_fs() {
    use bemtvi_test_harness::{await_lines, spawn_with_daemon_fs_init, DaemonFs};

    let dir = temp_dir("session_store_offtick");
    // A fresh daemon fs per session (the fake is consumed by the daemon task), same content.
    let seed = || {
        let fake = DaemonFs::default();
        fake.set("/virtual/notes.txt", "n1\nn2\nn3\nn4\nn5\n");
        fake
    };
    let session_init = |file: Option<&str>| ServerInit {
        file: file.map(str::to_string),
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: true,
        restore_session: true,
        ..Default::default()
    };

    // Session 1: the file lives only on the daemon, so the open crosses the wire. Park the
    // cursor on line 4 and quit — the exit flush captures the layout with that cursor.
    {
        let (rpc, incoming) =
            spawn_with_daemon_fs_init(seed(), session_init(Some("/virtual/notes.txt"))).await;
        await_lines(&rpc, &["n1", "n2", "n3", "n4", "n5"]).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, "4G");
        assert_eq!(
            cursor(&rpc).await,
            (4, 0),
            "cursor parked on line 4 before quit"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the restore rebuilds the window immediately and fills it over the wire a
    // tick or two later. Wait for the text, then the cursor must be back on line 4.
    {
        let (rpc, _incoming) = spawn_with_daemon_fs_init(seed(), session_init(None)).await;
        assert_eq!(
            await_lines(&rpc, &["n1", "n2", "n3", "n4", "n5"]).await,
            vec!["n1", "n2", "n3", "n4", "n5"],
            "the restored buffer fills in from the daemon"
        );
        assert_eq!(
            cursor(&rpc).await,
            (4, 0),
            "the cursor is restored once the off-tick read lands, not left clamped at line 1"
        );
    }
}

/// The off-tick cursor restore across **tabs**, where the clamp against a not-yet-arrived
/// buffer can bite twice more: the restore leaves each tab in turn (stashing the live
/// view into the outgoing window), and entering a tab restores its focused window's saved
/// position — both against a buffer whose bytes are still in flight.
#[tokio::test]
async fn session_restores_the_cursor_in_every_tab_over_off_tick_fs() {
    use bemtvi_test_harness::{await_lines, spawn_with_daemon_fs_init, DaemonFs};

    let dir = temp_dir("session_store_offtick_tabs");
    let seed = || {
        let fake = DaemonFs::default();
        fake.set("/virtual/one.txt", "1a\n1b\n1c\n1d\n1e\n")
            .set("/virtual/two.txt", "2a\n2b\n2c\n2d\n2e\n");
        fake
    };
    let session_init = |file: Option<&str>| ServerInit {
        file: file.map(str::to_string),
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: true,
        restore_session: true,
        ..Default::default()
    };

    // Session 1: tab 1 on line 4 of one.txt, tab 2 on line 2 of two.txt, quit from tab 1.
    {
        let (rpc, incoming) =
            spawn_with_daemon_fs_init(seed(), session_init(Some("/virtual/one.txt"))).await;
        await_lines(&rpc, &["1a", "1b", "1c", "1d", "1e"]).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, "4G");
        feed(&rpc, ":tabnew /virtual/two.txt<CR>");
        await_lines(&rpc, &["2a", "2b", "2c", "2d", "2e"]).await;
        feed(&rpc, "2G");
        assert_eq!(cursor(&rpc).await, (2, 0), "tab 2 parked on line 2");
        feed(&rpc, "gT"); // back to tab 1, so that is the tab the session is quit from
        assert_eq!(cursor(&rpc).await, (4, 0), "tab 1 still on line 4");
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: both tabs come back, each with its own cursor — the active one once its
    // read lands, the other when it is switched to.
    {
        let (rpc, _incoming) = spawn_with_daemon_fs_init(seed(), session_init(None)).await;
        assert_eq!(tab_count(&rpc).await, 2, "both tabs restored");
        assert_eq!(
            await_lines(&rpc, &["1a", "1b", "1c", "1d", "1e"]).await,
            vec!["1a", "1b", "1c", "1d", "1e"],
            "the active tab's buffer fills in from the daemon"
        );
        assert_eq!(
            cursor(&rpc).await,
            (4, 0),
            "the active tab's cursor survives the restore"
        );
        feed(&rpc, "gt");
        assert_eq!(
            await_lines(&rpc, &["2a", "2b", "2c", "2d", "2e"]).await,
            vec!["2a", "2b", "2c", "2d", "2e"],
            "the background tab's buffer is there once it is switched to"
        );
        assert_eq!(
            cursor(&rpc).await,
            (2, 0),
            "the background tab kept its own cursor through the restore"
        );
    }
}

/// The scroll position rides along with the cursor over an off-tick fs. Same family as
/// the cursor: the restore sets the window's saved `top`, then `ensure_visible` runs
/// against an EMPTY replica and the read landing resets the scroll to the top of the
/// file — so a session left deep in a long file came back with the cursor right but the
/// viewport re-derived (the cursor pinned to the last screen row instead of where it sat).
#[tokio::test]
async fn session_restores_the_scroll_position_over_off_tick_fs() {
    use bemtvi_test_harness::{await_lines, spawn_with_daemon_fs_init, DaemonFs};

    let dir = temp_dir("session_store_offtick_scroll");
    let body: String = (1..=80).map(|n| format!("line{n}\n")).collect();
    let first: Vec<String> = (1..=5).map(|n| format!("line{n}")).collect();
    let seed = {
        let body = body.clone();
        move || {
            let fake = DaemonFs::default();
            fake.set("/virtual/long.txt", &body);
            fake
        }
    };
    let session_init = |file: Option<&str>| ServerInit {
        file: file.map(str::to_string),
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        workspace_session: true,
        restore_session: true,
        ..Default::default()
    };
    // The cursor's SCREEN row is the viewport made visible: `zt` puts it near the top,
    // while a re-derived scroll (`ensure_visible` from the top of the file) pins it to the
    // last row. The frame carries no topline, and this is the difference a user sees.
    let cursor_row = |map: &[(rmpv::Value, rmpv::Value)]| {
        bemtvi_test_harness::window0_field(map, "cursor_row").and_then(rmpv::Value::as_u64)
    };

    // Session 1: scroll deep into the file with the cursor parked at the top of the
    // viewport (`zt`), so the saved scroll is not something `ensure_visible` would
    // re-derive on its own.
    let saved_top = {
        let (rpc, mut incoming) =
            spawn_with_daemon_fs_init(seed(), session_init(Some("/virtual/long.txt"))).await;
        await_lines(&rpc, &first.iter().map(String::as_str).collect::<Vec<_>>()).await;
        exec_lua(&rpc, "btv.shada.save_layout(true)").await;
        feed(&rpc, "60G");
        let map = bemtvi_test_harness::redraw_after(&rpc, &mut incoming, "zt").await;
        let top = cursor_row(&map).expect("a cursor_row in the frame");
        assert!(
            top < 5,
            "`zt` really did park the cursor near the top of the viewport: row {top}"
        );
        feed(&rpc, ":qa<CR>");
        await_server_exit(incoming).await;
        top
    };

    // Session 2: the same viewport comes back once the off-tick read lands.
    {
        let (rpc, mut incoming) = spawn_with_daemon_fs_init(seed(), session_init(None)).await;
        await_lines(&rpc, &first.iter().map(String::as_str).collect::<Vec<_>>()).await;
        assert_eq!(
            cursor(&rpc).await,
            (60, 0),
            "the cursor came back on line 60"
        );
        let map = bemtvi_test_harness::redraw_after(&rpc, &mut incoming, "").await;
        assert_eq!(
            cursor_row(&map),
            Some(saved_top),
            "the restored window shows the same viewport it was left showing (the cursor \
             sits where it did, not pinned to the bottom row by a re-derived scroll)"
        );
    }
}
