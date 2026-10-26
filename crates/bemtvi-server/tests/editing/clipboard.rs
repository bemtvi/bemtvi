use crate::support::*;

#[tokio::test]
async fn clipboard_register_round_trips_through_the_provider() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ihello world<Esc>");
    // `"+yy` writes the line to the injected provider (linewise: trailing `\n`).
    feed(&rpc, "\"+yy");
    let _ = lines(&rpc).await; // barrier: the yank has been processed
    assert_eq!(clip.peek(), Some(("hello world\n".to_string(), true)));
    // `"+p` reads it back from the provider and pastes it below.
    feed(&rpc, "o<Esc>\"+p");
    assert_eq!(lines(&rpc).await, vec!["hello world", "", "hello world"]);
}

#[tokio::test]
async fn clipboard_paste_reads_externally_seeded_contents() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    // Something external put a linewise line on the clipboard; `"+p` pastes it.
    clip.seed("from outside\n", true);
    feed(&rpc, "\"+p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "from outside"]);
}

#[tokio::test]
async fn clipboard_round_trips_charwise_kind() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ihello<Esc>");
    // A charwise yank reaches the provider charwise…
    feed(&rpc, "0\"+yl");
    let _ = lines(&rpc).await; // barrier
    assert_eq!(clip.peek(), Some(("h".to_string(), false)));
    // …and a charwise clipboard paste splices inline, not as a new line.
    clip.seed("X", false);
    feed(&rpc, "0\"+p");
    assert_eq!(lines(&rpc).await, vec!["hXello"]);
}

#[tokio::test]
async fn clipboard_yank_mirrors_the_unnamed_register() {
    let (rpc, _incoming, _clip) = start_with_clipboard().await;
    feed(&rpc, "ihello<Esc>");
    // vim sets `""` on any yank regardless of target, so a plain `p` after a
    // `"+yy` still pastes the same text.
    feed(&rpc, "\"+yy");
    feed(&rpc, "p");
    assert_eq!(lines(&rpc).await, vec!["hello", "hello"]);
}

#[tokio::test]
async fn star_register_aliases_plus() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    // `"*` and `"+` map to the one provider in v1.
    clip.seed("star\n", true);
    feed(&rpc, "\"*p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "star"]);
}

#[tokio::test]
async fn clipboard_paste_with_empty_provider_reports_instead_of_silently_failing() {
    // The provider is present but holds nothing — exactly a browser that hasn't
    // granted clipboard-read yet, so the web mirror was never pushed (Firefox on a
    // fresh load). `"+p` must say so (vim's E353) rather than silently no-op.
    let (rpc, mut incoming, _clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    let map = latest_after(&rpc, &mut incoming, "\"+p").await;
    assert!(
        view_str(&map, "message").contains("E353"),
        "expected a loud empty-register message, got: {:?}",
        view_str(&map, "message")
    );
    // Nothing was pasted — the buffer is untouched.
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

#[tokio::test]
async fn clipboard_paste_without_a_provider_errors_loudly() {
    // The default server has no clipboard provider (Disabled).
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "yy"); // fill the unnamed register
    let map = latest_after(&rpc, &mut incoming, "\"+p").await;
    assert!(
        view_str(&map, "message").contains("clipboard"),
        "expected a loud clipboard error, got: {:?}",
        view_str(&map, "message")
    );
    // Crucially the unnamed register was NOT silently pasted instead.
    assert_eq!(lines(&rpc).await, vec!["alpha"]);
}

#[tokio::test]
async fn clipboard_delete_without_a_provider_aborts() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // `"+dd` with no provider errors loudly and leaves the buffer untouched
    // rather than destroying the line with nowhere to put it.
    let map = latest_after(&rpc, &mut incoming, "gg\"+dd").await;
    assert!(
        view_str(&map, "message").contains("clipboard"),
        "expected a loud clipboard error, got: {:?}",
        view_str(&map, "message")
    );
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

// ===== btv.clipboard =========================================================
// Lua reads and writes the clipboard through `btv.clipboard`, asynchronously (a
// browser's clipboard can only be reached that way); `btv.reg` refuses `+` / `*`.

/// Run `code` (a Lua expression yielding a promise) and wait for it to settle, as
/// `{ ok, value }` with `value` the resolution or the rejection message.
async fn settle(rpc: &Rpc, code: &str) -> (bool, Value) {
    exec_lua(
        rpc,
        &format!(
            "_G.settled = nil
             ({code}):next(function(v) _G.settled = {{ true, v }} end,
                            function(e) _G.settled = {{ false, tostring(e) }} end)"
        ),
    )
    .await;
    assert!(
        poll_true(rpc, "return _G.settled ~= nil").await,
        "{code} never settled"
    );
    let got = exec_lua(rpc, "return { _G.settled[1], _G.settled[2] }").await;
    let pair = got.as_array().expect("a pair").clone();
    (
        pair[0].as_bool() == Some(true),
        pair.get(1).cloned().unwrap_or(Value::Nil),
    )
}

#[tokio::test]
async fn clipboard_read_gets_what_another_app_copied() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    clip.seed("from outside", false);
    let (ok, text) = settle(&rpc, "btv.clipboard.read()").await;
    assert!(ok);
    assert_eq!(text.as_str(), Some("from outside"));
    // Read again after another copy: live, not a cached value.
    clip.seed("and again", false);
    assert_eq!(
        settle(&rpc, "btv.clipboard.read()").await.1.as_str(),
        Some("and again")
    );
}

