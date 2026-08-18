//! The **expression register** — `"=` and `<C-r>=`.
//!
//! The one register whose contents are *computed*: the prompt takes a Lua
//! expression, the bounded sandbox evaluates it, and the result is stored in the
//! `=` register (and inserted, for `<C-r>=`). Black-box throughout — keys in,
//! buffer text / register mirror / echoed message out.

use crate::support::*;

/// Feed `keys` and return the first message any resulting frame carries.
///
/// A failing expression echoes on the frame the submit produced, and a later
/// barrier repaint clears the message line — so taking the *latest* redraw loses
/// it. Poll for the latest frame that still carries one.
async fn message_from_any_frame(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    feed(rpc, keys);
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(incoming, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

/// The `=` register's stored text, as Lua's `getreg` sees it.
async fn reg_eq(rpc: &Rpc) -> String {
    exec_lua(rpc, r#"return vim.fn.getreg("=")"#)
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ===== `<C-r>=` in Insert ====================================================

#[tokio::test]
async fn ctrl_r_equals_inserts_an_arithmetic_result() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i<C-r>=6*7<CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["42"]);
}

#[tokio::test]
async fn ctrl_r_equals_inserts_a_string_result() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, r#"i<C-r>=("ab"):rep(3)<CR><Esc>"#);
    assert_eq!(lines(&rpc).await, vec!["ababab"]);
}

#[tokio::test]
async fn the_result_lands_mid_line_and_typing_continues_after_it() {
    let (rpc, _incoming) = start(None).await;
    // The computed text is inserted at the cursor, and what is typed next follows
    // it rather than landing inside it.
    feed(&rpc, "ia=<C-r>=1+1<CR>!<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a=2!"]);
}

#[tokio::test]
async fn line_lnum_and_col_are_in_scope() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    // On line 2, column 4 (past `two`), the expression sees all three.
    feed(
        &rpc,
        "2GA <C-r>=line .. \"/\" .. lnum .. \"/\" .. col<CR><Esc>",
    );
    assert_eq!(
        lines(&rpc).await,
        // `A ` appended a space first, so the line the expression sees is `two `
        // and the cursor sits in its 5th column.
        vec!["one", "two two /2/5", "three"],
        "line is the cursor's line text, lnum/col are 1-based"
    );
}

#[tokio::test]
async fn the_whole_computed_insert_undoes_as_one_change() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // The prompt runs inside the insert session, so its undo snapshot still covers
    // the typing on either side of the computed text.
    feed(&rpc, "A-<C-r>=2*2<CR>-<Esc>");
    assert_eq!(lines(&rpc).await, vec!["abc-4-"]);
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}

#[tokio::test]
async fn the_result_lands_at_every_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    // Drop a cursor on each of the three lines (placement mode), then insert a
    // computed value at all of them.
    feed(&rpc, "gg<A-c>jcjc<Esc>");
    feed(&rpc, "I<C-r>=1+2<CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["3one", "3two", "3three"]);
}

#[tokio::test]
async fn esc_at_the_prompt_computes_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<C-r>=1+1<Esc>");
    // The prompt was abandoned: the typed `x` stands, nothing was computed, and
    // the register was never written.
    assert_eq!(lines(&rpc).await, vec!["x"]);
    assert_eq!(reg_eq(&rpc).await, "");
}

#[tokio::test]
async fn an_empty_expression_computes_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<C-r>=<CR>y<Esc>");
    assert_eq!(lines(&rpc).await, vec!["xy"]);
}

// ===== `"=` at a command boundary ============================================

#[tokio::test]
async fn quote_equals_then_p_pastes_the_computed_text() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>");
    // vim's ordering: `"=` prompts, `<CR>` evaluates and stores, and the *next*
    // command (`p`) pastes what was stored.
    feed(&rpc, "\"=42<CR>p");
    assert_eq!(lines(&rpc).await, vec!["ab42"]);
}

#[tokio::test]
async fn quote_equals_then_capital_p_pastes_before_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>");
    feed(&rpc, "0\"=\"X\"<CR>P");
    assert_eq!(lines(&rpc).await, vec!["Xab"]);
}

#[tokio::test]
async fn a_result_ending_in_a_newline_pastes_linewise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>");
    // The register-kind rule: trailing newline ⇒ linewise, so `p` opens a new line
    // below rather than splicing into this one.
    feed(&rpc, "\"=\"two\\n\"<CR>p");
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

#[tokio::test]
async fn a_count_typed_after_the_prompt_repeats_the_paste() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<Esc>");
    feed(&rpc, "\"=\"-\"<CR>3p");
    assert_eq!(lines(&rpc).await, vec!["x---"]);
}

