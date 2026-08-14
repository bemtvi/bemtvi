//! Keyboard macros — `q{reg}` recording (this phase) and `@{reg}` playback.
//!
//! A macro register holds bemtvi **key notation** (`ciwfoo<Esc>`), so every
//! assertion here can read it back the way a user would: `btv.reg.get`, or
//! pasting the register into the buffer with `"ap`.

use crate::support::*;

/// The register a recording lands in, read back as text.
async fn reg(rpc: &Rpc, name: &str) -> String {
    let v = exec_lua(rpc, &format!("return btv.reg.get(\"{name}\")")).await;
    v.as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn recording_stores_the_typed_keys_as_notation() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // Record a "change the word" macro, then inspect what landed in `a`. The
    // terminating `q` is not part of the macro.
    feed(&rpc, "<F2>a0ciwbeta<Esc><F2>");
    assert_eq!(reg(&rpc, "a").await, "0ciwbeta<Esc>");
    assert_eq!(lines(&rpc).await, vec!["beta"]);
}

#[tokio::test]
async fn a_recording_can_be_pasted_as_text() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "<F2>bx<F2>");
    // `"bp` pastes the recorded keystrokes as ordinary text — the notation is a
    // real, readable register, not an opaque blob.
    feed(&rpc, "\"bp");
    assert_eq!(lines(&rpc).await, vec!["alphx"]);
}

#[tokio::test]
async fn uppercase_register_appends_to_the_recording() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "<F2>ax <F2>");
    feed(&rpc, "<F2>Ax<F2>");
    // Every key round-trips through its notation, `<Space>` included — which is
    // what makes `parse_keys` able to replay the register verbatim.
    assert_eq!(reg(&rpc, "a").await, "x<Space>x");
}

#[tokio::test]
async fn insert_mode_keys_are_recorded_including_the_escape() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "<F2>cihello<Esc><F2>");
    assert_eq!(reg(&rpc, "c").await, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
}

#[tokio::test]
async fn the_stop_key_is_normal_mode_only() {
    let (rpc, _incoming) = start(None).await;
    // Only Normal mode's `<F2>` ends a recording. Mid-insert it is just another
    // recorded key (a no-op one), so a macro can never be committed half-way
    // through an insert session — replaying it would strand you in Insert.
    feed(&rpc, "<F2>aihello<F2> there<Esc><F2>");
    assert_eq!(lines(&rpc).await, vec!["hello there"]);
    assert_eq!(reg(&rpc, "a").await, "ihello<F2><Space>there<Esc>");
}

#[tokio::test]
async fn literal_arguments_are_recorded() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // `f`'s target and `r`'s replacement bypass the keymap matcher (they are read
    // raw, like vim's `plain_vgetc`); they must still reach the recording.
    feed(&rpc, "<F2>a0flrL<F2>");
    assert_eq!(reg(&rpc, "a").await, "0flrL");
    assert_eq!(lines(&rpc).await, vec!["aLpha"]);
}

#[tokio::test]
async fn a_mapping_records_its_lhs_not_its_effect() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<leader>d", function() btv.cmd("normal! dd") end)
           btv.g.mapleader = " ""#,
    )
    .await;
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<Space>d", function() btv.cmd("delete") end)"#,
    )
    .await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    feed(&rpc, "<F2>agg<Space>d<F2>");
    // A Lua-handler mapping produces no keys at all, so recording what reaches the
    // editor would have captured nothing for it. The LHS is what replays.
    assert_eq!(reg(&rpc, "a").await, "gg<Space>d");
    assert_eq!(lines(&rpc).await, vec!["two"]);
}

#[tokio::test]
async fn keys_a_mapping_feeds_are_not_recorded() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<F5>", "dd", { remap = false })"#,
    )
    .await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    feed(&rpc, "<F2>agg<F5><F2>");
    // The RHS keys are fed, not typed: the recording holds the LHS only.
    assert_eq!(reg(&rpc, "a").await, "gg<F5>");
    assert_eq!(lines(&rpc).await, vec!["two"]);
}

#[tokio::test]
async fn keys_a_mapping_feeds_as_typeahead_are_not_recorded() {
    let (rpc, _incoming) = start(None).await;
    // `btv._feedkeys` typeahead is drained through the same matcher a typed key
    // rides, so only the suppression guard keeps it out of the recording.
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<F6>", function() btv._feedkeys("dd", true, false) end)"#,
    )
    .await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // Two batches on purpose: the typeahead drains at the end of the batch that
    // queued it, so the fed `dd` must run while the recording is still open.
    feed(&rpc, "<F2>agg<F6>");
    feed(&rpc, "<F2>");
    assert_eq!(reg(&rpc, "a").await, "gg<F6>");
    assert_eq!(lines(&rpc).await, vec!["two"]);
}