#[tokio::test]
async fn clipboard_read_of_an_empty_clipboard_is_empty() {
    let (rpc, _incoming, _clip) = start_with_clipboard().await;
    let (ok, text) = settle(&rpc, "btv.clipboard.read()").await;
    assert!(ok);
    assert_eq!(text.as_str(), Some(""));
}

#[tokio::test]
async fn clipboard_write_puts_text_on_the_clipboard_and_leaves_the_unnamed_register() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    let (ok, _) = settle(&rpc, "btv.clipboard.write('C:/some/path')").await;
    assert!(ok);
    assert_eq!(clip.peek(), Some(("C:/some/path".to_string(), false)));
    feed(&rpc, "ialpha<Esc>p");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha"],
        "the unnamed register is untouched"
    );
}

#[tokio::test]
async fn clipboard_write_carries_linewise_to_a_paste() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    settle(&rpc, "btv.clipboard.write('a line\\n')").await;
    assert_eq!(clip.peek(), Some(("a line\n".to_string(), true)));
    settle(&rpc, "btv.clipboard.write('one', { linewise = true })").await;
    assert_eq!(clip.peek(), Some(("one".to_string(), true)));
    feed(&rpc, "ialpha<Esc>\"+p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "one"]);
}

#[tokio::test]
async fn the_clipboard_api_without_a_provider_rejects() {
    let (rpc, _incoming) = start(None).await;
    for code in ["btv.clipboard.read()", "btv.clipboard.write('x')"] {
        let (ok, err) = settle(&rpc, code).await;
        assert!(!ok, "{code} resolved");
        assert!(
            err.as_str().unwrap_or("").contains("clipboard"),
            "{code}: {err:?}"
        );
    }
}

#[tokio::test]
async fn the_register_api_refuses_the_clipboard_registers() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    for (call, instead) in [
        ("btv.reg.get, '+'", "btv.clipboard.read()"),
        ("vim.fn.getreg, '*'", "btv.clipboard.read()"),
        ("btv.reg.gettype, '+'", "btv.clipboard.read()"),
        ("btv.reg.set, '+', 'x'", "btv.clipboard.write()"),
        ("vim.fn.setreg, '*', 'x'", "btv.clipboard.write()"),
    ] {
        let err = exec_lua(
            &rpc,
            &format!("local ok, e = pcall({call}) return tostring(e)"),
        )
        .await;
        let err = err.as_str().unwrap_or("");
        assert!(
            err.contains("is the clipboard") && err.contains(instead),
            "{call}: {err}"
        );
    }
    let _ = lines(&rpc).await;
    assert_eq!(clip.peek(), None, "nothing was written");
}

// ===== the built-in Ctrl+C / Ctrl+V bindings =================================
// The desktop copy/paste chords, shipped as overridable defaults (prelude
// `keymap.lua`) on the system clipboard rather than the unnamed register.

#[tokio::test]
async fn ctrl_c_copies_the_visual_selection_to_the_clipboard() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ihello world<Esc>");
    // Select `hello` (`v` + `e` to the word's last char) and copy it: charwise, so
    // it reaches the provider charwise.
    feed(&rpc, "0ve<C-c>");
    let _ = lines(&rpc).await; // barrier: the yank has been processed
    assert_eq!(clip.peek(), Some(("hello".to_string(), false)));
    // A copy leaves the text alone.
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn ctrl_c_copies_a_linewise_selection() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // `V` selects the line; the clipboard gets it linewise (trailing newline).
    feed(&rpc, "ggVj<C-c>");
    let _ = lines(&rpc).await;
    assert_eq!(clip.peek(), Some(("one\ntwo\n".to_string(), true)));
}

#[tokio::test]
async fn ctrl_v_pastes_the_clipboard_in_normal_mode() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    // Something external is on the clipboard — the whole point of the chord.
    clip.seed("X", false);
    // At the cursor, not after it: the cursor sits on `a` (`<Esc>` left it there),
    // so the paste lands before it, where a modeless editor would put it.
    feed(&rpc, "0<C-v>");
    assert_eq!(lines(&rpc).await, vec!["Xalpha"]);
}