#[tokio::test]
async fn a_count_typed_before_the_prompt_survives_it() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<Esc>");
    // `3"=…<CR>p`: entering the command line resets the parse state, so the count
    // has to be restored with the register for the `p` that follows.
    feed(&rpc, "3\"=\"-\"<CR>p");
    assert_eq!(lines(&rpc).await, vec!["x---"]);
}

#[tokio::test]
async fn the_computed_text_is_readable_as_the_equals_register() {
    let (rpc, _incoming) = start(None).await;
    feed_sync(&rpc, "\"=1+1<CR>").await;
    // Stored, not re-evaluated per read — so the register file (and its Lua
    // mirror) carries the result like any other register.
    assert_eq!(reg_eq(&rpc).await, "2");
}

#[tokio::test]
async fn a_computed_value_can_replace_a_selection() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcd<Esc>");
    // `"=` opened from Visual comes *back* to Visual, so the selection it was typed
    // over is still there for the `p` that follows — which puts the computed text
    // over it.
    feed(&rpc, "0vl\"=\"XY\"<CR>p");
    assert_eq!(lines(&rpc).await, vec!["XYcd"]);
    assert_eq!(reg_eq(&rpc).await, "XY");
}

#[tokio::test]
async fn yanking_into_the_equals_register_is_still_refused() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // `=` is a read-only register: a delete targeting it aborts the whole command
    // (as it did before it computed anything).
    feed(&rpc, "\"=dd");
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}

// ===== failure, loudly =======================================================

#[tokio::test]
async fn a_syntax_error_reports_and_inserts_nothing() {
    let (rpc, mut incoming) = start(None).await;
    feed_sync(&rpc, "ix").await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "<C-r>=1+<CR>").await;
    assert!(
        msg.contains("invalid expression"),
        "a compile error should be reported, got {msg:?}"
    );
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn a_runtime_error_reports_and_inserts_nothing() {
    let (rpc, mut incoming) = start(None).await;
    feed_sync(&rpc, "ix").await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "<C-r>=error(\"boom\")<CR>").await;
    assert!(
        msg.contains("expression failed") && msg.contains("boom"),
        "a runtime error should name itself, got {msg:?}"
    );
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn a_table_result_reports_and_inserts_nothing() {
    let (rpc, mut incoming) = start(None).await;
    feed_sync(&rpc, "ix").await;
    let msg = message_from_any_frame(&rpc, &mut incoming, "<C-r>={}<CR>").await;
    assert!(
        msg.contains("expected a string or number"),
        "a table is a bug in the expression, not text: {msg:?}"
    );
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn a_runaway_expression_is_abandoned_at_its_deadline() {
    let (rpc, mut incoming) = start(None).await;
    feed_sync(&rpc, "ix").await;
    let started = std::time::Instant::now();
    let msg = message_from_any_frame(
        &rpc,
        &mut incoming,
        "<C-r>=(function() while true do end end)()<CR>",
    )
    .await;
    assert!(
        msg.contains("budget"),
        "the deadline should be reported, got {msg:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "an infinite loop must be abandoned promptly, took {:?}",
        started.elapsed()
    );
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn a_failure_leaves_the_previous_result_in_the_register() {
    let (rpc, _incoming) = start(None).await;
    feed_sync(&rpc, "\"=\"kept\"<CR>").await;
    feed_sync(&rpc, "\"=error(\"boom\")<CR>").await;
    assert_eq!(
        reg_eq(&rpc).await,
        "kept",
        "a failed evaluation must not clobber what was stored"
    );
}

#[tokio::test]
async fn the_expression_cannot_reach_the_host() {
    let (rpc, mut incoming) = start(None).await;
    feed_sync(&rpc, "ix").await;
    // The sandbox environment holds no `io` (nor `os`, `require`, `btv`), so this
    // is an indexing error rather than a file write.
    let msg = message_from_any_frame(&rpc, &mut incoming, "<C-r>=io.open(\"/tmp/x\")<CR>").await;
    assert!(
        msg.contains("expression failed"),
        "reaching for io should fail, got {msg:?}"
    );
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

// ===== the prompt itself =====================================================

#[tokio::test]
async fn the_prompt_reports_itself_as_an_equals_line() {
    let (rpc, _incoming) = start(None).await;
    feed_sync(&rpc, "i<C-r>=").await;
    let kind = exec_lua(&rpc, "return vim.fn.getcmdtype()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(kind, "=", "vim's expression-register prompt character");
}

#[tokio::test]
async fn the_prompt_recalls_its_own_history() {
    let (rpc, _incoming) = start(None).await;
    feed_sync(&rpc, "\"=\"first\"<CR>").await;
    // `<Up>` at a fresh prompt recalls the last submitted expression, which then
    // evaluates to the same thing.
    feed(&rpc, "i<C-r>=<Up><CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["first"]);
}

#[tokio::test]
async fn a_computed_insert_is_not_dot_repeatable() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>"); // the last *repeatable* change
    feed(&rpc, "A-<C-r>=1+1<CR><Esc>");
    assert_eq!(lines(&rpc).await, vec!["ab-2"]);
    // Transiting the command line makes a command non-repeatable — the same
    // central rule that keeps `:s` and `d/foo` out of `.` — so `.` re-runs the
    // insert *before* it (`iab`) rather than computing a second value.
    feed(&rpc, ".");
    let after = lines(&rpc).await;
    assert!(
        !after[0].contains("22"),
        "the expression must not have been re-evaluated: {after:?}"
    );
    assert_eq!(after, vec!["ab-ab2"], "`.` replayed the previous change");
}

#[tokio::test]
async fn a_computed_paste_is_not_dot_repeatable_either() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<Esc>");
    feed(&rpc, "\"=\"-\"<CR>p");
    assert_eq!(lines(&rpc).await, vec!["x-"]);
    feed(&rpc, ".");
    // Same rule: the prompt transited the command line, so `.` replays the insert
    // that came before instead of pasting again.
    let after = lines(&rpc).await;
    assert!(
        !after[0].contains("--"),
        "the computed paste must not repeat: {after:?}"
    );
}

// ===== `<C-r>=` inside the command line ======================================

#[tokio::test]
async fn ctrl_r_equals_splices_a_result_into_an_ex_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    // The computed `2` completes `:2d`, so the nested prompt has to hand its result
    // back to the line that was being typed rather than to the buffer.
    feed(&rpc, ":<C-r>=1+1<CR>d<CR>");
    assert_eq!(lines(&rpc).await, vec!["one", "three"]);
}

#[tokio::test]
async fn the_result_lands_at_the_command_cursor_not_the_end() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    // Type `:d`, walk the cursor back to the front, then splice the range in ahead
    // of it: the result goes in *at the cursor*, building `:2d`.
    feed(&rpc, ":d<Home><C-r>=2<CR><CR>");
    assert_eq!(lines(&rpc).await, vec!["one", "three"]);
}

#[tokio::test]
async fn ctrl_r_equals_splices_into_a_search_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two2<CR>three<Esc>gg");
    // `/two` + a computed `2` searches for `two2`, landing on line 2.
    feed(&rpc, "/two<C-r>=1+1<CR><CR>");
    assert_eq!(cursor(&rpc).await.0, 2, "the spliced pattern found line 2");
}

