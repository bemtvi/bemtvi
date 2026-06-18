//! The `gc`/`gcc` comment operator and `'commentstring'`.
//!
//! Driven black-box like the rest of the suite: feed vim keys, assert on buffer
//! lines. The comment template comes either from the file's filetype default
//! (open a `foo.rs` / `foo.py` / `foo.lua`) or an explicit `:set commentstring` /
//! `nx.bo.commentstring`.

use crate::support::*;

/// A unique temp path with the given extension, so the buffer picks up the
/// matching filetype (and thus its default `'commentstring'`).
fn temp_with_ext(tag: &str, ext: &str) -> String {
    temp_path(tag)
        .with_extension(ext)
        .to_string_lossy()
        .into_owned()
}

/// Open a fresh server on a file of `ext` seeded with `content`.
async fn start_file(tag: &str, ext: &str, content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = temp_with_ext(tag, ext);
    std::fs::write(&path, content).expect("write temp file");
    start(Some(path)).await
}

#[tokio::test]
async fn gcc_comments_the_current_line_with_the_filetype_default() {
    // A rust file: `'commentstring'` defaults to `// %s`.
    let (rpc, _incoming) = start_file("gcc_rust", "rs", "let x = 1;\n").await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["// let x = 1;"]);
}

#[tokio::test]
async fn gcc_toggles_back_to_uncommented() {
    let (rpc, _incoming) = start_file("gcc_toggle", "rs", "let x = 1;\n").await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["// let x = 1;"]);
    // A second `gcc` on the now-commented line uncomments it back to the original.
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["let x = 1;"]);
}

#[tokio::test]
async fn gc_with_a_motion_comments_a_range() {
    let (rpc, _incoming) = start_file("gc_motion", "rs", "a;\nb;\nc;\n").await;
    // `gcj` = comment this line and the next (linewise over the `j` motion).
    feed(&rpc, "gcj");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// b;", "c;"]);
}

#[tokio::test]
async fn count_before_gcc_comments_several_lines() {
    let (rpc, _incoming) = start_file("gcc_count", "rs", "a;\nb;\nc;\n").await;
    feed(&rpc, "3gcc");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// b;", "// c;"]);
}

#[tokio::test]
async fn gc_aligns_to_the_minimum_indent_and_keeps_relative_indent() {
    // Indented block: the comment marker goes at the block's *minimum* indent, and
    // each line keeps its own indentation past that point.
    let src = "fn main() {\n    let x = 1;\n        let y = 2;\n}\n";
    let (rpc, _incoming) = start_file("gc_indent", "rs", src).await;
    // Select the two inner lines and comment them (min indent = 4 spaces).
    feed(&rpc, "jVjgc");
    assert_eq!(
        lines(&rpc).await,
        vec![
            "fn main() {",
            "    // let x = 1;",
            "    //     let y = 2;",
            "}",
        ],
    );
}

#[tokio::test]
async fn gc_on_a_mixed_block_comments_every_line() {
    // Neovim rule: a block is uncommented only when *every* non-blank line is
    // already commented. A mix → comment them all (so the second line ends up
    // double-commented), not toggle each.
    let (rpc, _incoming) = start_file("gc_mixed", "rs", "a;\n// b;\n").await;
    feed(&rpc, "Vjgc");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// // b;"]);
}

#[tokio::test]
async fn gc_skips_blank_lines_when_deciding_and_blanks_get_bare_marker() {
    // A blank line inside the range doesn't affect the commented/uncommented
    // decision, and is commented with just the trimmed marker (no trailing space).
    let (rpc, _incoming) = start_file("gc_blank", "rs", "a;\n\nb;\n").await;
    feed(&rpc, "VGgc");
    assert_eq!(lines(&rpc).await, vec!["// a;", "//", "// b;"]);
}

#[tokio::test]
async fn gcip_comments_a_paragraph_text_object() {
    let (rpc, _incoming) = start_file("gcip", "rs", "a;\nb;\n\nc;\n").await;
    // `gcip` = comment the inner paragraph (the first two lines).
    feed(&rpc, "gcip");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// b;", "", "c;"]);
}

