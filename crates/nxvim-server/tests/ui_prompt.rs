//! Behavior tests for `nx.ui.input` (alias `vim.ui.input`) and `nx.ui.confirm` —
//! the two command-line prompt primitives of the `nx.ui.*` async UI surface
//! (`docs/specs/2026-06-11-native-plugin-api.md`). Both are PROMISE-ONLY and
//! non-blocking (ADR 0002 rule 3): the Lua call returns a promise at once and it
//! settles on a later tick when the user submits / cancels.
//!
//! Black-box like the rest: a real server sources an `init.lua`, the prompt is
//! driven over the same msgpack-RPC a UI uses, and the assertion is on the value
//! the promise's `:next` records, read back through `nvim_exec_lua`. Delivering the
//! prompt result fires the resolver and `apply_lua_effects` drains the `:next`
//! reaction to fixpoint in the same tick (the microtask-convergence path), so the
//! effect is visible on the next ordered read — no redraw timing needed.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
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

// ----- nx.ui.input -----------------------------------------------------------

#[tokio::test]
async fn input_returns_typed_text_on_enter() {
    let dir = temp_dir("ui_input_text");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = nil, false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text)
           _G.result, _G.called = text, true
         end)",
    )
    .await;

    // The command line is now an open prompt; type into it and submit.
    feed(&rpc, "hello");
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("hello")
    );
}

#[tokio::test]
async fn input_cancel_resolves_with_nil() {
    let dir = temp_dir("ui_input_cancel");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = 'unset', false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // The promise resolved (so a caller can clean up) but with no text.
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(exec_lua(&rpc, "return _G.result").await, Value::Nil);
}

#[tokio::test]
async fn input_prefills_default_and_returns_it_unedited() {
    let dir = temp_dir("ui_input_default");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result = nil
         nx.ui.input({ prompt = 'File: ', default = 'init.lua' }):next(function(t) _G.result = t end)",
    )
    .await;

    // Submit without editing: the prefilled default comes back verbatim.
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("init.lua")
    );
}

#[tokio::test]
async fn input_empty_enter_is_empty_string_not_nil() {
    let dir = temp_dir("ui_input_empty");
    let (rpc, _incoming) = start(&dir, "").await;

    // An empty submission is "" (not a cancel) — matching neovim's vim.ui.input.
    exec_lua(
        &rpc,
        "_G.result = 'unset'
         nx.ui.input({}):next(function(t) _G.result = t end)",
    )
    .await;

    feed(&rpc, "<CR>");

    assert_eq!(exec_lua(&rpc, "return _G.result").await.as_str(), Some(""));
}

#[tokio::test]
async fn input_backspace_past_start_does_not_cancel() {
    let dir = temp_dir("ui_input_bs");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = 'unset', false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    // Type a char, then backspace it AND backspace again past the empty start.
    // Unlike the ex/search command line, a vim.ui.input prompt must stay open
    // (only <Esc> cancels) — so the promise has not settled yet.
    feed(&rpc, "a<BS><BS>");
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(false),
        "backspacing past the start must not cancel the input"
    );

    // The prompt is still live: typing and submitting delivers the text.
    feed(&rpc, "ok<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("ok")
    );
}

#[tokio::test]
async fn vim_ui_input_callback_alias_still_works() {
    let dir = temp_dir("ui_input_alias");
    let (rpc, _incoming) = start(&dir, "").await;

    // vim.ui.input keeps neovim's callback shape (the compat layer adapts the
    // nx.ui.input promise back to on_confirm) — plugins that pass a callback work.
    exec_lua(
        &rpc,
        "_G.result = nil
         vim.ui.input({ prompt = 'Name: ' }, function(text) _G.result = text end)",
    )
    .await;
    feed(&rpc, "via alias<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("via alias")
    );
}

#[tokio::test]
async fn nx_ui_input_rejects_the_old_callback_shape() {
    let dir = temp_dir("ui_input_guard");
    let (rpc, _incoming) = start(&dir, "").await;

    // Passing an on_confirm to the promise-only nx.ui.input fails loudly (no silent
    // no-op) — the error names the migration. pcall captures the raised message.
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.ui.input({}, function() end) end)
         return ok == false and e or '<no error>'",
    )
    .await;
    assert!(
        err.as_str().unwrap_or("").contains("promise-only"),
        "expected a promise-only migration error, got {err:?}"
    );
}

