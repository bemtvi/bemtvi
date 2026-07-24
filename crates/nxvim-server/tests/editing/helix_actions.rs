//! Helix editing model — Phase 5: the named-action registry + keymap plugin.
//!
//! Phase 5 exposes every Helix verb *by name* through `nx._helix_action` ->
//! `Editor::apply_helix_action` (the seam the bundled `prelude/helix.lua` plugin
//! binds), and gives the Helix modes their own `helix` keymap bucket (`'h'`) so the
//! plugin can layer the goto/space menus and insert-entry keys on top of the native
//! `handle_helix` grammar. These tests drive both ends: the registry directly (via
//! `nx._helix_action`) and the plugin's default `helix`-bucket maps (via fed keys),
//! plus the bucket's fall-through to the native grammar.

use crate::support::*;

/// The named-action registry drives verbs directly: `enable_helix` turns the model
/// on, and a subsequent named verb (`select_all`) acts on the live selection set.
#[tokio::test]
async fn registry_enables_and_drives_verbs() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc def<Esc>0");
    assert_eq!(mode(&rpc).await, "n", "starts in vim normal");

    // `enable_helix` then `select_all`, drained in order in one chunk.
    exec_lua(
        &rpc,
        "nx._helix_action('enable_helix'); nx._helix_action('select_all')",
    )
    .await;
    assert_eq!(mode(&rpc).await, "hn", "enable_helix entered Helix normal");
    // `cursor()` is 1-based line, 0-based col — the head sits on `f` (col 6).
    assert_eq!(
        cursor(&rpc).await,
        (1, 6),
        "select_all put the head on the last char"
    );
}

/// An unknown action name **fails loud** (per the no-silent-stubs rule): the server
/// surfaces an `E5108` rather than silently ignoring it.
#[tokio::test]
async fn unknown_action_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>0:helix<CR>");
    exec_lua(&rpc, "nx._helix_action('does_not_exist')").await;
    let map = redraw_after_matching(&rpc, &mut incoming, "", |m| {
        message(m).contains("does_not_exist")
    })
    .await;
    assert!(
        message(&map).contains("E5108"),
        "unknown action echoed E5108, got {:?}",
        message(&map)
    );
}

/// The plugin's `helix`-bucket insert-entry maps fire: `a` (append_mode) opens Insert
/// one char past the selection, and `<Esc>` resumes Helix normal.
#[tokio::test]
async fn plugin_insert_entry_append() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<Esc>0:helix<CR>");
    // Point selection on `h`; `a` appends after it (col 1), then type `X`.
    feed(&rpc, "aX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["hXello"],
        "append inserted after the selected char"
    );
    assert_eq!(mode(&rpc).await, "hn", "<Esc> resumed Helix normal");
}

/// The plugin's goto (`g`) menu: `gg` jumps to the file start, `ge` to the last line.
#[tokio::test]
async fn plugin_goto_menu() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>:helix<CR>");
    // 1-based line: three lines → last line is 3.
    assert_eq!(cursor(&rpc).await.0, 3, "start on the last line");

    feed(&rpc, "gg");
    assert_eq!(cursor(&rpc).await, (1, 0), "gg went to the file start");

    feed(&rpc, "ge");
    assert_eq!(cursor(&rpc).await.0, 3, "ge went to the last line");
}

/// The plugin binds `u`/`U` to undo/redo — unreachable from the native grammar.
#[tokio::test]
async fn plugin_undo_redo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<Esc>0:helix<CR>");
    feed(&rpc, "aX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hXello"], "edit applied");

    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["hello"], "u undid the edit");
    assert_eq!(mode(&rpc).await, "hn", "still in Helix normal after undo");

    feed(&rpc, "U");
    assert_eq!(lines(&rpc).await, vec!["hXello"], "U redid the edit");
}

/// A key the plugin does NOT map still reaches the native `handle_helix` grammar:
/// the `helix` bucket falls through (like the multi-cursor `'m'` bucket), so `w`
/// selects the next word even with the plugin loaded.
#[tokio::test]
async fn helix_bucket_falls_through_to_native() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    // `w` is unmapped by the plugin — the native word-motion selects `hello `.
    let map = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "native `w` moved the head");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "native `w` selected the word and its trailing space"
    );
}

/// `nx.helix.enable()` / `disable()` are the public opt-in surface, idempotent both
/// ways — the plugin's wrapper over the registry.
#[tokio::test]
async fn public_enable_disable_toggle() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<Esc>0");
    exec_lua(&rpc, "nx.helix.enable(); nx.helix.enable()").await;
    assert_eq!(
        mode(&rpc).await,
        "hn",
        "enable is idempotent → Helix normal"
    );
    exec_lua(&rpc, "nx.helix.disable()").await;
    assert_eq!(mode(&rpc).await, "n", "disable returned to vim normal");
}

/// The plugin binds `]d`/`[d` in the `helix` bucket, so diagnostic navigation works
/// in Helix mode exactly like the vim-mode defaults.
#[tokio::test]
async fn bracket_d_navigates_diagnostics_in_helix() {
    let path = write_n_lines("hxdiag", 7);
    let (rpc, _i) = start(Some(path)).await;
    exec_lua(
        &rpc,
        r#"
        local E = nx.diagnostic.severity.ERROR
        nx.diagnostic.set(nx.ns.create("hxtest"), 0, {
          { lnum = 1, col = 0, message = "err one", severity = E },
          { lnum = 5, col = 0, message = "err two", severity = E },
        })
        return true
        "#,
    )
    .await;
    feed(&rpc, ":helix<CR>");

    // Cursor starts on line 1; `]d` walks to the diagnostics on 0-based lines 1, 5.
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 2, "`]d` -> first diagnostic");
    feed(&rpc, "]d");
    assert_eq!(cursor(&rpc).await.0, 6, "`]d` -> second diagnostic");
    feed(&rpc, "[d");
    assert_eq!(cursor(&rpc).await.0, 2, "`[d` -> back to the first");
}
