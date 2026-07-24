//! Behavior tests for the `BufReadCmd` autocmd — vim's "replace the default file
//! read" hook (Primitive B of the explorer Lua-port plan,
//! `docs/plans/2026-06-25-explorer-lua-port.md`). A registered handler can **claim**
//! a buffer read and fill the buffer itself (returning a truthy value); an unclaimed
//! read falls through to the editor's normal load. Driven black-box over RPC: a
//! handler in `init.lua` claims paths by a marker, and we assert on the resulting
//! buffer lines.

use nxvim_test_harness::{command, exec_lua, lines, start_with_config, temp_dir, write_temp};

/// A `BufReadCmd` handler scoped to `*` that claims any path ending in `.special`
/// (filling the buffer with a sentinel) and **declines** everything else (returning
/// nil, so the default read runs). The flagship shape: one `*` handler deciding per
/// path — exactly how the explorer-as-plugin will claim directories but not files.
const READCMD_INIT: &str = r#"
nx.autocmd.create("BufReadCmd", {
  pattern = "*",
  callback = function(args)
    if args.file:sub(-#".special") == ".special" then
      vim.api.nvim_buf_set_lines(args.buf, 0, -1, false, { "CLAIMED", args.file })
      return true
    end
    -- A regular file: decline, so the editor's default read fills the buffer.
  end,
})
"#;

#[tokio::test]
async fn a_handler_claims_a_matching_read() {
    // `:e <something>.special` is claimed by the handler, which fills the buffer
    // itself — the default read never runs (the path need not even exist on disk).
    let dir = temp_dir("readcmd_claim");
    let (rpc, _incoming) = start_with_config(&dir, READCMD_INIT).await;
    let path = dir.join("ghost.special");
    command(&rpc, &format!("edit {}", path.display())).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["CLAIMED".to_string(), path.display().to_string()],
        "the BufReadCmd handler should own the buffer content"
    );
}

#[tokio::test]
async fn an_unclaimed_read_falls_through_to_the_default() {
    // A regular file the handler declines is read normally, even though a BufReadCmd
    // handler is registered (it returned nil for this path).
    let dir = temp_dir("readcmd_decline");
    let (rpc, _incoming) = start_with_config(&dir, READCMD_INIT).await;
    let path = write_temp("readcmd_plain", "txt", "real-line-one\nreal-line-two");
    command(&rpc, &format!("edit {path}")).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["real-line-one".to_string(), "real-line-two".to_string()],
        "a declined read must fall through to the default file load"
    );
}

#[tokio::test]
async fn the_claimed_buffer_is_named_for_the_path() {
    // The claimed buffer carries the opened path as its name (the handler got it as
    // `args.file`), so `:ls` / the statusline read the directory/file the user asked
    // for — the same identity a normal open would have.
    let dir = temp_dir("readcmd_named");
    let (rpc, _incoming) = start_with_config(&dir, READCMD_INIT).await;
    let path = dir.join("doc.special");
    command(&rpc, &format!("edit {}", path.display())).await;
    let name = exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)").await;
    assert_eq!(
        name.as_str(),
        Some(path.display().to_string().as_str()),
        "the claimed buffer keeps the opened path as its name"
    );
}