#[tokio::test]
async fn a_withheld_prefix_still_records_and_stops_cleanly() {
    let (rpc, _incoming) = start(None).await;
    // `<F2>x` is a live mapping prefix, so the matcher WITHHOLDS every `<F2>` until
    // the next key decides it: the terminating `<F2>` reaches the editor only once
    // the following `j` breaks the prefix, i.e. after `j` was already typed. Because
    // the recording is fed when a key *executes* (not when it was typed), the `<F2>`
    // is still the last key noted and the macro ends exactly there.
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<F2>x", "dd", { remap = false })"#,
    )
    .await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    feed(&rpc, "<F2>agg0x");
    feed(&rpc, "<F2>j");
    assert_eq!(reg(&rpc, "a").await, "gg0x");
    assert_eq!(lines(&rpc).await, vec!["ne", "two"]);
}

#[tokio::test]
async fn the_message_line_announces_the_recording() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<F2>a").await;
    assert_eq!(message(&map), "recording @a");
    let map = redraw_after(&rpc, &mut incoming, "<F2>").await;
    assert_eq!(message(&map), "");
}

#[tokio::test]
async fn an_unrecordable_register_name_is_a_dead_end() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // `q%` names a read-only register: nothing records (no `recording @%`), and
    // the `%` is consumed by the prompt rather than acting as a motion.
    let map = redraw_after(&rpc, &mut incoming, "<F2>%").await;
    assert_eq!(message(&map), "");
    // A following `q` would now be a *fresh* record prompt, not a stop; `x` proves
    // the editor is back at a clean boundary and nothing was left pending.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["alph"]);
}

// ── playback ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_recorded_macro_replays() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Record "prefix this line with a dash, then go down".
    feed(&rpc, "gg<F2>aI- <Esc>j<F2>");
    assert_eq!(reg(&rpc, "a").await, "I-<Space><Esc>j");
    feed(&rpc, "<F3>a");
    assert_eq!(lines(&rpc).await, vec!["- one", "- two", "three"]);
}

#[tokio::test]
async fn a_count_repeats_the_macro() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>ofour<Esc>");
    feed(&rpc, "gg<F2>aI- <Esc>j<F2>");
    // `3<F3>a` runs it three more times, covering the remaining lines.
    feed(&rpc, "3<F3>a");
    assert_eq!(
        lines(&rpc).await,
        vec!["- one", "- two", "- three", "- four"]
    );
}

#[tokio::test]
async fn f3_twice_replays_the_last_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    feed(&rpc, "gg<F2>aI- <Esc>j<F2>");
    feed(&rpc, "<F3>a");
    feed(&rpc, "<F3><F3>");
    assert_eq!(lines(&rpc).await, vec!["- one", "- two", "- three"]);
}

#[tokio::test]
async fn f3_colon_repeats_the_last_ex_command() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // The `:` register holds the last ex command, so playing it re-runs the
    // command rather than typing its text (vim's `@:`).
    feed(&rpc, "gg:s/o/0/<CR>");
    feed(&rpc, "j<F3>:");
    assert_eq!(lines(&rpc).await, vec!["0ne", "tw0", "three"]);
}

#[tokio::test]
async fn a_macro_replays_through_the_keymap_matcher() {
    let (rpc, _incoming) = start(None).await;
    // The recording holds the mapping's LHS, so playback only works if it
    // re-enters the matcher — `Editor::input` would see an unbound `<F5>`.
    exec_lua(
        &rpc,
        r#"btv.keymap.set("n", "<F5>", function() btv.cmd("normal! x") end)"#,
    )
    .await;
    feed(&rpc, "ialpha<Esc>obravo<Esc>");
    feed(&rpc, "gg<F2>a0<F5>j<F2>");
    assert_eq!(reg(&rpc, "a").await, "0<F5>j");
    feed(&rpc, "<F3>a");
    assert_eq!(lines(&rpc).await, vec!["lpha", "ravo"]);
}

#[tokio::test]
async fn a_macro_can_play_another_macro() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // `b` deletes the character under the cursor; `a` calls `b`, then steps down.
    feed(&rpc, "gg<F2>bx<F2>"); // records `x`, and runs it: "ne"
    feed(&rpc, "gg<F2>a<F3>bj<F2>"); // records `<F3>bj`, and runs it: "e", cursor line 2
    assert_eq!(reg(&rpc, "a").await, "<F3>bj");
    // Playing `a` suspends nothing the caller needs: the nested `b` runs to
    // completion, then `a` resumes at its own `j`.
    feed(&rpc, "2<F3>a");
    assert_eq!(lines(&rpc).await, vec!["e", "wo", "hree"]);
}

#[tokio::test]
async fn a_played_macro_is_not_itself_recorded() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    feed(&rpc, "gg<F2>bx<F2>");
    // Recording `a` while playing `b`: the register holds the `<F3>b` that was
    // typed, never the keys it expanded to.
    feed(&rpc, "<F2>a<F3>b<F2>");
    assert_eq!(reg(&rpc, "a").await, "<F3>b");
}

#[tokio::test]
async fn a_macro_runs_before_the_rest_of_the_batch() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    feed(&rpc, "gg<F2>ax<F2>");
    // vim puts a played register ahead of the remaining typeahead: the `j` and
    // the second `x` act *after* the macro, on the next line.
    feed(&rpc, "gg<F3>ajx");
    assert_eq!(lines(&rpc).await, vec!["e", "wo"]);
}

