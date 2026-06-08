//! The in-window directory listing — nxvim's file explorer (vim's netrw).
//!
//! Opening a directory (at startup or via `:e dir`) lists its entries in a
//! read-only buffer; `<CR>` opens the entry under the cursor and `-` goes up.
//! These tests drive it the way a user would and assert on the listing buffer and
//! on the file that gets opened.

use crate::support::*;

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
    assert_eq!(
        lines(&rpc).await,
        vec!["../", "sub/", "alpha.txt", "beta.txt"],
    );
}

#[tokio::test]
async fn enter_on_a_file_opens_it() {
    // `<CR>` on a file row reads it into a buffer. Listing rows: 0=`../`, 1=`sub/`,
    // 2=`alpha.txt` — so `jj<CR>` opens `alpha.txt`.
    let dir = fixture_dir("explore_openfile");
    let (rpc, _incoming) = start(Some(dir)).await;
    feed(&rpc, "jj<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha-body"]);
}

#[tokio::test]
async fn enter_on_a_subdirectory_descends_in_place() {
    // `<CR>` on `sub/` (row 1) re-lists that directory in the same window.
    let dir = fixture_dir("explore_descend");
    let (rpc, _incoming) = start(Some(dir)).await;
    feed(&rpc, "j<CR>");
    assert_eq!(lines(&rpc).await, vec!["../", "inner.txt"]);
}

#[tokio::test]
async fn dash_goes_up_to_the_parent() {
    // Descend into `sub/`, then `-` lists the parent again.
    let dir = fixture_dir("explore_up");
    let (rpc, _incoming) = start(Some(dir)).await;
    feed(&rpc, "j<CR>");
    assert_eq!(lines(&rpc).await, vec!["../", "inner.txt"]);
    feed(&rpc, "-");
    assert_eq!(
        lines(&rpc).await,
        vec!["../", "sub/", "alpha.txt", "beta.txt"],
    );
}

#[tokio::test]
async fn enter_on_dotdot_goes_up() {
    // `<CR>` on the `../` row is the same as `-`.
    let dir = fixture_dir("explore_dotdot");
    let (rpc, _incoming) = start(Some(dir)).await;
    feed(&rpc, "j<CR>"); // into sub/
    assert_eq!(lines(&rpc).await, vec!["../", "inner.txt"]);
    feed(&rpc, "gg<CR>"); // cursor to `../`, open it
    assert_eq!(
        lines(&rpc).await,
        vec!["../", "sub/", "alpha.txt", "beta.txt"],
    );
}

#[tokio::test]
async fn editing_keys_cannot_corrupt_the_listing() {
    // The listing is effectively `nomodifiable`: delete/insert keys are inert, so
    // it stays a faithful picture of the directory.
    let dir = fixture_dir("explore_readonly");
    let (rpc, _incoming) = start(Some(dir)).await;
    let before = lines(&rpc).await;
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
    assert_eq!(
        lines(&rpc).await,
        vec!["../", "sub/", "alpha.txt", "beta.txt"],
    );
}

#[tokio::test]
async fn opening_a_file_from_the_explorer_destroys_the_listing() {
    // The explorer is a transient picker: opening a file from it wipes the listing
    // buffer, so it doesn't linger in the buffer list or as the alternate.
    let dir = fixture_dir("explore_wipe");
    let (rpc, _incoming) = start(Some(dir)).await;

    // Before: exactly one buffer — the directory listing.
    assert_eq!(buf_count(&rpc).await, 1);

    feed(&rpc, "jj<CR>"); // open alpha.txt
    assert_eq!(lines(&rpc).await, vec!["alpha-body"]);

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
    assert_eq!(lines(&rpc).await, vec!["../"]);
}
