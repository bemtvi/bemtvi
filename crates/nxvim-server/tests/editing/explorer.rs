//! The in-window directory listing — nxvim's file explorer (vim's netrw).
//!
//! The explorer is a pure-Lua plugin (`prelude/explorer.lua`, on by default): opening a
//! directory (at startup or via `:e dir`) claims the read through `BufReadCmd` and fills
//! a read-only listing buffer over `nx.fs`; `<CR>` opens the entry under the cursor and
//! `-` goes up. These tests drive it the way a user would and assert on the listing
//! buffer and on the file that gets opened.
//!
//! Because the fill is asynchronous (`nx.fs.readdir` settles off the editor tick), the
//! tests `await_lines` for the listing to appear and, crucially, wait for it *before*
//! feeding navigation keys (which act on the filled buffer).

use crate::support::*;
use std::time::Duration;

/// A fresh temp directory pre-populated with two files and one sub-directory
/// (itself holding one file). Returns the directory's path as a string.
fn fixture_dir(tag: &str) -> String {
    let dir = temp_dir(tag);
    std::fs::write(dir.join("alpha.txt"), "alpha-body\n").expect("write alpha");
    std::fs::write(dir.join("beta.txt"), "beta-body\n").expect("write beta");
    std::fs::create_dir(dir.join("sub")).expect("mkdir sub");
    std::fs::write(dir.join("sub").join("inner.txt"), "inner-body\n").expect("write inner");
    dir.to_string_lossy().into_owned()
}

/// Poll the current buffer's lines until they match `want` or the budget runs out — the
/// async-fill counterpart of a synchronous assert (the listing fills a tick or two after
/// the open, when `nx.fs.readdir` settles). Returns the final lines either way.
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        if lines(rpc).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lines(rpc).await
}

/// The standard fixture listing, after the async fill.
const LISTING: &[&str] = &["../", "sub/", "alpha.txt", "beta.txt"];