// ----- nx.ui.input history (readline recall) ---------------------------------

#[tokio::test]
async fn input_history_recalls_a_prior_submission_with_up() {
    let dir = temp_dir("ui_input_hist_recall");
    let (rpc, _incoming) = start(&dir, "").await;

    // Submit a line under a history namespace.
    exec_lua(
        &rpc,
        "nx.ui.input({ prompt = '> ', history = 'repl' }):next(function() end)",
    )
    .await;
    feed(&rpc, "first cmd<CR>");

    // Re-open the SAME namespace: <Up> recalls the prior submission, which then
    // submits unedited.
    exec_lua(
        &rpc,
        "_G.recalled = nil
         nx.ui.input({ prompt = '> ', history = 'repl' }):next(function(t) _G.recalled = t end)",
    )
    .await;
    feed(&rpc, "<Up><CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.recalled").await.as_str(),
        Some("first cmd"),
        "<Up> must recall the previous submission from the namespace"
    );
}

#[tokio::test]
async fn input_history_is_namespaced() {
    let dir = temp_dir("ui_input_hist_ns");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "nx.ui.input({ prompt = '> ', history = 'alpha' }):next(function() end)",
    )
    .await;
    feed(&rpc, "alpha entry<CR>");

    // A different namespace sees none of alpha's history: <Up> is a no-op, so the
    // empty line submits as "".
    exec_lua(
        &rpc,
        "_G.r = 'unset'
         nx.ui.input({ prompt = '> ', history = 'beta' }):next(function(t) _G.r = t end)",
    )
    .await;
    feed(&rpc, "<Up><CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some(""),
        "a different namespace must not recall another's history"
    );
}

#[tokio::test]
async fn input_without_history_namespace_does_not_recall() {
    let dir = temp_dir("ui_input_no_hist");
    let (rpc, _incoming) = start(&dir, "").await;

    // A prompt with a namespace records; one WITHOUT a namespace must not recall it
    // (no history opted in) — <Up> is inert and the empty line submits as "".
    exec_lua(
        &rpc,
        "nx.ui.input({ prompt = '> ', history = 'shared' }):next(function() end)",
    )
    .await;
    feed(&rpc, "remembered<CR>");

    exec_lua(
        &rpc,
        "_G.r = 'unset'
         nx.ui.input({ prompt = '> ' }):next(function(t) _G.r = t end)",
    )
    .await;
    feed(&rpc, "<Up><CR>");
    assert_eq!(exec_lua(&rpc, "return _G.r").await.as_str(), Some(""));
}

#[tokio::test]
async fn input_history_skips_empty_and_consecutive_dups() {
    let dir = temp_dir("ui_input_hist_dedup");
    let (rpc, _incoming) = start(&dir, "").await;

    // An empty submission is not recorded; two identical submissions collapse to one
    // entry. After "", "dup", "dup", a single <Up> lands on "dup" and a second <Up>
    // stays there (the ring has exactly one entry).
    for line in ["<CR>", "dup<CR>", "dup<CR>"] {
        exec_lua(
            &rpc,
            "nx.ui.input({ prompt = '> ', history = 'd' }):next(function() end)",
        )
        .await;
        feed(&rpc, line);
    }

    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ', history = 'd' }):next(function(t) _G.r = t end)",
    )
    .await;
    feed(&rpc, "<Up><Up><CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some("dup"),
        "empty lines are not recorded and consecutive dups collapse to one entry"
    );
}

#[tokio::test]
async fn input_rejects_a_non_string_history() {
    let dir = temp_dir("ui_input_hist_guard");
    let (rpc, _incoming) = start(&dir, "").await;

    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.ui.input({ history = 7 }) end)
         return ok == false and e or '<no error>'",
    )
    .await;
    assert!(
        err.as_str()
            .unwrap_or("")
            .contains("history must be a string"),
        "expected a history-type error, got {err:?}"
    );
}