#[tokio::test]
async fn ctrl_v_pastes_the_clipboard_in_insert_mode() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    clip.seed("BETA", false);
    // Mid-word, still in insert mode afterwards — the text goes in at the caret and
    // typing continues.
    feed(&rpc, "A <C-v> done<Esc>");
    assert_eq!(lines(&rpc).await, vec!["alpha BETA done"]);
}

#[tokio::test]
async fn the_shift_twins_are_bound_to_the_same_thing() {
    // A terminal without the kitty keyboard protocol collapses Ctrl+Shift+C onto
    // `<C-c>`, but a GUI / browser client — and a kitty-protocol terminal — reports
    // it as its own chord, so both spellings have to be mapped or the shifted one
    // does nothing on exactly the clients that can tell them apart.
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "0vll<C-S-c>");
    let _ = lines(&rpc).await;
    assert_eq!(clip.peek(), Some(("hel".to_string(), false)));
    clip.seed("Y", false);
    feed(&rpc, "0<C-S-v>");
    assert_eq!(lines(&rpc).await, vec!["Yhello"]);
}

#[tokio::test]
async fn ctrl_v_pastes_into_the_command_line() {
    let (rpc, mut incoming, clip) = start_with_clipboard().await;
    clip.seed("pasted/path.txt", false);
    // `:e ` then the chord: the clipboard lands in the line being typed, so a path
    // copied from a terminal can be pasted into `:e` instead of retyped.
    let map = latest_after(&rpc, &mut incoming, ":e <C-v>").await;
    assert_eq!(view_str(&map, "cmdline"), "e pasted/path.txt");
    // The shifted twin is bound here too — a terminal that can tell them apart (and
    // every GUI / browser client) reports Ctrl+Shift+V as its own chord.
    let map = latest_after(&rpc, &mut incoming, "<Esc>:e <C-S-v>").await;
    assert_eq!(view_str(&map, "cmdline"), "e pasted/path.txt");
}

/// The chords are `default` maps, which is what lets a `noremap` RHS reach them: a
/// fed key consults the *built-in* maps and skips the user ones, exactly as vim's
/// built-ins fire for a `noremap` mapping. So a config that maps its own key to
/// `<C-v>` gets the clipboard paste — the flag, not just the binding, is load-bearing.
#[tokio::test]
async fn a_noremap_rhs_still_reaches_the_built_in_chord() {
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    clip.seed("CLIP", false);
    exec_lua(
        &rpc,
        "vim.g.mapleader = ',' btv.keymap.set('n', '<leader>p', '<C-v>')",
    )
    .await;
    feed(&rpc, "0,p");
    assert_eq!(lines(&rpc).await, vec!["CLIPalpha"]);
}

#[tokio::test]
async fn a_config_map_overrides_the_built_in_chord() {
    // A user's own map on the same key wins over the built-in, and mapping it to an
    // empty function turns the chord off. Without that a config could never take
    // `<C-v>` back.
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ialpha<Esc>");
    clip.seed("CLIP", false);
    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<C-v>', function() btv.cmd('normal! ihijacked') end)",
    )
    .await;
    feed(&rpc, "0<C-v>");
    assert_eq!(lines(&rpc).await, vec!["hijackedalpha"]);
}

// ===== OSC 52 (the ssh / no-host-tool fallback) ==============================

/// The reported bug: over ssh there is no `pbcopy`/`wl-copy`/`xclip` on the
/// remote box, so `"+y` had no provider at all and errored. A client that speaks
/// OSC 52 *is* a clipboard — the yank must leave as the terminal escape that puts
/// the text on the machine the user is actually sitting at.
#[tokio::test]
async fn osc52_yank_writes_the_clipboard_escape_to_the_terminal() {
    let (rpc, mut incoming) = start_with_osc52().await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "\"+yy");
    let params = wait_notification(&mut incoming, "btv_ui_send").await;
    // `ESC ] 52 ; c ; <base64> ESC \` — selection `c` (the clipboard), payload the
    // linewise yank including its trailing newline.
    assert_eq!(
        params.first().and_then(|v| v.as_str()),
        Some("\x1b]52;c;aGVsbG8gd29ybGQK\x1b\\"),
        "expected an OSC 52 clipboard write, got: {params:?}"
    );
}