/// The highlight groups painted on screen `row` of the focused window, read off
/// the redraw `highlights` map (each span is `[start_col, end_col, group]`).
fn row_groups(map: &[(Value, Value)], row: usize) -> Vec<String> {
    let rows = match window0_field(map, "highlights").and_then(Value::as_array) {
        Some(rows) => rows,
        None => return vec![],
    };
    let Some(spans) = rows.get(row).and_then(Value::as_array) else {
        return vec![];
    };
    spans
        .iter()
        .filter_map(|s| {
            s.as_array()?
                .get(2)
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

/// Whether screen `row` of the focused window carries highlight group `group`.
fn row_has_group(map: &[(Value, Value)], row: usize, group: &str) -> bool {
    row_groups(map, row).iter().any(|g| g == group)
}

/// The number of open buffers (`nvim_list_bufs`).
async fn buf_count(rpc: &Rpc) -> usize {
    match rpc
        .request("nvim_list_bufs", vec![])
        .await
        .expect("list_bufs")
    {
        Value::Array(items) => items.len(),
        _ => 0,
    }
}

#[tokio::test]
async fn opening_a_directory_lists_its_entries() {
    // `nxvim <dir>` opens the explorer: a `../` up-entry, then directories first
    // (suffixed `/`), then files, each group sorted by name.
    let dir = fixture_dir("explore_list");
    let (rpc, _incoming) = start(Some(dir)).await;
    assert_eq!(await_lines(&rpc, LISTING).await, LISTING);
}

#[tokio::test]
async fn enter_on_a_file_opens_it() {
    // `<CR>` on a file row reads it into a buffer. Listing rows: 0=`../`, 1=`sub/`,
    // 2=`alpha.txt` — so `jj<CR>` opens `alpha.txt`.
    let dir = fixture_dir("explore_openfile");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    feed(&rpc, "jj<CR>");
    assert_eq!(await_lines(&rpc, &["alpha-body"]).await, vec!["alpha-body"]);
}

#[tokio::test]
async fn enter_on_a_subdirectory_descends_in_place() {
    // `<CR>` on `sub/` (row 1) re-lists that directory in the same window.
    let dir = fixture_dir("explore_descend");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    feed(&rpc, "j<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "inner.txt"]).await,
        vec!["../", "inner.txt"]
    );
}

#[tokio::test]
async fn dash_goes_up_to_the_parent() {
    // Descend into `sub/`, then `-` lists the parent again.
    let dir = fixture_dir("explore_up");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    feed(&rpc, "j<CR>");
    await_lines(&rpc, &["../", "inner.txt"]).await;
    feed(&rpc, "-");
    assert_eq!(await_lines(&rpc, LISTING).await, LISTING);
}

#[tokio::test]
async fn enter_on_dotdot_goes_up() {
    // `<CR>` on the `../` row is the same as `-`.
    let dir = fixture_dir("explore_dotdot");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    feed(&rpc, "j<CR>"); // into sub/
    await_lines(&rpc, &["../", "inner.txt"]).await;
    feed(&rpc, "gg<CR>"); // cursor to `../`, open it
    assert_eq!(await_lines(&rpc, LISTING).await, LISTING);
}

#[tokio::test]
async fn editing_keys_cannot_corrupt_the_listing() {
    // The listing is `nomodifiable`: delete/insert keys are inert, so it stays a
    // faithful picture of the directory.
    let dir = fixture_dir("explore_readonly");
    let (rpc, _incoming) = start(Some(dir)).await;
    let before = await_lines(&rpc, LISTING).await;
    feed(&rpc, "ddxggddpiHELLO<Esc>otext<Esc>");
    assert_eq!(
        lines(&rpc).await,
        before,
        "editing must not change the listing"
    );
}

#[tokio::test]
async fn j_and_k_move_the_selection() {
    // Vertical motions move the cursor through the listing without editing it.
    let dir = fixture_dir("explore_nav");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    // `nvim_win_get_cursor` is (1-based row, 0-based col); the listing rests at
    // column 0.
    feed(&rpc, "jj");
    assert_eq!(cursor(&rpc).await, (3, 0)); // row 3 = `alpha.txt`
    feed(&rpc, "k");
    assert_eq!(cursor(&rpc).await, (2, 0)); // `sub/`
    feed(&rpc, "G");
    assert_eq!(cursor(&rpc).await, (4, 0)); // `beta.txt`, last row
    feed(&rpc, "gg");
    assert_eq!(cursor(&rpc).await, (1, 0)); // `../`, first row
}

#[tokio::test]
async fn edit_command_opens_the_explorer() {
    // `:e <dir>` opens the explorer just like a startup directory argument.
    let dir = fixture_dir("explore_excmd");
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, &format!(":e {dir}<CR>"));
    assert_eq!(await_lines(&rpc, LISTING).await, LISTING);
}

#[tokio::test]
async fn opening_a_file_from_the_explorer_destroys_the_listing() {
    // The explorer is a transient picker: opening a file from it wipes the listing
    // buffer, so it doesn't linger in the buffer list or as the alternate.
    let dir = fixture_dir("explore_wipe");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;

    // Before: exactly one buffer — the directory listing.
    assert_eq!(buf_count(&rpc).await, 1);

    feed(&rpc, "jj<CR>"); // open alpha.txt
    assert_eq!(await_lines(&rpc, &["alpha-body"]).await, vec!["alpha-body"]);

    // After: still exactly one buffer — the file. The explorer is gone, not just
    // hidden, so it is neither listed nor reachable as the alternate.
    assert_eq!(buf_count(&rpc).await, 1);
    feed(&rpc, "<C-^>"); // no alternate to return to
    assert_eq!(lines(&rpc).await, vec!["alpha-body"]);
}

#[tokio::test]
async fn an_empty_directory_lists_only_the_up_entry() {
    let dir = temp_dir("explore_empty").to_string_lossy().into_owned();
    let (rpc, _incoming) = start(Some(dir)).await;
    assert_eq!(await_lines(&rpc, &["../"]).await, vec!["../"]);
}

#[tokio::test]
async fn the_listing_is_not_marked_modified() {
    // A directory listing is a read-only picture of the filesystem, never a buffer with
    // unsaved edits — so it must not carry the `[+]` modified flag (the plugin clears it
    // after filling, since the fill is a read, not an edit).
    let dir = fixture_dir("explore_modified");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.modified").await.as_bool(),
        Some(false),
        "the directory listing must not be modified"
    );
    // Descending re-lists in place; it must stay clean too.
    feed(&rpc, "j<CR>"); // into sub/
    await_lines(&rpc, &["../", "inner.txt"]).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.modified").await.as_bool(),
        Some(false),
        "a descended-into listing must not be modified either"
    );
}