#[tokio::test]
async fn playing_an_empty_register_does_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "<F3>z");
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

#[tokio::test]
async fn replaying_with_no_history_is_loud() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<F3><F3>").await;
    assert_eq!(message(&map), "E748: No previously used register");
}

#[tokio::test]
async fn a_self_recursive_macro_terminates_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // `a` plays itself. vim relies on the first failing command to break the
    // recursion; until failure aborts playback, the depth cap is what ends it —
    // and it says so rather than hanging.
    feed(&rpc, "<F2>a<F3>a<F2>");
    let map = redraw_after(&rpc, &mut incoming, "<F3>a").await;
    assert_eq!(message(&map), "E169: Command too recursive");
}

// ── failure aborts playback ─────────────────────────────────────────────────

#[tokio::test]
async fn a_count_stops_at_the_end_of_the_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    feed(&rpc, "gg<F2>aI- <Esc>j<F2>");
    // The classic idiom: record once, then replay far more times than there are
    // lines. The `j` on the last line fails, which ends the whole run — without
    // that, the remaining repeats would keep prefixing the last line.
    feed(&rpc, "99<F3>a");
    assert_eq!(lines(&rpc).await, vec!["- one", "- two", "- three"]);
}

#[tokio::test]
async fn an_error_message_aborts_the_rest_of_the_macro() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha bravo<Esc>");
    // A macro is an ordinary register, so it can be written directly — which keeps
    // this test about the *playback* and not about the run that recorded it. The
    // search finds nothing (`E486`), so the `dd` after it must never run.
    exec_lua(&rpc, r#"btv.reg.set("a", "/charlie<CR>dd")"#).await;
    feed(&rpc, "<F3>a");
    assert_eq!(lines(&rpc).await, vec!["alpha bravo"]);
}

#[tokio::test]
async fn an_unmatched_find_aborts_the_rest_of_the_macro() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obravo<Esc>");
    // `fz` matches nothing: a *silent* failure (vim beeps, bemtvi has no bell), so
    // only the failure flag can stop the `x` that follows.
    exec_lua(&rpc, r#"btv.reg.set("a", "0fzx")"#).await;
    feed(&rpc, "gg<F3>a");
    assert_eq!(lines(&rpc).await, vec!["alpha", "bravo"]);
}

#[tokio::test]
async fn a_failure_does_not_abort_ordinary_typing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // The flag is per-keystroke and only playback reads it: a `j` at the end of
    // the buffer still leaves the next typed key working normally.
    feed(&rpc, "j0x");
    assert_eq!(lines(&rpc).await, vec!["lpha"]);
}

// ── surfaces ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_lua_surface_reports_the_recording_register() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        exec_lua(&rpc, "return btv.macro.recording() or ''").await,
        Value::from("")
    );
    feed(&rpc, "<F2>a");
    assert_eq!(
        exec_lua(&rpc, "return btv.macro.recording()").await,
        Value::from("a")
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.reg_recording()").await,
        Value::from("a")
    );
    feed(&rpc, "<F2>");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.reg_recording()").await,
        Value::from("")
    );
}

#[tokio::test]
async fn a_playing_macro_can_be_detected_from_lua() {
    let (rpc, _incoming) = start(None).await;
    // A mapping the macro fires records what `btv.macro.executing()` said while it
    // was running — the only moment the answer is non-nil.
    exec_lua(
        &rpc,
        r#"btv.g.seen = "unset"
           btv.keymap.set("n", "<F5>", function() btv.g.seen = btv.macro.executing() or "none" end)"#,
    )
    .await;
    feed(&rpc, "<F2>a<F5><F2>");
    assert_eq!(
        exec_lua(&rpc, "return btv.g.seen").await,
        Value::from("none")
    );
    feed(&rpc, "<F3>a");
    assert_eq!(exec_lua(&rpc, "return btv.g.seen").await, Value::from("a"));
    // …and it is clear again once the playback has finished.
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.reg_executing()").await,
        Value::from("")
    );
}

#[tokio::test]
async fn btv_macro_play_runs_a_register_from_lua() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    feed(&rpc, "gg<F2>aI- <Esc>j<F2>");
    exec_lua(&rpc, r#"btv.macro.play("a", 2)"#).await;
    assert_eq!(lines(&rpc).await, vec!["- one", "- two", "- three"]);
}

/// The focused window's rendered status line, flattened to text.
fn status_text(map: &[(Value, Value)]) -> String {
    field(map, "status")
        .and_then(Value::as_array)
        .expect("a status segment array")
        .iter()
        .filter_map(|seg| match seg {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_statusline_can_show_the_recording() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"btv.statusline.setup({ left = { "macro", "filename" }, right = {} })"#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<F2>a").await;
    let text = status_text(&map);
    assert!(text.contains("recording @a"), "statusline was {text:?}");
    // The segment is empty again once the recording stops — it contributes
    // nothing rather than a blank slot.
    let map = redraw_after(&rpc, &mut incoming, "<F2>").await;
    let text = status_text(&map);
    assert!(!text.contains("recording"), "statusline was {text:?}");
}
