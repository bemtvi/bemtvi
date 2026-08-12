//! The query-resolution bridge, buffer-open half: a runtimepath `after/queries`
//! overlay with a `;; extends` modeline must *add* its captures to the language's
//! base highlight query (not replace it), and those captures must reach the paint.
//!
//! On first highlight of a buffer, the server resolves the language's runtimepath
//! queries (`queries/<lang>/*.scm` + `after/queries/<lang>/*.scm`) onto the
//! engine's bundled base and installs the merge. This test drops an overlay that
//! captures a node the base query does not name, opens a file, and asserts the
//! custom capture shows up in the redraw `highlights` payload — proving the overlay
//! both merged and painted.
//!
//! `#[ignore]`d, not hermetic: it installs a real grammar into a temp data dir,
//! which needs network + a C compiler — the same opt-in posture as the other
//! treesitter / PTY e2e tests. Run with:
//!
//! ```sh
//! cargo test -p bemtvi-server --test treesitter_query_bridge -- --ignored --nocapture
//! ```

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;

/// True if any row of the `highlights` payload paints a span whose capture group is
/// `group`.
fn payload_has_group(map: &[(Value, Value)], group: &str) -> bool {
    window0_field(map, "highlights")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter().filter_map(Value::as_array).any(|cols| {
                cols.iter()
                    .filter_map(Value::as_array)
                    .any(|span| span.get(2).and_then(Value::as_str) == Some(group))
            })
        })
}

