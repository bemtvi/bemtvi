use crate::support::*;

// ----- the bottom panel: focus-locked overlays over ordinary buffers ---------
//
// `:messages` / `:registers` / `:marks` / `:jumps` / `:changes` (and `:ls`, and scripted
// `nx.panel.open`) open a **panel**: an ordinary `nomodifiable` buffer shown in a
// focus-locked bottom overlay. Navigation is plain motions; selection / dismissal are
// buffer-local maps a `FileType` autocmd installs (`q` / `<Esc>` to close everywhere,
// `<CR>` to switch for `:ls`). There is no bespoke navigation/content/select API — the
// "grab" is purely a focus lock (`<C-w>` can't leave). Per-command content lives in each
// feature's own suite; these cover the shared panel behavior.

#[tokio::test]
async fn messages_command_opens_a_panel_with_the_history() {
    let (rpc, _incoming) = start(None).await;

    feed(&rpc, ":lua print('alpha')<CR>");
    feed(&rpc, ":lua print('beta')<CR>");
    feed(&rpc, ":messages<CR>");

    // A panel is open, focused, holding the history (newest last).
    assert!(panel_is_open(&rpc).await, "`:messages` opens a panel");
    let shown = lines(&rpc).await;
    assert!(
        shown.contains(&"alpha".to_string()) && shown.contains(&"beta".to_string()),
        "history was: {shown:?}"
    );

    // It is `nomodifiable`: an edit is refused (E21) and the content is untouched.
    feed(&rpc, "dd");
    assert_eq!(
        lines(&rpc).await,
        shown,
        "the listing is read-only, so `dd` must not change it"
    );
}

#[tokio::test]
async fn a_panel_is_navigable_with_plain_motions() {
    let (rpc, _incoming) = start(None).await;
    for i in 0..15 {
        feed(&rpc, &format!(":lua print('line{i}')<CR>"));
    }
    feed(&rpc, ":messages<CR>");
    // The panel is an ordinary buffer: `G` / `gg` are normal motions on it (the old panel
    // routed these through a bespoke grab; now they just move the cursor).
    feed(&rpc, "G");
    let bottom = cursor(&rpc).await.0;
    feed(&rpc, "gg");
    assert_eq!(cursor(&rpc).await.0, 1, "gg reaches the first line");
    assert!(bottom > 1, "G reached a lower line ({bottom}) first");
}

#[tokio::test]
async fn a_panel_hard_locks_focus() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":lua print('locked')<CR>");
    feed(&rpc, ":messages<CR>");
    let shown = lines(&rpc).await;
    assert!(
        shown.contains(&"locked".to_string()),
        "panel content: {shown:?}"
    );

    // Window-nav is inert while the panel is up: `<C-w>w` / `<C-w>j` can't leave it, so the
    // focused buffer is still the panel (its content unchanged), not the main buffer.
    feed(&rpc, "<C-w>w");
    assert_eq!(
        lines(&rpc).await,
        shown,
        "<C-w>w must not leave the locked panel"
    );
    feed(&rpc, "<C-w>j");
    assert_eq!(
        lines(&rpc).await,
        shown,
        "<C-w>j must not leave the locked panel"
    );
    assert!(panel_is_open(&rpc).await, "the panel is still open");
}

#[tokio::test]
async fn q_and_esc_dismiss_the_panel() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>"); // give the main buffer distinct content
    let main = lines(&rpc).await;

    feed(&rpc, ":messages<CR>");
    assert!(panel_is_open(&rpc).await, "panel open");
    feed(&rpc, "q");
    assert!(!panel_is_open(&rpc).await, "`q` dismisses the panel");
    assert_eq!(lines(&rpc).await, main, "focus returns to the main buffer");

    // `<Esc>` dismisses too.
    feed(&rpc, ":messages<CR>");
    assert!(panel_is_open(&rpc).await, "panel re-opens");
    feed(&rpc, "<Esc>");
    assert!(!panel_is_open(&rpc).await, "`<Esc>` dismisses the panel");
    assert_eq!(lines(&rpc).await, main, "focus returns to the main buffer");
}