#[tokio::test]
async fn what_was_already_typed_survives_the_nested_prompt() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // Mid-line, the nested prompt suspends `:s/abc/X` and gives it back intact with
    // the computed suffix appended.
    feed(&rpc, ":s/abc/X<C-r>=9*9<CR>/<CR>");
    assert_eq!(lines(&rpc).await, vec!["X81"]);
}

#[tokio::test]
async fn esc_at_the_nested_prompt_returns_to_the_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // `<Esc>` abandons the *expression*, not the command line: the line resumes
    // exactly as typed and still runs.
    feed(&rpc, ":s/abc/Y<C-r>=1+1<Esc>/<CR>");
    assert_eq!(lines(&rpc).await, vec!["Y"]);
}

#[tokio::test]
async fn a_failing_nested_expression_keeps_the_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // The expression fails; the line it was opened over is still there to finish.
    feed(&rpc, ":s/abc/Z<C-r>=error(\"boom\")<CR>/<CR>");
    assert_eq!(lines(&rpc).await, vec!["Z"]);
}

#[tokio::test]
async fn the_nested_prompt_reports_itself_as_an_equals_line() {
    let (rpc, _incoming) = start(None).await;
    feed_sync(&rpc, ":s/a/b<C-r>=").await;
    let kind = exec_lua(&rpc, "return vim.fn.getcmdtype()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(kind, "=", "the nested line is an expression prompt");
    // …and back on the outer line it is an ex command again.
    feed_sync(&rpc, "1<CR>").await;
    let kind = exec_lua(&rpc, "return vim.fn.getcmdtype()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(kind, ":", "the suspended ex line came back");
}

#[tokio::test]
async fn an_expression_prompt_nests_inside_an_expression_prompt() {
    let (rpc, _incoming) = start(None).await;
    // `<C-r>=` while typing an expression opens another prompt; the inner result is
    // spliced into the outer expression, which then evaluates. `2 .. 1` is `21`.
    feed(&rpc, ":s/^/<C-r>=2 .. <C-r>=1<CR><CR>/<CR>");
    assert_eq!(lines(&rpc).await, vec!["21"]);
}