#[tokio::test]
#[ignore = "needs network + a C compiler to install a real grammar; opt-in like the other ts e2e tests"]
async fn after_queries_extends_adds_capture_to_paint() {
    // The server resolves grammars from `BEMTVI_DATA_DIR` (process-global); serialize
    // against other tests that touch process-wide state.
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_bridge_data");
    bemtvi_ts::install::install(&data, "json")
        .expect("install json grammar (network + C compiler required)");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    // A runtimepath dir carrying an `after/queries` overlay that captures JSON
    // numbers as a group the base query never emits.
    let rtp = temp_dir("ts_bridge_rtp");
    let qdir = rtp.join("after/queries/json");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(
        qdir.join("highlights.scm"),
        ";; extends\n(number) @catppuccin.test\n",
    )
    .unwrap();

    let file = write_temp("ts_bridge", "json", "{\"a\": 123}\n");
    let (_rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    // The custom capture from the `;; extends` overlay must reach the paint.
    let map = wait_redraw(&mut incoming, |m| payload_has_group(m, "catppuccin.test")).await;
    assert!(
        payload_has_group(&map, "catppuccin.test"),
        "the `;; extends` overlay capture should paint"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}

#[tokio::test]
#[ignore = "needs network + a C compiler to install real grammars; opt-in like the other ts e2e tests"]
async fn inherits_pulls_runtimepath_query_of_inherited_lang() {
    // The javascript grammar's bundled `injections.scm` is just `; inherits: ecma,jsx`
    // — the real query lives in `ecma`. A config `queries/ecma/injections.scm` must
    // therefore reach a `.js` buffer *through* `; inherits:` resolution. This injects
    // `json` into JS string contents (a node base ecma never injects), so a json
    // `@number` inside the string proves the inherited overlay resolved and painted.
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_inherits_data");
    bemtvi_ts::install::install(&data, "javascript").expect("install javascript");
    bemtvi_ts::install::install(&data, "json").expect("install json");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    let rtp = temp_dir("ts_inherits_rtp");
    let qdir = rtp.join("queries/ecma");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(
        qdir.join("injections.scm"),
        "((string_fragment) @injection.content (#set! injection.language \"json\"))\n",
    )
    .unwrap();

    let file = write_temp("ts_inherits", "js", "const x = \"42\";\n");
    let (_rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    let map = wait_redraw(&mut incoming, |m| payload_has_group(m, "number")).await;
    assert!(
        payload_has_group(&map, "number"),
        "a `queries/ecma/injections.scm` overlay should reach a js buffer via `; inherits:`"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}

/// An overlay must reach a language the buffer **injects**, not only the language
/// the buffer *is*.
///
/// Resolution used to key off `ts_language_for(buffer)` — a buffer's own filetype —
/// so a language only ever reached the engine overlaid if some buffer was written in
/// it. The typescript inside a `.vue` file's `<script setup lang="ts">`, the rust
/// inside a markdown fence, and every nested layer under them got the bundled query
/// and nothing else: the same grammar painted two different ways depending on how it
/// was reached. Here the rust overlay is only reachable through markdown's fence
/// injection, so the custom capture painting proves the injected layer resolved.
#[tokio::test]
#[ignore = "needs network + a C compiler to install real grammars; opt-in like the other ts e2e tests"]
async fn overlay_reaches_an_injected_language() {
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_injected_data");
    bemtvi_ts::install::install(&data, "markdown").expect("install markdown");
    bemtvi_ts::install::install(&data, "rust").expect("install rust");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    // A capture the bundled rust query never emits, on a node it definitely has.
    let rtp = temp_dir("ts_injected_rtp");
    let qdir = rtp.join("after/queries/rust");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(
        qdir.join("highlights.scm"),
        ";; extends\n(function_item name: (identifier) @bemtvi.injected.test)\n",
    )
    .unwrap();

    // No `.rs` buffer anywhere: rust is reached *only* as markdown's injected fence.
    let file = write_temp("ts_injected", "md", "# t\n\n```rust\nfn zzz() {}\n```\n");
    let (_rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    let map = wait_redraw(&mut incoming, |m| {
        payload_has_group(m, "bemtvi.injected.test")
    })
    .await;
    assert!(
        payload_has_group(&map, "bemtvi.injected.test"),
        "an `after/queries/rust` overlay should reach the rust injected into a markdown fence"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}

/// The same gap on the **stateless** highlighter — the picker preview, an LSP doc
/// float, `btv.treesitter.highlight`. Those surfaces have no buffer at all, so before
/// this neither the language they paint nor anything it injects was ever resolved.
/// Driven through `btv.treesitter.highlight` because it returns the spans to Lua;
/// the preview pane and doc floats go through the same call.
#[tokio::test]
#[ignore = "needs network + a C compiler to install real grammars; opt-in like the other ts e2e tests"]
async fn overlay_reaches_the_stateless_highlighter() {
    let _guard = serial_lock().lock().await;

    let data = temp_dir("ts_stateless_data");
    bemtvi_ts::install::install(&data, "markdown").expect("install markdown");
    bemtvi_ts::install::install(&data, "rust").expect("install rust");
    std::env::set_var("BEMTVI_DATA_DIR", &data);

    let rtp = temp_dir("ts_stateless_rtp");
    let qdir = rtp.join("after/queries/rust");
    std::fs::create_dir_all(&qdir).unwrap();
    std::fs::write(
        qdir.join("highlights.scm"),
        ";; extends\n(function_item name: (identifier) @bemtvi.stateless.test)\n",
    )
    .unwrap();

    // No file: nothing opens a buffer, so no language is resolved by the buffer path
    // and the snippet below is the first thing to touch either grammar.
    let (rpc, _incoming) = start_attached(
        ServerInit {
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;

    exec_lua(
        &rpc,
        "_G.seen = {}\n\
         btv.async(function()\n\
           local spans = btv.await(btv.treesitter.highlight('markdown', '```rust\\nfn zzz() {}\\n```\\n'))\n\
           for _, s in ipairs(spans) do _G.seen[s.group] = true end\n\
           _G.done = true\n\
         end)()",
    )
    .await;
    poll_true(&rpc, "return _G.done").await;

    let found = exec_lua(&rpc, "return _G.seen['bemtvi.stateless.test'] == true").await;
    assert_eq!(
        found,
        Value::Boolean(true),
        "an `after/queries/rust` overlay should reach the stateless highlighter's injected rust"
    );

    std::env::remove_var("BEMTVI_DATA_DIR");
}
