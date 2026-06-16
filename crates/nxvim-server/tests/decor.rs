//! Behavior tests for `nx.decor` — viewport-scoped decoration providers
//! (`docs/specs/2026-06-11-native-plugin-api.md` §6;
//! `docs/plans/2026-06-15-nx-decor-viewport-decorations.md`).
//!
//! Phase 2 (this file): the provider registry + the off-tick dispatch + the `ctx`
//! snapshot. A provider records the `ctx` it was handed into a Lua global; the test
//! drives scrolling over the same msgpack-RPC a UI uses and reads the global back
//! through `nvim_exec_lua` — proving the provider is dispatched off the viewport
//! signal, that the snapshot tracks the visible range (top advances on scroll), and
//! that the `bufs.filetype` filter skips non-matching buffers. Marks the provider
//! publishes are recorded Lua-side but not yet rendered — that is Phase 3.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, lua_u64, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// An `init.lua` registering a `lua`-scoped probe provider (records its `ctx` into a
/// global) and a `rust`-scoped one (sets a flag if it ever runs — it must not, on a
/// `lua` buffer). Each also `publish`es a mark, exercising the publish path.
const PROBE_INIT: &str = r#"
_G.probe = nil
_G.rust_ran = false
nx.decor.provider {
  name = "probe",
  bufs = { filetype = { "lua" } },
  on_range = function(ctx, publish)
    _G.probe = { top = ctx.top, bot = ctx.bot, n = #ctx.lines, ft = ctx.filetype, gen = ctx.gen }
    publish({ { ctx.top, 0, end_col = 1, hl = "Comment" } })
  end,
}
nx.decor.provider {
  name = "rust_only",
  bufs = { filetype = { "rust" } },
  on_range = function(_ctx, _publish)
    _G.rust_ran = true
  end,
}
"#;

/// Write a `.lua` file with `n` numbered lines, in `dir`, and return its path.
fn write_big_lua(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
    let body: String = (0..n)
        .map(|i| format!("local x{i} = {{ {i} }}\n"))
        .collect();
    let path = dir.join("big.lua");
    std::fs::write(&path, body).expect("write big.lua");
    path
}

#[tokio::test]
async fn provider_is_dispatched_with_the_visible_slice_and_tracks_scroll() {
    let dir = temp_dir("decor_scroll");
    let (rpc, _incoming) = start(&dir, PROBE_INIT).await;
    let path = write_big_lua(&dir, 200);

    // Open the file — switching to the `lua` buffer is a viewport change, so the
    // provider runs with the freshly-visible top-of-file slice.
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    let top0 = lua_u64(&rpc, "return _G.probe and _G.probe.top").await;
    assert_eq!(top0, Some(0), "first dispatch sees the top of the file");
    let ft = exec_lua(&rpc, "return _G.probe.ft").await;
    assert_eq!(
        ft.as_str(),
        Some("lua"),
        "ctx.filetype is the buffer filetype"
    );
    let n = lua_u64(&rpc, "return _G.probe.n").await.unwrap();
    let bot0 = lua_u64(&rpc, "return _G.probe.bot").await.unwrap();
    // `lines` is exactly the [top, bot] slice — a full screen, well short of 200.
    assert_eq!(n, bot0 + 1, "ctx.lines covers exactly top..=bot");
    assert!(
        (10..200).contains(&n),
        "a viewport-sized slice, not the whole buffer: {n}"
    );

    // The `rust`-scoped provider never fires on a `lua` buffer (the bufs filter).
    assert_eq!(
        exec_lua(&rpc, "return _G.rust_ran").await.as_bool(),
        Some(false),
        "a filetype-scoped provider skips non-matching buffers"
    );

    // Jump to the bottom: the viewport scrolls, so the provider re-runs with an
    // advanced top reflecting the new visible range.
    feed(&rpc, "G");
    let top1 = lua_u64(&rpc, "return _G.probe.top").await.unwrap();
    assert!(
        top1 > 0,
        "scrolling re-dispatches with the moved viewport: {top1}"
    );
}

#[tokio::test]
async fn publish_records_normalized_marks() {
    let dir = temp_dir("decor_publish");
    let (rpc, _incoming) = start(&dir, PROBE_INIT).await;
    let path = write_big_lua(&dir, 50);
    feed(&rpc, &format!(":e {}<CR>", path.display()));

    // The probe publishes one mark per dispatch — recorded Lua-side (Phase 2). It is
    // normalized into the canonical positional→named form the extmark layer takes.
    let row = lua_u64(&rpc, "return nx._decor.last.marks[1].row").await;
    assert_eq!(row, Some(0), "positional row survives normalization");
    let end_col = lua_u64(&rpc, "return nx._decor.last.marks[1].end_col").await;
    assert_eq!(end_col, Some(1), "named end_col is carried through");
    let hl = exec_lua(&rpc, "return nx._decor.last.marks[1].hl").await;
    assert_eq!(
        hl.as_str(),
        Some("Comment"),
        "the hl group is carried through"
    );
}