#[tokio::test]
async fn visual_gc_comments_the_selection() {
    let (rpc, _incoming) = start_file("vis_gc", "rs", "a;\nb;\nc;\n").await;
    feed(&rpc, "VG");
    feed(&rpc, "gc");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// b;", "// c;"]);
}

#[tokio::test]
async fn gcc_dot_repeats() {
    let (rpc, _incoming) = start_file("gcc_dot", "rs", "a;\nb;\nc;\n").await;
    feed(&rpc, "gcc");
    // `.` replays the whole `gcc` change on the next line.
    feed(&rpc, "j.");
    assert_eq!(lines(&rpc).await, vec!["// a;", "// b;", "c;"]);
}

#[tokio::test]
async fn python_uses_a_hash_comment() {
    let (rpc, _incoming) = start_file("py_gc", "py", "x = 1\n").await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["# x = 1"]);
}

#[tokio::test]
async fn lua_uses_a_dash_comment() {
    let (rpc, _incoming) = start_file("lua_gc", "lua", "local x = 1\n").await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["-- local x = 1"]);
}

#[tokio::test]
async fn block_commentstring_wraps_on_both_sides() {
    // A CSS file uses the block form `/* %s */`: the marker brackets the line.
    let (rpc, _incoming) = start_file("css_gc", "css", "a { color: red; }\n").await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["/* a { color: red; } */"]);
    // And it round-trips: a second toggle strips both sides.
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["a { color: red; }"]);
}

#[tokio::test]
async fn explicit_commentstring_override_via_nx_bo() {
    // A plain `.txt` buffer has no filetype default; set one through `nx.bo`.
    let (rpc, _incoming) = start_file("ovr_bo", "txt", "hello\n").await;
    exec_lua(&rpc, "nx.bo.commentstring = '// %s'").await;
    // The override reads back through the mirror.
    assert_eq!(
        exec_lua(&rpc, "return nx.bo.commentstring").await.as_str(),
        Some("// %s"),
    );
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["// hello"]);
}

#[tokio::test]
async fn set_commentstring_ex_command_drives_gc() {
    let (rpc, _incoming) = start_file("ovr_set", "txt", "hello\n").await;
    // The ex `:set` path (no spaces, so no escaping needed): left `//`, no right.
    feed(&rpc, ":set commentstring=//%s<CR>");
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["//hello"]);
}

#[tokio::test]
async fn nx_bo_reflects_the_filetype_default() {
    // With no explicit override, `nx.bo.commentstring` reports the filetype default.
    let (rpc, _incoming) = start_file("bo_default", "rs", "x\n").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.bo.commentstring").await.as_str(),
        Some("// %s"),
    );
}

#[tokio::test]
async fn filetype_autocmd_can_override_commentstring() {
    // Mirrors the `examples/commenting` config: a `FileType` autocmd sets a custom
    // template, and `gcc` then uses it.
    let dir = temp_dir("cms_ft");
    std::fs::write(
        dir.join("init.lua"),
        "vim.api.nvim_create_autocmd('FileType', {\n\
           pattern = 'bash',\n\
           callback = function(args) vim.bo[args.buf].commentstring = '#  %s' end,\n\
         })\n",
    )
    .expect("init.lua");
    let file = temp_with_ext("cms_ft_buf", "sh");
    std::fs::write(&file, "echo hi\n").expect("sh file");
    let (rpc, _incoming) = start_with(ServerInit {
        file: Some(file),
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    })
    .await;
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["#  echo hi"]);
}

#[tokio::test]
async fn a_string_rhs_keymap_can_drive_the_comment_operator() {
    // The example maps a key to the literal `"gcc"`; the remap must replay into the
    // built-in comment operator.
    let (rpc, _incoming) = start_file("kmap_gc", "rs", "let x = 1;\n").await;
    exec_lua(&rpc, "nx.keymap.set('n', ',c', 'gcc')").await;
    feed(&rpc, ",c");
    assert_eq!(lines(&rpc).await, vec!["// let x = 1;"]);
}

#[tokio::test]
async fn gc_without_a_commentstring_leaves_the_buffer_unchanged() {
    // A plain buffer with no filetype and no override has no template — `gc` warns
    // and changes nothing rather than mangling the line.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "gcc");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
}