// ----- nx.ui.input autocomplete (the <Tab> wildmenu) -------------------------

#[tokio::test]
async fn input_tab_completes_from_a_sync_source() {
    let dir = temp_dir("ui_input_complete_sync");
    let (rpc, _incoming) = start(&dir, "").await;

    // A synchronous `complete` source returns a candidate list directly.
    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ', complete = function(line, col)
           return { { label = 'banana' }, { label = 'apple' } }
         end }):next(function(t) _G.r = t end)",
    )
    .await;

    // Type a prefix that uniquely matches one candidate, open the wildmenu (<Tab>),
    // select the row (<Tab> again), then accept + submit (<CR>).
    feed(&rpc, "ba");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some("banana"),
        "<Tab> must complete the prompt from the source's candidates"
    );
}

#[tokio::test]
async fn input_tab_completes_from_an_async_promise_source() {
    let dir = temp_dir("ui_input_complete_async");
    let (rpc, _incoming) = start(&dir, "").await;

    // An async source returns a PROMISE of candidates (resolved a microtask later) —
    // the shape a DAP `completions` round-trip takes. The wildmenu must still open.
    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ', complete = function(line, col)
           return nx.promise.new(function(resolve)
             nx.schedule(function() resolve({ { label = 'asyncval' } }) end)
           end)
         end }):next(function(t) _G.r = t end)",
    )
    .await;

    feed(&rpc, "as");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some("asyncval"),
        "an async (promise) completion source must drive the wildmenu too"
    );
}

#[tokio::test]
async fn input_completion_inserts_the_insert_field_over_a_member_token() {
    let dir = temp_dir("ui_input_complete_member");
    let (rpc, _incoming) = start(&dir, "").await;

    // The completed token is the trailing identifier run (breaking on `.`), so a
    // member completion replaces only the part after the dot — `os.get` → `os.getcwd`.
    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ', complete = function()
           return { { label = 'getcwd', insert = 'getcwd' } }
         end }):next(function(t) _G.r = t end)",
    )
    .await;

    feed(&rpc, "os.get");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some("os.getcwd"),
        "completion must replace only the trailing identifier run, keeping `os.`"
    );
}

#[tokio::test]
async fn input_completion_honors_an_explicit_replace_range() {
    let dir = temp_dir("ui_input_complete_range");
    let (rpc, _incoming) = start(&dir, "").await;

    // A candidate carrying an explicit `start`/`length` (the DAP CompletionItem range)
    // replaces exactly that span — here chars [0, 6) ("os.get") — instead of the
    // trailing-identifier token ("get"). Without it, accepting would wrongly yield
    // "os.os.getcwd"; with it, the whole span becomes the member access.
    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ', complete = function()
           return { { label = 'os.getcwd', insert = 'os.getcwd', start = 0, length = 6 } }
         end }):next(function(t) _G.r = t end)",
    )
    .await;

    feed(&rpc, "os.get");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.r").await.as_str(),
        Some("os.getcwd"),
        "an explicit start/length must replace that span, not the token"
    );
}

#[tokio::test]
async fn input_without_complete_ignores_tab() {
    let dir = temp_dir("ui_input_no_complete");
    let (rpc, _incoming) = start(&dir, "").await;

    // A prompt with no `complete` source must not open a wildmenu on <Tab>; the line
    // is unaffected and submits as typed.
    exec_lua(
        &rpc,
        "_G.r = nil
         nx.ui.input({ prompt = '> ' }):next(function(t) _G.r = t end)",
    )
    .await;
    feed(&rpc, "plain");
    feed(&rpc, "<Tab>");
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.r").await.as_str(), Some("plain"));
}

