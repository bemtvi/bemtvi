//! End-to-end coverage for a custom `'tabline'`, driven the way a real client
//! drives the editor (black-box RPC). Boots the shipped `examples/tabline/`
//! config — whose `init.lua` sets `tabline = '%!v:lua.require("myutils")
//! .my_tab_line()'` over the sample buffer — and asserts on the styled tabline
//! row the server renders into the `redraw` map's `tabline_segments`.
//!
//! This exercises the whole path at once: the `'tabline'` option, the `%!` whole
//! re-parse, the nested `%{v:lua…my_tab_label(i)}` eval, the `%nT`/`%T`/`%999X`
//! tab click-region items (rendered to nothing), and the Lua surface a real
//! tabline reaches — `vim.fn.tabpagenr` / `tabpagebuflist` / `bufname`,
//! `vim.bo[n].modified`, `vim.split`, and `vim.spairs`. It also guards the example
//! against bitrot.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{barrier, feed, map_get, start_attached};
use rmpv::Value;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;

fn example_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/tabline"
    ))
    .canonicalize()
    .expect("examples/tabline exists")
}

/// Start a server that sources `examples/tabline/init.lua` over `file`, with the
/// example on the runtimepath so `require('myutils')` resolves `lua/myutils.lua`.
async fn start(file: PathBuf) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = example_dir();
    start_attached(
        ServerInit {
            file: Some(file.to_string_lossy().into_owned()),
            config_dir: Some(dir.clone()),
            runtimepath: vec![dir],
            ..Default::default()
        },
        80,
        22,
    )
    .await
}

/// The concatenated text of a redraw map's `tabline_segments` (the rendered custom
/// tabline row), or `None` when the frame carries no custom tabline (`Nil`).
fn tabline_text(map: &[(Value, Value)]) -> Option<String> {
    match map_get(map, "tabline_segments") {
        Some(Value::Array(segs)) => Some(
            segs.iter()
                .filter_map(|s| match s {
                    Value::Map(m) => map_get(m, "text").and_then(Value::as_str),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Drain queued redraws and return the freshest one's tabline text (take-latest,
/// per CLAUDE.md, so a stale startup/barrier frame never leaks in under load).
fn drain_to_latest_tabline(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Option<String>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            if let Some(Value::Map(map)) = params.into_iter().next() {
                latest = Some(tabline_text(&map));
            }
        }
    }
    latest
}

/// Feed `keys`, settle, and return the freshest custom-tabline text.
async fn tabline_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Option<String> {
    while incoming.try_recv().is_ok() {} // discard earlier frames
    feed(rpc, keys);
    barrier(rpc).await;
    if let Some(t) = drain_to_latest_tabline(incoming) {
        return t;
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(t) = drain_to_latest_tabline(incoming) {
            return t;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Create a uniquely-named scratch file so the second tab's label is recognisable
/// and the test never touches a checked-in file.
fn scratch_file() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push("scratch_tabline.md");
    std::fs::write(&p, "scratch\n").expect("write scratch file");
    p
}

#[tokio::test]
async fn custom_tabline_renders_one_tab_then_two_with_close_and_modified() {
    let sample = example_dir().join("sample.txt");
    let (rpc, mut incoming) = start(sample).await;

    // One tab: the example forces showtabline=2, so the custom line shows even
    // with a single tab. The label comes from my_tab_label(1) reading the buffer.
    let one = tabline_after(&rpc, &mut incoming, "<Esc>")
        .await
        .expect("a custom tabline renders with one tab (showtabline=2)");
    assert!(
        one.contains("1:sample.txt"),
        "single-tab label is the sample file (tabpagebuflist + bufname + split), got {one:?}"
    );
    assert!(
        !one.contains("close"),
        "no %999X close region with a single tab, got {one:?}"
    );

    // Open a second tab on a distinctly-named scratch file.
    let scratch = scratch_file();
    let two = tabline_after(
        &rpc,
        &mut incoming,
        &format!(":tabedit {}<CR>", scratch.display()),
    )
    .await
    .expect("the custom tabline still renders with two tabs");
    assert!(
        two.contains("1:sample.txt"),
        "tab 1's label survives, got {two:?}"
    );
    assert!(
        two.contains("2:scratch_tabline.md"),
        "tab 2's label is the scratch file (tabpagenr loop + per-tab my_tab_label), got {two:?}"
    );
    assert!(
        two.contains("close"),
        "the %=…%999Xclose region appears once there are >1 tabs, got {two:?}"
    );
    assert!(
        !two.contains("%999X") && !two.contains("%2T") && !two.contains("%T"),
        "the %nT / %T / %999X click-region markers render to nothing, got {two:?}"
    );

    // Edit tab 2's buffer (in-memory only): vim.bo[bufnr].modified flips, so the
    // label gains the `*` my_tab_label appends.
    let edited = tabline_after(&rpc, &mut incoming, "ix<Esc>").await.unwrap();
    assert!(
        edited.contains("2:scratch_tabline.md*"),
        "an edited tab's label gains a `*` from vim.bo[n].modified, got {edited:?}"
    );

    let _ = std::fs::remove_file(scratch_file());
}