/// A charwise yank sends exactly the yanked text (no phantom newline), and `"*`
/// rides the same clipboard as `"+`.
#[tokio::test]
async fn osc52_star_register_sends_the_charwise_text() {
    let (rpc, mut incoming) = start_with_osc52().await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "0\"*yl");
    let params = wait_notification(&mut incoming, "btv_ui_send").await;
    assert_eq!(
        params.first().and_then(|v| v.as_str()),
        Some("\x1b]52;c;aA==\x1b\\"),
        "expected an OSC 52 write of \"h\", got: {params:?}"
    );
}

/// The terminal can't be *read* back (an OSC 52 read query is a round trip most
/// terminals refuse), so the provider remembers what this session put there —
/// `"+p` pastes it rather than erroring, exactly like a real clipboard.
#[tokio::test]
async fn osc52_paste_reads_back_what_this_session_copied() {
    let (rpc, _incoming) = start_with_osc52().await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "\"+yy");
    feed(&rpc, "o<Esc>\"+p");
    assert_eq!(lines(&rpc).await, vec!["hello world", "", "hello world"]);
}

/// The escape is only safe to emit on a terminal that understands it, so the
/// fallback arms on the client's declared capability. A client that doesn't
/// declare `osc52` gets no provider at all — the loud error, not a copy that
/// quietly goes nowhere.
#[tokio::test]
async fn osc52_stays_off_for_a_client_that_cannot_do_it() {
    let (rpc, mut incoming) = start_osc52_with_caps(vec![]).await;
    feed(&rpc, "ialpha<Esc>");
    let map = latest_after(&rpc, &mut incoming, "\"+yy").await;
    assert!(
        view_str(&map, "message").contains("clipboard"),
        "expected a loud clipboard error, got: {:?}",
        view_str(&map, "message")
    );
    assert!(
        drain_notification(&mut incoming, "btv_ui_send").is_none(),
        "a client that can't do OSC 52 must not be sent one"
    );
}

#[tokio::test]
async fn registers_listing_shows_the_clipboard_registers() {
    // `:registers` must list `"+` / `"*` alongside the stored ones, as vim does —
    // it is how you check what a `"+y` (or the `<C-c>` chord) actually put on the
    // system clipboard. They were absent: the listing walked the stored register
    // file plus `%` `/` `:` and stopped, so the one register whose contents live
    // outside the editor was the one you could not see.
    let (rpc, _incoming, clip) = start_with_clipboard().await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "\"+yiw");
    let _ = lines(&rpc).await; // barrier: the yank has been processed
    assert_eq!(clip.peek(), Some(("world".to_string(), false)));

    feed(&rpc, ":registers<CR>");
    let shown = lines(&rpc).await;
    let plus = shown
        .iter()
        .find(|l| l.contains("\"+"))
        .unwrap_or_else(|| panic!("no \"+ row in {shown:?}"));
    assert!(
        plus.contains("world"),
        "the \"+ row must show the clipboard contents, got {plus:?}"
    );
    assert!(
        shown.iter().any(|l| l.contains("\"*")),
        "\"* is the same clipboard and is listed too, got {shown:?}"
    );
}

#[tokio::test]
async fn registers_listing_omits_an_empty_clipboard() {
    // An empty clipboard is not a row: `:registers` lists what is *there*, and a
    // blank `"+` line would be noise on every listing.
    let (rpc, _incoming, _clip) = start_with_clipboard().await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, ":registers<CR>");
    let shown = lines(&rpc).await;
    assert!(
        shown.iter().all(|l| !l.contains("\"+")),
        "an empty clipboard must not be listed, got {shown:?}"
    );
}

/// On Windows the real host clipboard (`ClipboardProvider::System`, what the GUI and
/// TUI pass) must back `"+`: a yank reaches the OS clipboard and `"+p` reads it back.
/// Detection used to know only macOS/Linux tools, so on Windows `"+y` (the GUI's
/// `<C-c>`) failed with "No provider". Ignored by default: it overwrites the
/// developer's real clipboard. Run with `--ignored` on Windows.
#[cfg(windows)]
#[tokio::test]
#[ignore = "writes the real host clipboard"]
async fn system_clipboard_round_trips_on_windows() {
    let (rpc, _incoming) = start_with(ServerInit {
        clipboard: bemtvi_server::ClipboardProvider::System,
        ..Default::default()
    })
    .await;
    feed(&rpc, "ihéllo wörld ✓<Esc>");
    feed(&rpc, "0\"+yiW");
    let _ = lines(&rpc).await; // barrier: the yank has been processed
                               // The OS clipboard now holds the text — read it independently of bemtvi.
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw",
        ])
        .output()
        .expect("run powershell Get-Clipboard");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), "héllo");
    // And `"+p` reads it back through the provider.
    feed(&rpc, "o<Esc>\"+p");
    assert_eq!(lines(&rpc).await, vec!["héllo wörld ✓", "héllo"]);
}