#[tokio::test]
async fn input_completion_debounce_zero_requeries_every_edit() {
    let dir = temp_dir("ui_input_complete_nodebounce");
    let (rpc, _incoming) = start(&dir, "").await;

    // `complete_debounce = 0` disables coalescing: every edit that narrows the open
    // menu re-queries the source immediately. The source counts its invocations.
    exec_lua(
        &rpc,
        "_G.calls = 0
         nx.ui.input({ prompt = '> ', complete_debounce = 0, complete = function()
           _G.calls = _G.calls + 1
           return { { label = 'alpha' } }
         end }):next(function() end)",
    )
    .await;

    feed(&rpc, "a");
    feed(&rpc, "<Tab>"); // initial query (immediate) → 1, menu opens
    feed(&rpc, "l"); // refresh → 2
    feed(&rpc, "p"); // refresh → 3
    assert_eq!(
        exec_lua(&rpc, "return _G.calls").await.as_i64(),
        Some(3),
        "with debounce 0, each narrowing edit re-queries the source"
    );
}

#[tokio::test]
async fn input_completion_debounces_refresh_queries() {
    let dir = temp_dir("ui_input_complete_debounce");
    let (rpc, _incoming) = start(&dir, "").await;

    // With a debounce, a burst of narrowing edits coalesces into a single re-query
    // after the quiet period — the initial `<Tab>` still queries at once.
    exec_lua(
        &rpc,
        "_G.calls = 0
         nx.ui.input({ prompt = '> ', complete_debounce = 400, complete = function()
           _G.calls = _G.calls + 1
           return { { label = 'alpha' } }
         end }):next(function() end)",
    )
    .await;

    feed(&rpc, "a");
    feed(&rpc, "<Tab>"); // initial query (immediate) → 1, menu opens
    feed(&rpc, "l"); // refresh → debounced (not fired yet)
    feed(&rpc, "p"); // refresh → re-arms the debounce

    // Right after the burst (well within 400ms) the source has NOT been re-queried.
    assert_eq!(
        exec_lua(&rpc, "return _G.calls").await.as_i64(),
        Some(1),
        "debounced refreshes must not fire per keystroke"
    );

    // After the quiet period the coalesced query fires exactly once.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.calls").await.as_i64(),
        Some(2),
        "the burst of edits coalesces into a single re-query"
    );
}

#[tokio::test]
async fn input_rejects_a_non_function_complete() {
    let dir = temp_dir("ui_input_complete_guard");
    let (rpc, _incoming) = start(&dir, "").await;

    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.ui.input({ complete = 'nope' }) end)
         return ok == false and e or '<no error>'",
    )
    .await;
    assert!(
        err.as_str()
            .unwrap_or("")
            .contains("complete must be a function"),
        "expected a complete-type error, got {err:?}"
    );
}

// ----- nx.ui.confirm ---------------------------------------------------------

#[tokio::test]
async fn confirm_yes_is_true() {
    let dir = temp_dir("ui_confirm_yes");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer, _G.called = nil, false
         nx.ui.confirm('Delete file?'):next(function(ok) _G.answer, _G.called = ok, true end)",
    )
    .await;

    feed(&rpc, "y");

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn confirm_no_is_false() {
    let dir = temp_dir("ui_confirm_no");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer = nil
         nx.ui.confirm('Delete file?'):next(function(ok) _G.answer = ok end)",
    )
    .await;

    feed(&rpc, "n");

    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(false)
    );
}

#[tokio::test]
async fn confirm_enter_takes_the_default() {
    let dir = temp_dir("ui_confirm_default");
    let (rpc, _incoming) = start(&dir, "").await;

    // Default Yes: <CR> accepts.
    exec_lua(
        &rpc,
        "_G.a = nil
         nx.ui.confirm('Save?'):next(function(ok) _G.a = ok end)",
    )
    .await;
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.a").await, Value::Boolean(true));

    // default = false: <CR> declines.
    exec_lua(
        &rpc,
        "_G.b = nil
         nx.ui.confirm('Overwrite?', { default = false }):next(function(ok) _G.b = ok end)",
    )
    .await;
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.b").await, Value::Boolean(false));
}

#[tokio::test]
async fn confirm_escape_is_false() {
    let dir = temp_dir("ui_confirm_esc");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer = nil
         nx.ui.confirm('Quit without saving?'):next(function(ok) _G.answer = ok end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // Cancel is a "no" — never a silent true.
    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(false)
    );
}
