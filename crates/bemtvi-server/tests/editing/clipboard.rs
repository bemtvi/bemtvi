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