#[tokio::test]
async fn directory_rows_are_highlighted() {
    // The `nx.decor` provider colours directory rows: the `../` up-entry one group,
    // sub-directories another, files none. Rows: 0=`../`, 1=`sub/`, 2=`alpha.txt`.
    let dir = fixture_dir("explore_highlight");
    let (_rpc, mut incoming) = start(Some(dir)).await;
    // The listing fills and the provider runs off-tick; wait for the frame that carries
    // the `sub/` highlight.
    let map = wait_redraw(&mut incoming, |m| row_has_group(m, 1, "NxDirDirectory")).await;
    assert!(row_has_group(&map, 0, "NxDirParent"), "../ row coloured");
    assert!(
        row_has_group(&map, 1, "NxDirDirectory"),
        "sub/ row coloured"
    );
    assert!(
        !row_has_group(&map, 2, "NxDirDirectory") && !row_has_group(&map, 2, "NxDirParent"),
        "the file row alpha.txt is left uncoloured"
    );
}

#[tokio::test]
async fn double_click_opens_a_file() {
    // A double-click on a file row opens it, the mouse form of `<CR>` (netrw). Rows:
    // 0=`../`, 1=`sub/`, 2=`alpha.txt`. A fake mouse clock makes the two presses land
    // inside `'mousetime'` deterministically.
    let dir = fixture_dir("explore_dblclick_file");
    let clock = TestClock::new();
    let (rpc, _incoming) = start_with(ServerInit {
        file: Some(dir),
        mouse_clock: Some(clock.handle()),
        ..Default::default()
    })
    .await;
    await_lines(&rpc, LISTING).await;
    // First press places the cursor on `alpha.txt` (screen row 2 → buffer line 3).
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 2, 0);
    assert_eq!(cursor(&rpc).await, (3, 0));
    // Second press 100 ms later (within the default `'mousetime'`) is the double —
    // it opens the file.
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 2, 0);
    assert_eq!(await_lines(&rpc, &["alpha-body"]).await, vec!["alpha-body"]);
}

#[tokio::test]
async fn double_click_descends_into_a_subdirectory() {
    // A double-click on `sub/` (screen row 1) descends into it, like `<CR>`.
    let dir = fixture_dir("explore_dblclick_dir");
    let clock = TestClock::new();
    let (rpc, _incoming) = start_with(ServerInit {
        file: Some(dir),
        mouse_clock: Some(clock.handle()),
        ..Default::default()
    })
    .await;
    await_lines(&rpc, LISTING).await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 1, 0);
    assert_eq!(cursor(&rpc).await, (2, 0));
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 1, 0);
    assert_eq!(
        await_lines(&rpc, &["../", "inner.txt"]).await,
        vec!["../", "inner.txt"]
    );
}

#[tokio::test]
async fn single_click_only_moves_the_cursor() {
    // A single click is plain cursor placement — it must NOT open the entry (only the
    // double-click does). Click `sub/` once and confirm the listing is unchanged.
    let dir = fixture_dir("explore_singleclick");
    let (rpc, _incoming) = start(Some(dir)).await;
    await_lines(&rpc, LISTING).await;
    feed_mouse(&rpc, "left", "press", 1, 0);
    assert_eq!(cursor(&rpc).await, (2, 0));
    assert_eq!(
        lines(&rpc).await,
        LISTING,
        "a single click must not descend"
    );
}
