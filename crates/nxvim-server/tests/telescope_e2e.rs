//! End-to-end proof that telescope.nvim actually *runs* on nxvim — not just loads.
//! Starts a real server with telescope.nvim + plenary.nvim on the runtimepath,
//! opens a picker over RPC, types into the prompt, and asserts the results list
//! filters live. This exercises the whole stack the picker needs: float windows,
//! the prompt scratch buffer, insert-mode typing, `nvim_buf_attach`'s `on_lines`
//! firing the finder, the sorter, and the results-buffer render.
//!
//! telescope/plenary are cloned (pinned) into a shared cache by the harness'
//! `clone_plugin`, so this runs against a known-good upstream revision rather than
//! the developer's local install — hermetic. It SKIPS only when the clone can't
//! happen (no `git` / no network), like the lspconfig submodule test. With the
//! plugins present it is a hard behavioral assertion.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_test_harness::{clone_plugin, exec_lua, feed, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path) -> Option<(Rpc, UnboundedReceiver<Incoming>)> {
    let telescope = clone_plugin("telescope.nvim")?;
    let plenary = clone_plugin("plenary.nvim")?;
    std::fs::write(dir.join("init.lua"), "require('telescope').setup{}\n").ok()?;
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf(), telescope, plenary],
        ..Default::default()
    };
    let (server_end, client_end) = tokio::io::duplex(1 << 18);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(120u64), Value::from(40u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
    Some((rpc, incoming))
}

/// Settle async work (plenary defers the finder through vim.schedule, drained on
/// the server's convergence each turn): send a few barriers so a chain of
/// scheduled steps runs to completion.
async fn settle(rpc: &Rpc) {
    // Interleave barriers with short real sleeps: plenary.async defers the finder
    // through vim.schedule (drained on each convergence) AND telescope debounces
    // the prompt through a timer (fired by the server's event-loop actor after real
    // time elapses), so both a barrier and elapsed wall-clock are needed.
    for _ in 0..15 {
        let _ = rpc.request("nvim_get_mode", vec![]).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let _ = rpc.request("nvim_get_mode", vec![]).await;
}

fn joined(v: &Value) -> String {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[tokio::test]
async fn telescope_picker_filters_live() {
    let dir = temp_dir("telescope");
    let Some((rpc, _incoming)) = start(&dir).await else {
        eprintln!("skip: telescope.nvim / plenary.nvim not installed in ~/.local/share/nvim/lazy");
        return;
    };

    // Open a custom table-finder picker and stash it + its results buffer so we
    // can read the filtered list back. `:find()` opens the floats and feeds `A`
    // to enter insert mode in the prompt.
    let open = exec_lua(
        &rpc,
        r#"
        local pickers = require('telescope.pickers')
        local finders = require('telescope.finders')
        local conf = require('telescope.config').values
        _G.__pick = pickers.new({}, {
          prompt_title = 'e2e',
          finder = finders.new_table({ results = { 'apple', 'banana', 'cherry', 'blueberry' } }),
          sorter = conf.generic_sorter({}),
        })
        _G.__pick:find()
        return _G.__pick.results_bufnr
        "#,
    )
    .await;
    let results_bufnr = open.as_u64().expect("results_bufnr returned") as i64;
    assert!(results_bufnr > 0, "picker should report a results buffer");

    settle(&rpc).await;

    // Read the results buffer directly (it holds the rendered, sorted entries).
    let read = format!("return vim.api.nvim_buf_get_lines({results_bufnr}, 0, -1, false)");
    let initial = joined(&exec_lua(&rpc, &read).await);
    assert!(
        initial.contains("apple") && initial.contains("banana") && initial.contains("cherry"),
        "all entries should show before filtering, got:\n{initial}"
    );

    // Type "ban" into the prompt. This inserts into the prompt buffer, fires
    // on_lines, re-runs the finder, and re-renders the results.
    feed(&rpc, "ban");
    settle(&rpc).await;

    let prompt = exec_lua(&rpc, "return _G.__pick:_get_prompt()").await;
    assert_eq!(
        prompt.as_str(),
        Some("ban"),
        "the typed query should reach the prompt via insert-mode typing + buffer edits"
    );

    // The entry manager should have narrowed to the single fuzzy match.
    let num = exec_lua(
        &rpc,
        "return _G.__pick.manager and _G.__pick.manager:num_results() or -1",
    )
    .await;
    assert_eq!(
        num.as_i64(),
        Some(1),
        "the finder should re-run on type and keep only the matching entry"
    );

    let filtered = joined(&exec_lua(&rpc, &read).await);
    assert!(
        filtered.contains("banana"),
        "the matching entry should survive the filter, got:\n{filtered}"
    );
    assert!(
        !filtered.contains("cherry") && !filtered.contains("apple"),
        "non-matching entries should be filtered out, got:\n{filtered}"
    );
}