#[tokio::test]
async fn nx_panel_open_mounts_a_scripted_panel_with_buffer_local_keys() {
    let (rpc, _incoming) = start(None).await;

    // A plugin opens a panel and wires its own `<CR>` via a `FileType` autocmd — the
    // ordinary buffer mechanism, not a bespoke select callback.
    exec_lua(
        &rpc,
        r#"
        nx.autocmd.create("FileType", {
          pattern = "myft",
          callback = function(args)
            nx.keymap.set("n", "<CR>", function() nx._panel_hit = true end, { buffer = args.buf })
          end,
        })
        nx.panel.open{ lines = { "one", "two", "three" }, filetype = "myft" }
        "#,
    )
    .await;

    assert!(panel_is_open(&rpc).await, "nx.panel.open opens a panel");
    assert_eq!(
        lines(&rpc).await,
        vec!["one".to_string(), "two".to_string(), "three".to_string()],
    );

    // The buffer-local `<CR>` fires inside the panel.
    feed(&rpc, "<CR>");
    assert_eq!(
        lua_bool(&rpc, "return nx._panel_hit == true").await,
        Some(true),
        "the buffer-local <CR> fired"
    );

    // `nx.panel.close()` dismisses it.
    exec_lua(&rpc, "nx.panel.close()").await;
    assert!(
        !panel_is_open(&rpc).await,
        "nx.panel.close() closes the panel"
    );
}

// ----- named panels: hidden from :ls, listed by :lspanels --------------------

#[tokio::test]
async fn ls_excludes_panel_buffers() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>"); // a real document (buffer 1)

    // Build a couple of panel buffers, dismissing each.
    feed(&rpc, ":lua print('m')<CR>");
    feed(&rpc, ":messages<CR>");
    feed(&rpc, "q");
    feed(&rpc, ":registers<CR>");
    feed(&rpc, "q");

    // `:ls` lists documents only — panel buffers are surfaces, not documents.
    feed(&rpc, ":ls<CR>");
    let shown = lines(&rpc).await;
    assert!(
        shown.iter().all(|l| !l.contains("[Messages]")
            && !l.contains("[Registers]")
            && !l.contains("[Buffers]")),
        ":ls must not list panel buffers; got {shown:?}"
    );
}

#[tokio::test]
async fn lspanels_lists_named_panels_and_navigates_to_last_content() {
    let (rpc, _incoming) = start(None).await;

    // Two distinct *named* panels with distinct content — they don't clobber each other
    // (the old shared scratch buffer did).
    feed(&rpc, ":lua print('msg-line')<CR>");
    feed(&rpc, ":messages<CR>");
    assert!(lines(&rpc).await.iter().any(|l| l.contains("msg-line")));
    feed(&rpc, "q");
    feed(&rpc, ":registers<CR>");
    assert!(lines(&rpc)
        .await
        .iter()
        .any(|l| l.contains("Type Name Content")));
    feed(&rpc, "q");

    // `:lspanels` opens as a panel listing both named panels.
    feed(&rpc, ":lspanels<CR>");
    assert!(panel_is_open(&rpc).await, ":lspanels opens as a panel");
    let panels = lines(&rpc).await;
    assert!(
        panels.iter().any(|l| l.contains("[Messages]")),
        "panels: {panels:?}"
    );
    assert!(
        panels.iter().any(|l| l.contains("[Registers]")),
        "panels: {panels:?}"
    );
    assert!(
        panels.iter().all(|l| !l.contains("[Panels]")),
        "the panel list omits itself; got {panels:?}"
    );

    // `<CR>` on the [Messages] row (first, lowest id) re-opens it IN PLACE — still a panel,
    // showing its last content, with no regenerating command run.
    feed(&rpc, "gg<CR>");
    assert!(panel_is_open(&rpc).await, "navigating stays inside a panel");
    let shown = lines(&rpc).await;
    assert!(
        shown.iter().any(|l| l.contains("msg-line")),
        "navigating to [Messages] shows its last content; got {shown:?}"
    );
}
