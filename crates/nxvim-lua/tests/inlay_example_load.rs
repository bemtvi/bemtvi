//! Load coverage for the shipped `examples/inlay-hints/init.lua` — the user-facing
//! entry point for the inlay-hint Phase 2 surface. It must `exec` cleanly against
//! nxvim's real `vim.*`/`vim.lsp.*` surface (config a server, branch on
//! `server_capabilities.inlayHintProvider`, register its `<leader>i*` maps, link
//! the `LspInlayHint` group) with no external plugins, so unlike `telescope_load`
//! it never skips. Guards the example against bitrot as the API moves.

use nxvim_lua::LuaRuntime;
use std::path::PathBuf;

#[test]
fn inlay_example_init_loads() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let init = std::fs::read_to_string(repo.join("examples/inlay-hints/init.lua"))
        .expect("read example init.lua");

    let rt = LuaRuntime::new(vec![]).expect("runtime");
    rt.set_buf_snapshot(1, "/tmp/scratch.lua", "lua").unwrap();
    rt.exec(&init)
        .expect("example init.lua loads without error");

    // The toggle + get keymaps (`<leader>ih`, `<leader>ic`) should be registered.
    let maps = rt
        .eval_to_value("return #(vim._keymaps or {})")
        .expect("count maps");
    assert!(
        maps.as_i64().unwrap_or(0) >= 2,
        "example should register its <leader>i* maps, found {maps:?}"
    );

    // `vim.lsp.inlay_hint.get` is the Phase 2 read surface the example calls; with
    // no server attached it must return an empty list (not error / nil-index).
    let got = rt
        .eval_to_value("return #vim.lsp.inlay_hint.get({ bufnr = 1 })")
        .expect("inlay_hint.get is callable");
    assert_eq!(
        got.as_i64(),
        Some(0),
        "get returns an empty list with no hints"
    );
}
