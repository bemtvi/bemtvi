//! Global option plumbing: an option set through `vim.o` reaches the core, reads
//! back consistently, and — for UI-relevant ones like `guifont` — is relayed to
//! the client in the `redraw` (where a GUI parses it for the font).

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    command, drain_to_latest_redraw, exec_lua, field, field_str, message, start_attached, u64_at,
    write_temp,
};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// `:set <name…>` then read the echoed message off the redraw — the seam a loud
/// `:set` error surfaces through.
async fn set_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, args: &str) -> String {
    command(rpc, &format!("set {args}")).await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(incoming, |_| true).expect("a redraw arrived");
    message(&frame)
}

/// Every option `:set` recognizes (the `canonical` registry) must be wired all the
/// way to its storage — `:set <name>?` returns a real readout, never an empty
/// (silent) message and never E518 (the wiring-gap error). This guards the exact
/// `imagepreview` bug: a name added to the registry but missing from `apply_set_*`'s
/// slot match used to silently no-op; now it's loud, and this catches it.
///
/// The name list is enumerated from the authoritative catalog itself
/// (`btv._options_catalog`, built from `bemtvi_core::options::options_catalog()`),
/// not hand-kept here — so the guard covers every option automatically and can
/// never drift from what `:set` actually accepts.
#[tokio::test]
async fn every_known_option_is_wired_not_silent() {
    let (rpc, mut incoming) = start().await;
    let names = exec_lua(
        &rpc,
        "local o = {} \
         for _, r in ipairs(btv._options_catalog) do o[#o + 1] = r.name end \
         return table.concat(o, ',')",
    )
    .await;
    let names = names.as_str().expect("catalog names join to a string");
    assert!(
        names.split(',').count() >= 70,
        "the catalog should enumerate every documented option, got {names:?}"
    );
    for name in names.split(',') {
        let msg = set_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            msg.contains(name) && !msg.contains("E518"),
            "`:set {name}?` must give a real readout (option wired), got {msg:?}"
        );
    }
}

#[tokio::test]
async fn set_unknown_option_errors_loudly() {
    let (rpc, mut incoming) = start().await;
    // A genuinely non-existent option name is a loud E518, naming the option — never
    // a silent no-op (CLAUDE.md's no-silent-stub rule). Covers a typo and a bogus name.
    let msg = set_message(&rpc, &mut incoming, "nonexistentoption").await;
    assert!(
        msg.contains("E518") && msg.contains("nonexistentoption"),
        "unknown :set option must fail loud naming it, got {msg:?}"
    );

    // A `no`-prefixed bogus boolean, an `=`-assignment to a bogus name, and a `?`
    // query of a bogus name are all equally loud (not silently swallowed by the prefix
    // parsing).
    let msg = set_message(&rpc, &mut incoming, "nobogus").await;
    assert!(
        msg.contains("E518"),
        "`:set nobogus` must be loud, got {msg:?}"
    );
    let msg = set_message(&rpc, &mut incoming, "bogus=3").await;
    assert!(
        msg.contains("E518"),
        "`:set bogus=3` must be loud, got {msg:?}"
    );
    let msg = set_message(&rpc, &mut incoming, "bogus?").await;
    assert!(
        msg.contains("E518"),
        "`:set bogus?` must be loud, got {msg:?}"
    );
}

#[tokio::test]
async fn guifont_round_trips_and_reaches_the_redraw() {
    let (rpc, mut incoming) = start().await;

    // `vim.o.guifont = …` (the init.lua form) reaches the core and reads back.
    exec_lua(&rpc, "vim.o.guifont = 'Fira Code:h14'").await;
    let read = exec_lua(&rpc, "return vim.o.guifont").await;
    assert_eq!(
        read.as_str(),
        Some("Fira Code:h14"),
        "vim.o.guifont reads back"
    );

    // And it is relayed to the UI in the redraw, so a GUI can apply the font.
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "guifont"), "Fira Code:h14");
}

#[tokio::test]
async fn guifont_defaults_empty() {
    // Unset, both the read-back and the redraw field are empty — the GUI then uses
    // its own default font.
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.guifont").await.as_str(),
        Some("")
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "guifont"), "");
}

#[tokio::test]
async fn guiglyphoverflow_round_trips_and_reaches_the_redraw() {
    let (rpc, mut incoming) = start().await;

    // Unset, both the read-back and the redraw field are empty — each client then keeps
    // its own setting (`--glyph-overflow` / its built-in default).
    assert_eq!(
        exec_lua(&rpc, "return btv.o.guiglyphoverflow")
            .await
            .as_str(),
        Some("")
    );

    // `btv.o.guiglyphoverflow = …` (the init.lua form, wezterm's config knob) reaches
    // the core, reads back, and is relayed so the GUI / web client can size icons by it.
    exec_lua(&rpc, "btv.o.guiglyphoverflow = 'always'").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.guiglyphoverflow")
            .await
            .as_str(),
        Some("always"),
        "the option reads back through vim.o too"
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "guiglyphoverflow"), "always");

    // The ex path writes the same state, and the query echoes it.
    let msg = set_message(
        &rpc,
        &mut incoming,
        "guiglyphoverflow=when-followed-by-space",
    )
    .await;
    assert!(msg.is_empty(), "a valid set is silent, got {msg:?}");
    let msg = set_message(&rpc, &mut incoming, "guiglyphoverflow?").await;
    assert_eq!(msg, "guiglyphoverflow=when-followed-by-space");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        field_str(&frame, "guiglyphoverflow"),
        "when-followed-by-space"
    );
}

#[tokio::test]
async fn guiglyphoverflow_rejects_an_unknown_mode() {
    // An enumerated value: a typo fails loud (E474) and leaves the previous mode in
    // place, rather than silently rendering in a mode nobody asked for.
    let (rpc, mut incoming) = start().await;
    set_message(&rpc, &mut incoming, "guiglyphoverflow=always").await;
    let msg = set_message(&rpc, &mut incoming, "guiglyphoverflow=alway").await;
    assert!(
        msg.contains("E474") && msg.contains("alway"),
        "a bad mode must fail loud naming it, got {msg:?}"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.o.guiglyphoverflow")
            .await
            .as_str(),
        Some("always"),
        "the rejected write left the previous mode alone"
    );
}

#[tokio::test]
async fn timeout_and_timeoutlen_default_and_reach_the_redraw() {
    // The mapping-timeout config is on with a 1000ms wait by default (vim's), reads
    // back through `vim.o`, and is relayed to the client in every `redraw` so each
    // client runs its idle-flush timer to match.
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeout").await.as_bool(),
        Some(true),
        "timeout defaults on"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeoutlen").await.as_u64(),
        Some(1000),
        "timeoutlen defaults to 1000"
    );

    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        field(&frame, "timeout").and_then(rmpv::Value::as_bool),
        Some(true),
        "redraw relays timeout=true"
    );
    assert_eq!(
        u64_at(&frame, "timeoutlen"),
        1000,
        "redraw relays timeoutlen"
    );
}

#[tokio::test]
async fn notimeout_and_timeoutlen_round_trip_and_relay() {
    let (rpc, mut incoming) = start().await;

    // `:set notimeout timeoutlen=250` reaches the core, reads back through `vim.o`,
    // and the new values ride the next redraw (so the client disarms its flush and,
    // were it re-enabled, would wait 250ms).
    command(&rpc, "set notimeout timeoutlen=250").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeout").await.as_bool(),
        Some(false),
        "notimeout read back through vim.o"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeoutlen").await.as_u64(),
        Some(250)
    );

    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        field(&frame, "timeout").and_then(rmpv::Value::as_bool),
        Some(false),
        "redraw relays timeout=false under notimeout"
    );
    assert_eq!(u64_at(&frame, "timeoutlen"), 250);

    // And the `vim.o` write path sets it too (the init.lua form), reading back.
    exec_lua(&rpc, "vim.o.timeout = true; vim.o.timeoutlen = 700").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeout").await.as_bool(),
        Some(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.timeoutlen").await.as_u64(),
        Some(700)
    );
}

#[tokio::test]
async fn fillchars_round_trips_through_set_and_vim_wo() {
    let (rpc, mut incoming) = start().await;

    // Defaults empty (vim's default look, `eob:~`), both via `:set …?` and `vim.wo`.
    let msg = set_message(&rpc, &mut incoming, "fillchars?").await;
    assert_eq!(msg, "fillchars=");
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.fillchars").await.as_str(),
        Some("")
    );

    // `vim.wo.fillchars = 'eob: '` (the window-local Lua form) reaches the core and
    // reads back through both the `:set …?` echo and `vim.wo`.
    exec_lua(&rpc, "vim.wo.fillchars = 'eob: '").await;
    let msg = set_message(&rpc, &mut incoming, "fillchars?").await;
    assert_eq!(msg, "fillchars=eob: ");
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.fillchars").await.as_str(),
        Some("eob: ")
    );
}

#[tokio::test]
async fn unknown_vim_o_option_warns_but_stores() {
    let (rpc, mut incoming) = start().await;

    // First, a KNOWN option through `vim.o` must NOT warn (clean session, empty
    // message before any unknown write).
    exec_lua(&rpc, "vim.o.hlsearch = false").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert!(
        !message(&frame).to_lowercase().contains("unknown option"),
        "a known vim.o option must not warn, got {:?}",
        message(&frame)
    );

    // An option bemtvi doesn't model (a typo, or an unmodeled real neovim option) is
    // KEPT — compat: a config setting it still loads — but surfaces a warning naming
    // it, so it isn't silently swallowed (unlike `:set`, which rejects it outright).
    exec_lua(&rpc, "vim.o.nonexistentopt = true").await;
    let frame = drain_to_latest_redraw(&mut incoming, |m| message(m).contains("nonexistentopt"))
        .expect("the unknown-option warning surfaced");
    let msg = message(&frame);
    assert!(
        msg.contains("nonexistentopt") && msg.to_lowercase().contains("unknown option"),
        "an unmodeled vim.o option must warn naming it, got {msg:?}"
    );

    // …and it is still stored (reads back), so the compat catch-all is preserved.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.nonexistentopt")
            .await
            .as_bool(),
        Some(true),
        "the unknown option is kept (compat), not rejected"
    );
}

#[tokio::test]
async fn seeded_read_mostly_options_write_without_warning() {
    // `background` / `termguicolors` are deliberately modeled as read-mostly
    // store-backed options (seeded with neovim defaults so colorschemes can read
    // them). Writing them is expected — every colorscheme does — so it must NOT
    // surface the "unknown option" warning reserved for genuine typos. (Regression:
    // catppuccin's `vim.o.termguicolors`/`vim.o.background` warned on load.)
    let (rpc, mut incoming) = start().await;

    exec_lua(&rpc, "vim.o.termguicolors = true").await;
    exec_lua(&rpc, "vim.o.background = 'dark'").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert!(
        !message(&frame).to_lowercase().contains("unknown option"),
        "writing a seeded read-mostly option must not warn, got {:?}",
        message(&frame)
    );

    // …and they still round-trip (the store keeps the written value).
    assert_eq!(
        exec_lua(&rpc, "return vim.o.background").await.as_str(),
        Some("dark")
    );
}

#[tokio::test]
async fn autoread_defaults_on_and_round_trips_through_vim_o() {
    let (rpc, _incoming) = start().await;

    // neovim's default is on, so the mirror reflects the core default before any set.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(true),
        "vim.o.autoread defaults on (neovim)"
    );

    // A write through `vim.o` reaches the core and reads back (the `:checktime`
    // reload-vs-warn decision reads this exact flag).
    exec_lua(&rpc, "vim.o.autoread = false").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.autoread").await.as_bool(),
        Some(false),
        "vim.o.autoread = false round-trips"
    );
}

/// `vim.opt.wrap` / `vim.o.wrap` must reach the window-local `wrap` option in the
/// core — NOT warn-and-store like an unmodeled name. `wrap` is fully modeled (real
/// soft-wrap rendering; `:set wrap` works and `vim.wo.wrap` already routes), but the
/// `vim.o`/`vim.opt` window-option table was missing it, so `vim.opt.wrap = false`
/// silently no-oped and warned "unknown option". Regression guard for that gap.
#[tokio::test]
async fn wrap_reaches_core_via_vim_opt_and_vim_o() {
    let (rpc, mut incoming) = start().await;

    // Setting `wrap` through the rich `vim.opt` surface must not surface the
    // unknown-option warning reserved for genuine typos / unmodeled options…
    exec_lua(&rpc, "vim.opt.wrap = true").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert!(
        !message(&frame).to_lowercase().contains("unknown option"),
        "vim.opt.wrap must not warn — wrap is a modeled window option, got {:?}",
        message(&frame)
    );

    // …and it must reach the CORE, not just the compat store: the core's own
    // `:set wrap?` echo (the authoritative readout) reports it on.
    let echo = set_message(&rpc, &mut incoming, "wrap?").await;
    assert!(
        echo.contains("wrap") && !echo.contains("nowrap"),
        "the core reports wrap on after vim.opt.wrap = true, got {echo:?}"
    );

    // The `vim.o` scalar surface reaches it too (and turns it back off).
    exec_lua(&rpc, "vim.o.wrap = false").await;
    let echo = set_message(&rpc, &mut incoming, "wrap?").await;
    assert!(
        echo.contains("nowrap"),
        "the core reports nowrap after vim.o.wrap = false, got {echo:?}"
    );
}

/// `'scrolloff'` (abbrev `so`, numeric) and `'colorcolumn'` (abbrev `cc`,
/// comma-list) are modeled window options: `:set <name>=`, `vim.o.<name>`, and
/// `vim.opt.<abbrev>` all reach the core and read back through the `:set …?`
/// echo and the `vim.wo` mirror, without the unknown-option warning. (The margin
/// / ruler *behavior* is covered by the `editing::scrolloff` suite and the TUI
/// paint tests; this pins the option plumbing. `vim.opt.cc = { 100 }` also pins
/// the rich list surface — a Lua array encodes to the comma string.)
#[tokio::test]
async fn window_options_round_trip_through_set_and_vim_o() {
    for (name, default_echo, o_write, want_echo, wo_want, opt_write, opt_echo) in [
        (
            "scrolloff",
            "scrolloff=0",
            "vim.o.scrolloff = 8",
            "scrolloff=8",
            "8",
            "vim.opt.so = 3",
            "scrolloff=3",
        ),
        (
            "colorcolumn",
            "colorcolumn=",
            "vim.o.colorcolumn = '80,120'",
            "colorcolumn=80,120",
            "80,120",
            "vim.opt.cc = { 100 }",
            "colorcolumn=100",
        ),
    ] {
        let (rpc, mut incoming) = start().await;

        // The default, readable through the `:set …?` echo.
        let echo = set_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            echo.contains(default_echo),
            "[{name}] default, got {echo:?}"
        );

        // A write through `vim.o` must not warn and must reach the core (the
        // `:set …?` echo is the authoritative readout; `vim.wo` mirrors it).
        exec_lua(&rpc, o_write).await;
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
        let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
        assert!(
            !message(&frame).to_lowercase().contains("unknown option"),
            "[{name}] {o_write} must not warn, got {:?}",
            message(&frame)
        );
        let echo = set_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            echo.contains(want_echo),
            "[{name}] {o_write} reaches the core, got {echo:?}"
        );
        assert_eq!(
            exec_lua(&rpc, &format!("return tostring(vim.wo.{name})"))
                .await
                .as_str(),
            Some(wo_want),
            "[{name}] vim.wo reads the core value back"
        );

        // The abbreviation via the rich `vim.opt` surface reaches the same slot.
        exec_lua(&rpc, opt_write).await;
        let echo = set_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            echo.contains(opt_echo),
            "[{name}] {opt_write} reaches the core, got {echo:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Global-local options: a buffer-local option's GLOBAL value, the tier every
// newly created buffer is born from (`docs/plans/2026-08-01-global-local-options.md`).
// ---------------------------------------------------------------------------

/// `:setglobal <name…>` then read the echoed message — the `:setglobal` twin of
/// [`set_message`].
async fn setglobal_message(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    args: &str,
) -> String {
    command(rpc, &format!("setglobal {args}")).await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(incoming, |_| true).expect("a redraw arrived");
    message(&frame)
}

/// "ts=<n> et=<b>" for the current buffer, read through the `vim.bo` mirror.
async fn indent_of_current(rpc: &Rpc) -> String {
    exec_lua(
        rpc,
        r#"local b = btv.buf.current()
           return "ts=" .. tostring(btv.bo[b].tabstop) .. " et=" .. tostring(btv.bo[b].expandtab)"#,
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_string()
}

/// Open `path`, and hold the editor on a buffer that is NOT the startup throwaway:
/// `:edit` reuses a throwaway `[No Name]` in place (same bufnr), and these tests are
/// about buffers that are genuinely new.
async fn edit(rpc: &Rpc, path: &str) {
    command(rpc, &format!("edit {path}")).await;
}

/// THE bug this closes: `:set tabstop=3` must reach files opened *afterwards*, not
/// just the buffer that happened to be current — which is what a config's
/// `vim.opt.tabstop = 3` needs to mean. Before the global tier, every later buffer
/// was born at `BufferOptions::default()` and the config silently applied to the
/// startup `[No Name]` alone.
#[tokio::test]
async fn a_set_buffer_option_reaches_buffers_opened_later() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optglobal_later_a", "txt", "one\n");
    let file = write_temp("optglobal_later_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    command(&rpc, "set tabstop=3 expandtab").await;
    assert_eq!(indent_of_current(&rpc).await, "ts=3 et=true", "this buffer");
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=true",
        "a file opened after the `:set` is born from the global value"
    );
}

/// `:setlocal` is the opt-out: it writes only this buffer, so the next one still
/// gets the global value. (Before, `:setlocal` was a plain alias of `:set`.)
#[tokio::test]
async fn setlocal_does_not_leak_to_the_next_buffer() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optglobal_setlocal_a", "txt", "one\n");
    let file = write_temp("optglobal_setlocal_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    command(&rpc, "set tabstop=3").await;
    command(&rpc, "setlocal tabstop=8").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=8 et=false",
        "this buffer"
    );
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=false",
        "the next buffer follows the global value, not the `:setlocal` one"
    );
}

/// `:setglobal` is the other half: it changes what new buffers inherit without
/// touching the buffer you are in.
#[tokio::test]
async fn setglobal_spares_the_current_buffer_and_seeds_the_next() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optglobal_setglobal_a", "txt", "one\n");
    let file = write_temp("optglobal_setglobal_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    command(&rpc, "setglobal tabstop=2").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=4 et=false",
        "the current buffer keeps its own value"
    );
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=2 et=false",
        "while the next buffer is born from the global value"
    );
}

/// A buffer keeps the options it was born with across an in-place reload — `:e!` and
/// the deferred-open fill swap the whole `Buffer`, and must not quietly reset it to
/// the built-in defaults.
#[tokio::test]
async fn a_reload_keeps_the_buffers_own_options() {
    let (rpc, _incoming) = start().await;
    let file = write_temp("optglobal_reload", "txt", "one\n");
    edit(&rpc, &file).await;
    command(&rpc, "setlocal tabstop=7").await;
    command(&rpc, "edit!").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=7 et=false",
        "the reload kept this buffer's own `:setlocal`"
    );
}

/// The two tiers are separately readable: after a `:setlocal`, `:set ts?` and
/// `:setglobal ts?` disagree — and each reports its own tier.
#[tokio::test]
async fn set_query_reads_local_and_setglobal_query_reads_global() {
    let (rpc, mut incoming) = start().await;
    command(&rpc, "set tabstop=3").await;
    command(&rpc, "setlocal tabstop=8").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "tabstop?").await,
        "tabstop=8"
    );
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "tabstop?").await,
        "tabstop=3"
    );
}

/// A boolean toggles in both tiers off one value, so `:set invexpandtab` can't
/// leave the global and the buffer disagreeing.
#[tokio::test]
async fn a_toggle_moves_both_tiers_together() {
    let (rpc, mut incoming) = start().await;
    let seed = write_temp("optglobal_toggle_a", "txt", "one\n");
    let file = write_temp("optglobal_toggle_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    command(&rpc, "set expandtab!").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "expandtab?").await,
        "expandtab"
    );
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "expandtab?").await,
        "expandtab"
    );
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=4 et=true",
        "and the toggled value is what the next buffer inherits"
    );
}

/// An option the *read* decides — the encoding trio, and the `modifiable` marker — has
/// no global value, and `:setglobal` on one says so loudly rather than storing a value
/// nothing would read (CLAUDE.md's no-silent-stub rule). Same for the two nouns derived
/// per buffer, `filetype` and `ts_highlight`.
#[tokio::test]
async fn setglobal_of_a_scopeless_option_fails_loud() {
    let (rpc, mut incoming) = start().await;
    for name in [
        "fileencoding=latin1",
        "fileformat=dos",
        "bomb",
        "modifiable",
        "filetype=lua", // derived per buffer
        "ts_highlight", // per-buffer engine state
    ] {
        let msg = setglobal_message(&rpc, &mut incoming, name).await;
        let opt = name.split('=').next().unwrap();
        assert!(
            msg.contains("E5100") && msg.contains(opt),
            "`:setglobal {name}` must fail loud naming the option, got {msg:?}"
        );
    }
    // …and the loud rejection really did leave the buffer alone.
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[btv.buf.current()].fileencoding")
            .await
            .as_str()
            .unwrap_or_default(),
        "utf-8",
        "a rejected `:setglobal fileencoding` changed nothing"
    );
}

/// The `:setglobal` twin of `every_known_option_is_wired_not_silent`: every option
/// the catalog lists must answer `:setglobal <name>?` with either a real readout or
/// the loud "no global value" rejection — never silence, and never E518 (which would
/// mean a name the scoped path forgot to wire).
#[tokio::test]
async fn every_known_option_answers_setglobal_query() {
    let (rpc, mut incoming) = start().await;
    let names = exec_lua(
        &rpc,
        "local o = {} \
         for _, r in ipairs(btv._options_catalog) do o[#o + 1] = r.name end \
         return table.concat(o, ',')",
    )
    .await;
    let names = names.as_str().expect("catalog names join to a string");
    for name in names.split(',') {
        let msg = setglobal_message(&rpc, &mut incoming, &format!("{name}?")).await;
        assert!(
            msg.contains(name) && !msg.contains("E518"),
            "`:setglobal {name}?` must give a readout or a loud rejection, got {msg:?}"
        );
    }
}

// ---- Phase 2: the Lua surface over the same two tiers ----------------------

/// THE config-level bug: `vim.opt.tabstop = 3` in an `init.lua` has to reach the files
/// you open afterwards. `vim.opt`/`vim.o` write a buffer-local option in BOTH tiers, as
/// `:set` does — before this they forwarded to `vim.bo`, i.e. the one buffer that
/// happened to be current while the config ran.
#[tokio::test]
async fn vim_opt_reaches_buffers_opened_later() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optlua_opt_a", "txt", "one\n");
    let file = write_temp("optlua_opt_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    exec_lua(&rpc, "vim.opt.tabstop = 3 vim.opt.expandtab = true").await;
    assert_eq!(indent_of_current(&rpc).await, "ts=3 et=true", "this buffer");
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=true",
        "a file opened after the config line is born from the global value"
    );
}

/// `vim.bo` and `vim.opt_local` are the local-only surfaces — the ftplugin case, where a
/// per-buffer indent must not become everyone's default. (`vim.opt_local` used to be a
/// plain alias of `vim.opt`.)
#[tokio::test]
async fn vim_bo_and_opt_local_stay_on_this_buffer() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optlua_local_a", "txt", "one\n");
    let file = write_temp("optlua_local_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    exec_lua(&rpc, "vim.opt.tabstop = 3").await;
    exec_lua(&rpc, "vim.bo.tabstop = 8").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=8 et=false",
        "vim.bo write"
    );
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=false",
        "the next buffer still follows the global value"
    );
    exec_lua(&rpc, "vim.opt_local.tabstop = 9").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=9 et=false",
        "opt_local write"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.go.tabstop").await.as_i64(),
        Some(3),
        "and `vim.opt_local` left the global value alone"
    );
}

/// `vim.go` / `vim.opt_global` are the other half: they move what new buffers inherit
/// without touching the buffer you are in. Before, both fell into the `btv._o_store`
/// catch-all — readable back, honored by nothing.
#[tokio::test]
async fn vim_go_writes_the_tier_only() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optlua_go_a", "txt", "one\n");
    let file = write_temp("optlua_go_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    exec_lua(&rpc, "vim.go.tabstop = 2").await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=4 et=false",
        "the current buffer is untouched"
    );
    edit(&rpc, &file).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=2 et=false",
        "while the next buffer is born from it"
    );
    exec_lua(&rpc, "vim.opt_global.expandtab = true").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.go.expandtab").await.as_bool(),
        Some(true),
        "`vim.opt_global` reaches the same tier `vim.go` does"
    );
}

/// The two tiers read back separately from Lua, and `vim.go` reports the core's value —
/// including one set through the `:set` ex path, not just one written from Lua.
#[tokio::test]
async fn vim_o_reads_the_buffer_and_vim_go_reads_the_tier() {
    let (rpc, _incoming) = start().await;
    command(&rpc, "set tabstop=3").await;
    command(&rpc, "setlocal tabstop=8").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.tabstop").await.as_i64(),
        Some(8),
        "`vim.o` reads the current buffer"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.go.tabstop").await.as_i64(),
        Some(3),
        "`vim.go` reads the global value the `:set` moved"
    );
}

/// A global-scope option has a single value, so all three surfaces reach it — the scope
/// split is only meaningful for the buffer-local ones.
#[tokio::test]
async fn a_global_scope_option_is_the_same_in_every_scope() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "vim.opt_global.ignorecase = true").await;
    let read = exec_lua(
        &rpc,
        "return tostring(vim.o.ignorecase) .. ',' .. tostring(vim.go.ignorecase) \
         .. ',' .. tostring(vim.opt_local.ignorecase:get())",
    )
    .await;
    assert_eq!(read.as_str(), Some("true,true,true"));
}

// ---- Phase 3: the window tier, and the map-backed buffer nouns -------------

/// Window options get the same two tiers. A split copies the window it came from (vim's
/// rule, and how a config's window settings already propagated), so the tier's own job is
/// `:setglobal` / `vim.go` — and seeding a window minted with no source to copy.
#[tokio::test]
async fn window_options_have_a_global_value() {
    let (rpc, mut incoming) = start().await;
    // `:set` moves both tiers…
    command(&rpc, "set scrolloff=7").await;
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "scrolloff?").await,
        "scrolloff=7"
    );
    // …`:setlocal` only this window…
    command(&rpc, "setlocal scrolloff=2").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "scrolloff?").await,
        "scrolloff=2"
    );
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "scrolloff?").await,
        "scrolloff=7",
        "the global value is untouched by `:setlocal`"
    );
    // …and `:setglobal` only the tier.
    command(&rpc, "setglobal scrolloff=9").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "scrolloff?").await,
        "scrolloff=2",
        "this window keeps its own value"
    );
    // Booleans and strings ride the same split.
    command(&rpc, "setglobal nonumber").await;
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "number?").await,
        "nonumber"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "number?").await,
        "number",
        "the focused window still has its own value"
    );
    command(&rpc, "setglobal signcolumn=no").await;
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "signcolumn?").await,
        "signcolumn=no"
    );
}

/// A split still copies the window it came from — the tier must not quietly replace
/// vim's inheritance rule.
#[tokio::test]
async fn a_split_still_copies_the_source_window() {
    let (rpc, _incoming) = start().await;
    command(&rpc, "setlocal scrolloff=6").await;
    command(&rpc, "split").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.scrolloff").await.as_i64(),
        Some(6),
        "the new split inherited the window it was split from, not the global value"
    );
}

/// A window with no source window to copy — a dock is a fresh layer, not a split — is
/// born from the global value instead.
#[tokio::test]
async fn a_dock_window_is_born_from_the_window_tier() {
    let (rpc, _incoming) = start().await;
    command(&rpc, "setglobal scrolloff=4").await;
    // The dock op drains after the chunk, so read its window in a SECOND chunk.
    exec_lua(&rpc, r#"btv.dock.open{ side = "left", size = 20 }"#).await;
    let so = exec_lua(&rpc, "return btv.wo[btv.win.current()].scrolloff").await;
    assert_eq!(
        so.as_i64(),
        Some(4),
        "the dock's window started from the global value"
    );
}

/// `vim.go` / `vim.opt_global` reach the window tier from Lua, and `vim.wo` stays local.
#[tokio::test]
async fn vim_go_reaches_the_window_tier() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "vim.go.scrolloff = 5").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.scrolloff").await.as_i64(),
        Some(0),
        "`vim.go` left the focused window alone"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.go.scrolloff").await.as_i64(),
        Some(5),
        "and the tier reads back"
    );
    exec_lua(&rpc, "vim.opt.scrolloff = 3").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.scrolloff .. ',' .. vim.go.scrolloff")
            .await
            .as_str(),
        Some("3,3"),
        "`vim.opt` moves both, as `:set` does"
    );
}

/// The config pattern the map-backed nouns exist for: `foldmethod` and `foldexpr` are set
/// together, and BOTH have to reach the next buffer or folding silently does nothing
/// there. `foldexpr` lives in a per-buffer map, so its tier is a read-time fallback — a
/// buffer with no expression of its own follows the global one.
#[tokio::test]
async fn foldexpr_and_commentstring_follow_the_global_value() {
    let (rpc, mut incoming) = start().await;
    let seed = write_temp("optglobal_fold_a", "txt", "one\n");
    let file = write_temp("optglobal_fold_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    command(&rpc, "set foldmethod=expr").await;
    command(&rpc, "set foldexpr=v:lua.btv.treesitter.foldexpr()").await;
    command(&rpc, "set commentstring=#%s").await;
    edit(&rpc, &file).await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldexpr?").await,
        "foldexpr=v:lua.btv.treesitter.foldexpr()",
        "the next buffer folds by the global expression"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldmethod?").await,
        "foldmethod=expr",
        "…and by the global method, so the pair works together"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=#%s"
    );
    // `:setlocal` still pins one buffer without moving the global value.
    command(&rpc, "setlocal commentstring=;;%s").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=;;%s"
    );
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=#%s"
    );
}

/// The deliberate deviation from vim, which nothing covered: the map-backed nouns resolve
/// their tier as a **read-time fallback**, not a creation seed (their `HashMap` storage
/// already encodes "unset" as absence). So a `:setglobal` reaches buffers that are ALREADY
/// open and have no value of their own — where a slot-backed option like `tabstop` only
/// reaches buffers created afterwards. Both halves are asserted here so the two behaviors
/// can't silently converge.
#[tokio::test]
async fn a_late_setglobal_reaches_the_already_open_map_backed_nouns() {
    let (rpc, mut incoming) = start().await;
    let file = write_temp("optglobal_late_cms", "txt", "one\n");
    edit(&rpc, &file).await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=",
        "no filetype default for .txt, and no global value yet"
    );
    command(&rpc, "setglobal commentstring=##\\ %s").await;
    command(&rpc, "setglobal foldmarker=<<<,>>>").await;
    command(&rpc, "setglobal tabstop=3").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=## %s",
        "a map-backed noun falls back to the tier at READ time, so this open buffer follows"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldmarker?").await,
        "foldmarker=<<<,>>>",
        "…and so does `foldmarker`"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "tabstop?").await,
        "tabstop=4",
        "a slot-backed option is a creation SEED — this buffer was born before the write"
    );
    // A buffer of its own overrides the tier, and keeps doing so afterwards.
    command(&rpc, "setlocal commentstring=;;\\ %s").await;
    command(&rpc, "setglobal commentstring=//\\ %s").await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=;; %s",
        "the buffer's own value still wins over a later global one"
    );
}

/// `nvim_set_option_value` is the third spelling of a `:set`, and neovim documents it as
/// matching `:set` exactly: "for global-local options, both the global and local value
/// are set unless otherwise specified with {scope}". It kept writing the local value
/// only — so the config bug this whole model closes (`vim.opt.tabstop = 3` never reaching
/// files opened later) stayed open on the `vim.api` spelling of the same write.
///
/// The two narrowing forms still pin one instance, as neovim specifies: an explicit
/// `scope = "local"`, and a `buf` / `win` target (which implies local).
#[tokio::test]
async fn nvim_set_option_value_writes_both_tiers_like_set() {
    let (rpc, _incoming) = start().await;
    let seed = write_temp("optapi_a", "txt", "one\n");
    let later = write_temp("optapi_b", "txt", "two\n");
    let last = write_temp("optapi_c", "txt", "three\n");
    edit(&rpc, &seed).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_set_option_value("tabstop", 3, {})
           vim.api.nvim_set_option_value("expandtab", true, {})"#,
    )
    .await;
    assert_eq!(indent_of_current(&rpc).await, "ts=3 et=true", "this buffer");
    edit(&rpc, &later).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=true",
        "a scope-less `nvim_set_option_value` is a `:set` — it moves the tier too"
    );
    // …and the narrowing forms stay on one buffer.
    exec_lua(
        &rpc,
        r#"vim.api.nvim_set_option_value("tabstop", 7, { scope = "local" })
           vim.api.nvim_set_option_value("shiftwidth", 5, { buf = btv.buf.current() })"#,
    )
    .await;
    let pinned = exec_lua(
        &rpc,
        r#"local b = btv.buf.current()
           return btv.bo[b].tabstop .. "|" .. btv.bo[b].shiftwidth
                  .. "|" .. vim.go.tabstop .. "|" .. vim.go.shiftwidth"#,
    )
    .await;
    assert_eq!(
        pinned.as_str().unwrap_or_default(),
        "7|5|3|0",
        "`scope = \"local\"` and a `buf` target write the buffer, never the tier"
    );
    edit(&rpc, &last).await;
    assert_eq!(
        indent_of_current(&rpc).await,
        "ts=3 et=true",
        "so the next file is still born from the tier the scope-less write set"
    );
}

/// A buffer option with NO global value must say **that** through `vim.go` /
/// `vim.opt_global`, exactly as `:setglobal` answers `E5100` — not the generic
/// "unknown option … a typo, or an option bemtvi doesn't model" warning, which is
/// false (bemtvi models `'fileencoding'` fully; it just has no tier) and sends the
/// reader hunting for a misspelling. The write is still rejected rather than dropped
/// into `btv._o_store`, where a later read would hand back a value nothing honors.
///
/// Enumerated from the catalog's `global_tier` column, so the loud-and-accurate
/// rejection covers every tier-less name and any added later.
#[tokio::test]
async fn vim_go_of_a_tierless_option_says_it_has_no_global_value() {
    let (rpc, _incoming) = start().await;
    let out = exec_lua(
        &rpc,
        r#"local bad = {}
           for _, r in ipairs(btv._options_catalog) do
             if r.scope == "buffer" and not r.global_tier then
               local seen = {}
               local old = btv.notify
               btv.notify = function(m) seen[#seen + 1] = tostring(m) end
               local ok, err = pcall(function() vim.opt_global[r.name] = "x" end)
               btv.notify = old
               local said = table.concat(seen, " ")
               if not ok then
                 bad[#bad + 1] = r.name .. "(raised: " .. tostring(err) .. ")"
               elseif not said:find("no global value", 1, true) then
                 bad[#bad + 1] = r.name .. "(said: " .. said .. ")"
               elseif btv._o_store[r.name] ~= nil then
                 bad[#bad + 1] = r.name .. "(stored a value nothing honors)"
               end
             end
           end
           return table.concat(bad, ", ")"#,
    )
    .await;
    assert_eq!(
        out.as_str().unwrap_or_default(),
        "",
        "`vim.go` on a tier-less option must name the real reason, as `:setglobal` does"
    );
}

/// With no global value set, `commentstring` still falls through to the filetype's
/// built-in template — the tier is a fallback, not a floor.
#[tokio::test]
async fn an_unset_commentstring_tier_keeps_the_filetype_default() {
    let (rpc, mut incoming) = start().await;
    let file = write_temp("optglobal_cms_ft", "lua", "return 1\n");
    edit(&rpc, &file).await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=-- %s",
        "the lua filetype default still wins when no global value was set"
    );
}

// ---- The Lua scope routing: `vim.opt` must reach EVERY buffer/window option ----

/// The catalog-driven twin of `every_known_option_is_wired_not_silent`, for the **Lua**
/// surface: every buffer- or window-scoped option in `btv._options_catalog` must be
/// *routed* by `vim.opt` / `vim.o` to its scope — never fall into the `btv._o_store`
/// catch-all, which is where an unmodeled name goes and where nothing reads it again.
///
/// The routing tables were hand-kept name lists that drifted from the catalog, so
/// `vim.opt.foldmethod = "marker"` (and nine others) silently stored and did nothing.
/// Enumerating from the catalog is what keeps them from drifting again.
#[tokio::test]
async fn every_scoped_option_is_routed_by_vim_opt() {
    let (rpc, _incoming) = start().await;
    let unrouted = exec_lua(
        &rpc,
        r#"local bad = {}
           for _, r in ipairs(btv._options_catalog) do
             if r.scope == "buffer" or r.scope == "window" then
               -- `btv.o` routes by scope; an unrouted name lands in the catch-all.
               if btv._o_route(r.name) == nil then
                 bad[#bad + 1] = r.name .. "(" .. r.scope .. ")"
               end
               if r.abbrev and btv._o_route(r.abbrev) == nil then
                 bad[#bad + 1] = r.abbrev .. "(" .. r.scope .. ", abbrev)"
               end
             end
           end
           return table.concat(bad, ",")"#,
    )
    .await;
    assert_eq!(
        unrouted.as_str().unwrap_or_default(),
        "",
        "every buffer/window option must be routed by `vim.opt`, not stored"
    );
}

/// Routing a name is only half of it: the value has to land in the **core**. Every
/// buffer-scoped number/boolean option must come back changed after a `btv.bo` write —
/// the per-buffer Lua bridge (`Editor::set_buffer_option_num` / `_bool`) matches on the
/// name and silently `return`s on anything it does not know, so a catalog entry with no
/// arm there reads back its default forever.
///
/// This is `every_known_option_is_wired_not_silent`'s hole: that guard covers the `:set`
/// ex path, where an unwired name is a loud `E518`. The Lua bridge has no such error to
/// assert on, so the write-then-read-back *is* the assertion. It caught `'undolevels'`,
/// which routed fine and set nothing.
///
/// The probe value is "the current one, moved": `n + 1` for a number, the flipped
/// boolean. That is in range for every buffer option today (`softtabstop` -1 → 0 is the
/// tightest); an option with a narrower range needs its own value here rather than a
/// silent skip.
///
/// String options are deliberately out: "a different *valid* value" is not derivable
/// from the catalog (`foldmethod` takes an enum, `regexsyntax` two spellings), so they
/// are covered by name in the tests around this one.
#[tokio::test]
async fn every_buffer_option_write_from_lua_reaches_the_core() {
    let (rpc, _incoming) = start().await;
    let rows = exec_lua(
        &rpc,
        "local o = {} \
         for _, r in ipairs(btv._options_catalog) do \
           if r.scope == 'buffer' and r.kind ~= 'string' then \
             o[#o + 1] = r.name .. '|' .. r.kind \
           end \
         end \
         return table.concat(o, ',')",
    )
    .await;
    let rows = rows.as_str().expect("catalog rows join").to_string();
    assert!(
        rows.split(',').count() >= 10,
        "the catalog should enumerate the buffer options, got {rows:?}"
    );

    let mut dropped = Vec::new();
    for row in rows.split(',') {
        let (name, kind) = row.split_once('|').expect("name|kind");
        let before = exec_lua(&rpc, &format!("return btv.bo[0].{name}")).await;
        let want = if kind == "bool" {
            format!("{}", before.as_bool() != Some(true))
        } else {
            format!("{}", before.as_i64().expect("a number reads back") + 1)
        };
        exec_lua(&rpc, &format!("btv.bo[0].{name} = {want}")).await;
        // A separate round trip: the write echoes into the Lua mirror for
        // read-after-write, so only the server's next push shows what the core took.
        let after = exec_lua(&rpc, &format!("return btv.bo[0].{name}")).await;
        let got = if kind == "bool" {
            format!("{}", after.as_bool() == Some(true))
        } else {
            format!("{}", after.as_i64().unwrap_or(i64::MIN))
        };
        if got != want {
            dropped.push(format!("{name}: wrote {want}, read back {got}"));
        }
    }
    assert!(
        dropped.is_empty(),
        "a `btv.bo` write on these buffer options never reached the core: {dropped:?}"
    );
}

/// The exact config pattern the global-local plan set out to close: `vim.opt.foldmethod`
/// next to `vim.opt.foldexpr` has to fold every buffer, this one and the next. Both
/// halves used to land in `btv._o_store` — the core honored `:set foldmethod=expr`, but
/// the `vim.opt` spelling a config actually writes never reached it.
#[tokio::test]
async fn vim_opt_reaches_the_map_backed_buffer_nouns() {
    let (rpc, mut incoming) = start().await;
    let seed = write_temp("optroute_fold_a", "txt", "one\n");
    let file = write_temp("optroute_fold_b", "txt", "two\n");
    edit(&rpc, &seed).await;
    exec_lua(
        &rpc,
        r#"vim.opt.foldmethod = "marker"
           vim.opt.foldexpr = "v:lua.btv.treesitter.foldexpr()"
           vim.opt.foldmarker = "<<<,>>>"
           vim.opt.commentstring = "// %s""#,
    )
    .await;
    // This buffer took the local half…
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldmethod?").await,
        "foldmethod=marker"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldmarker?").await,
        "foldmarker=<<<,>>>"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=// %s"
    );
    // …and a file opened afterwards is born from the global half.
    edit(&rpc, &file).await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldmethod?").await,
        "foldmethod=marker",
        "a buffer opened after the config line inherits the global value"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldexpr?").await,
        "foldexpr=v:lua.btv.treesitter.foldexpr()"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "commentstring?").await,
        "commentstring=// %s"
    );
}

/// The numeric/boolean buffer options the routing table also missed
/// (`indentemptylines`, `foldnestmax`, `foldminlines`).
#[tokio::test]
async fn vim_opt_reaches_the_remaining_buffer_options() {
    let (rpc, mut incoming) = start().await;
    let file = write_temp("optroute_buf", "txt", "one\n");
    exec_lua(
        &rpc,
        r#"vim.opt.indentemptylines = true
           vim.opt.foldnestmax = 5
           vim.opt.foldminlines = 2"#,
    )
    .await;
    edit(&rpc, &file).await;
    assert_eq!(
        set_message(&rpc, &mut incoming, "indentemptylines?").await,
        "indentemptylines"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldnestmax?").await,
        "foldnestmax=5"
    );
    assert_eq!(
        set_message(&rpc, &mut incoming, "foldminlines?").await,
        "foldminlines=2"
    );
}

/// The window options the routing table missed. `vim.opt` writes the focused window
/// AND the tier, so `:set x?` and `:setglobal x?` both read back the new value.
#[tokio::test]
async fn vim_opt_reaches_the_remaining_window_options() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.opt.foldcolumn = 3
           vim.opt.foldlevel = 2
           vim.opt.foldenable = false
           vim.opt.breakindent = true
           vim.opt.showbreak = "> "
           vim.opt.breakindentopt = "sbr"
           vim.opt.sidescroll = 7
           vim.opt.sidescrolloff = 4
           vim.opt.padding = "1 2""#,
    )
    .await;
    for (query, expected) in [
        ("foldcolumn?", "foldcolumn=3"),
        ("foldlevel?", "foldlevel=2"),
        ("foldenable?", "nofoldenable"),
        ("breakindent?", "breakindent"),
        ("showbreak?", "showbreak=> "),
        ("breakindentopt?", "breakindentopt=sbr"),
        ("sidescroll?", "sidescroll=7"),
        ("sidescrolloff?", "sidescrolloff=4"),
        ("padding?", "padding=1 2"),
    ] {
        assert_eq!(
            set_message(&rpc, &mut incoming, query).await,
            expected,
            "`vim.opt` must reach the focused window for {query}"
        );
        assert_eq!(
            setglobal_message(&rpc, &mut incoming, query).await,
            expected,
            "`vim.opt` must move the global tier too for {query}"
        );
    }
}

/// …and those same window options must read back honestly through `vim.wo` — a write
/// that reaches the core but is invisible to the mirror is a half-wired option.
#[tokio::test]
async fn the_window_mirror_carries_every_routed_window_option() {
    let (rpc, _incoming) = start().await;
    command(
        &rpc,
        "set foldcolumn=3 foldlevel=2 nofoldenable breakindent",
    )
    .await;
    // An escaped space keeps the marker's trailing blank in one `:set` token.
    command(&rpc, "set showbreak=>\\ ").await;
    command(&rpc, "set breakindentopt=sbr").await;
    command(
        &rpc,
        "set sidescroll=7 sidescrolloff=4 winhighlight=Normal:NormalSB",
    )
    .await;
    let out = exec_lua(
        &rpc,
        r#"local w = btv.win.current()
           return table.concat({
             tostring(btv.wo[w].foldcolumn), tostring(btv.wo[w].foldlevel),
             tostring(btv.wo[w].foldenable), tostring(btv.wo[w].breakindent),
             btv.wo[w].showbreak, btv.wo[w].breakindentopt,
             tostring(btv.wo[w].sidescroll), tostring(btv.wo[w].sidescrolloff),
             btv.wo[w].winhighlight,
           }, "|")"#,
    )
    .await;
    assert_eq!(
        out.as_str().unwrap_or_default(),
        "3|2|false|true|> |sbr|7|4|Normal:NormalSB"
    );
}

/// …and the buffer mirror likewise, for the fold nouns `vim.bo` had no field for.
#[tokio::test]
async fn the_buffer_mirror_carries_the_fold_options() {
    let (rpc, _incoming) = start().await;
    command(&rpc, "set foldmethod=marker foldnestmax=5 foldminlines=2").await;
    command(&rpc, "set foldmarker=<<<,>>>").await;
    let out = exec_lua(
        &rpc,
        r#"local b = btv.buf.current()
           return table.concat({
             btv.bo[b].foldmethod, tostring(btv.bo[b].foldnestmax),
             tostring(btv.bo[b].foldminlines), btv.bo[b].foldmarker,
           }, "|")"#,
    )
    .await;
    assert_eq!(out.as_str().unwrap_or_default(), "marker|5|2|<<<,>>>");
}

/// `'scrollanim'` is a **global** option with a per-window override, so its global value
/// is the editor-wide one — not a window tier. `vim.go.scrollanim` read a window tier
/// that is never populated and so always answered `true`, disagreeing with `vim.o` and
/// even with its own abbreviation `vim.go.sca`.
#[tokio::test]
async fn vim_go_scrollanim_reads_the_editor_wide_value() {
    let (rpc, _incoming) = start().await;
    let read = |rpc: &Rpc| {
        let rpc = rpc.clone();
        async move {
            exec_lua(
                &rpc,
                r#"return tostring(vim.go.scrollanim) .. "|" .. tostring(vim.go.sca)
                          .. "|" .. tostring(vim.o.scrollanim)"#,
            )
            .await
            .as_str()
            .unwrap_or_default()
            .to_string()
        }
    };
    assert_eq!(read(&rpc).await, "true|true|true", "the default is on");
    command(&rpc, "set noscrollanim").await;
    assert_eq!(
        read(&rpc).await,
        "false|false|false",
        "`vim.go` must follow the editor-wide value, in both spellings"
    );
}

/// `'regexsyntax'` is global-local: a buffer's own value may be "follow the global",
/// which is what a fresh tier holds. `vim.go.regexsyntax` handed back that unresolved
/// "" instead of the dialect a new buffer will actually use — where `:setglobal rxs?`
/// resolves it. The two surfaces must agree.
#[tokio::test]
async fn vim_go_regexsyntax_resolves_like_setglobal() {
    let (rpc, mut incoming) = start().await;
    let read = |rpc: &Rpc| {
        let rpc = rpc.clone();
        async move {
            exec_lua(&rpc, "return vim.go.regexsyntax")
                .await
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };
    assert_eq!(read(&rpc).await, "pcre", "the resolved default");
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "regexsyntax?").await,
        "regexsyntax=pcre",
        "…and `:setglobal` agrees"
    );
    command(&rpc, "setglobal regexsyntax=vim").await;
    assert_eq!(read(&rpc).await, "vim");
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "regexsyntax?").await,
        "regexsyntax=vim"
    );
}

/// The catalog-driven twin of `every_scoped_option_is_routed_by_vim_opt`, one layer
/// further out: routing a name to `vim.go` is only half the wiring — the server has to
/// *push* that option's global value into the mirror `vim.go` reads. An option the
/// routing reaches but the mirror omits is silently read-only-wrong: the write lands in
/// the core, the Lua-side echo is wiped by the next server push, and the read falls back
/// to the built-in default forever after.
///
/// Enumerated from the catalog's own `global_tier` column so a newly-added option can't
/// slip past — `'winhighlight'` did exactly that (a catalog row and a real core tier,
/// but no `WoGlobalMirror` field).
#[tokio::test]
async fn every_tiered_option_is_carried_by_the_go_mirror() {
    let (rpc, _incoming) = start().await;
    let missing = exec_lua(
        &rpc,
        r#"local bad = {}
           for _, r in ipairs(btv._options_catalog) do
             if r.global_tier and (r.scope == "buffer" or r.scope == "window") then
               local mirror = r.scope == "window" and btv._wo_global or btv._bo_global
               -- `'regexsyntax'` is the one global-local name whose tier IS the
               -- editor-wide option (`btv._go_mirror`), not a per-scope tier table.
               if r.name ~= "regexsyntax" and mirror[r.name] == nil then
                 bad[#bad + 1] = r.name .. "(" .. r.scope .. ")"
               end
             end
           end
           return table.concat(bad, ",")"#,
    )
    .await;
    assert_eq!(
        missing.as_str().unwrap_or_default(),
        "",
        "every option with a global tier must be pushed into the mirror `vim.go` reads"
    );
}

/// …and the behavior that guard protects, for the option it caught: `'winhighlight'` has
/// a real `:setglobal` tier in the core (a dock window is born from it), so `vim.go` must
/// report what `:setglobal winhl=` wrote rather than the empty default.
#[tokio::test]
async fn vim_go_winhighlight_round_trips() {
    let (rpc, mut incoming) = start().await;
    command(&rpc, "setglobal winhighlight=Normal:NormalSB").await;
    let out = exec_lua(
        &rpc,
        r#"return vim.go.winhighlight .. "|" .. vim.go.winhl
                  .. "|" .. vim.opt_global.winhighlight:get()"#,
    )
    .await;
    assert_eq!(
        out.as_str().unwrap_or_default(),
        "Normal:NormalSB|Normal:NormalSB|Normal:NormalSB",
        "`vim.go` must read the tier `:setglobal winhl=` wrote, in both spellings"
    );
    assert_eq!(
        setglobal_message(&rpc, &mut incoming, "winhighlight?").await,
        "winhighlight=Normal:NormalSB",
        "…and agree with `:setglobal winhl?`"
    );
    // The tier is not the window: the focused window kept its own (empty) value.
    let win = exec_lua(
        &rpc,
        r#"return "[" .. btv.wo[btv.win.current()].winhighlight .. "]""#,
    )
    .await;
    assert_eq!(win.as_str().unwrap_or_default(), "[]");
}

/// Every window option the server mirrors into the `vim.go` tier must be *reachable*
/// through it — `breakindent` / `showbreak` / `breakindentopt` / `sidescroll` /
/// `sidescrolloff` were pushed into `btv._wo_global` and then read back as `nil`.
#[tokio::test]
async fn vim_go_reads_every_mirrored_window_option() {
    let (rpc, _incoming) = start().await;
    command(&rpc, "setglobal breakindent sidescroll=7 sidescrolloff=4").await;
    command(&rpc, "setglobal showbreak=>\\ ").await;
    command(&rpc, "setglobal breakindentopt=sbr").await;
    let out = exec_lua(
        &rpc,
        r#"return table.concat({
             tostring(vim.go.breakindent), vim.go.showbreak, vim.go.breakindentopt,
             tostring(vim.go.sidescroll), tostring(vim.go.sidescrolloff),
           }, "|")"#,
    )
    .await;
    assert_eq!(out.as_str().unwrap_or_default(), "true|> |sbr|7|4");
    // The tier is not the window: the focused window kept its own values.
    let win = exec_lua(
        &rpc,
        r#"local w = btv.win.current()
           return tostring(btv.wo[w].breakindent) .. "|" .. tostring(btv.wo[w].sidescroll)"#,
    )
    .await;
    assert_eq!(win.as_str().unwrap_or_default(), "false|1");
}

// -------------------------------------------------- the indent widths are bounded
//
// `'tabstop'` / `'shiftwidth'` / `'softtabstop'` feed `" ".repeat(n)` fills and
// `fill_indent`'s tab loop, so an unbounded value turns the next `<Tab>` or `>>`
// into a capacity-overflow panic (or an OOM). vim caps `tabstop` at 9999 and merely
// *accepts* an absurd `shiftwidth`, then hangs inserting that many spaces; bemtvi
// refuses all three above 10000, loudly.

#[tokio::test]
async fn an_absurd_tabstop_is_refused_loudly() {
    let (rpc, mut incoming) = start().await;
    let before = exec_lua(&rpc, "return vim.bo.tabstop").await.as_i64();
    let msg = set_message(&rpc, &mut incoming, "tabstop=100000").await;
    assert!(
        msg.contains("E474"),
        "an out-of-range 'tabstop' must fail loud, got {msg:?}"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.tabstop").await.as_i64(),
        before,
        "the refused value must leave the option untouched"
    );
}

#[tokio::test]
async fn an_absurd_shiftwidth_is_refused_loudly() {
    let (rpc, mut incoming) = start().await;
    let msg = set_message(&rpc, &mut incoming, "shiftwidth=999999").await;
    assert!(msg.contains("E474"), "got {msg:?}");
}

#[tokio::test]
async fn an_absurd_softtabstop_is_refused_loudly() {
    let (rpc, mut incoming) = start().await;
    let msg = set_message(&rpc, &mut incoming, "softtabstop=50000").await;
    assert!(msg.contains("E474"), "got {msg:?}");
}

#[tokio::test]
async fn a_large_but_allowed_tabstop_is_accepted() {
    // The boundary the other way: 10000 is in range, so the cap must not reject
    // every big-but-legal value.
    let (rpc, mut incoming) = start().await;
    let msg = set_message(&rpc, &mut incoming, "tabstop=10000").await;
    assert!(!msg.contains("E474"), "10000 is in range, got {msg:?}");
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.tabstop").await.as_i64(),
        Some(10000),
    );
}

#[tokio::test]
async fn a_huge_tabstop_written_from_lua_is_clamped_not_fatal() {
    // The `vim.bo` bridge follows this crate's ignore-garbage convention rather
    // than failing loud, but it must still not leave a value that panics the next
    // indent fill.
    let (rpc, _i) = start().await;
    exec_lua(&rpc, "vim.bo.tabstop = 100000000").await;
    let ts = exec_lua(&rpc, "return vim.bo.tabstop").await.as_i64();
    assert!(
        ts.is_some_and(|n| (1..=10000).contains(&n)),
        "a Lua-written 'tabstop' must be clamped into range, got {ts:?}"
    );
    // And the editor survives actually using it.
    command(&rpc, "normal! i\t").await;
    assert!(
        exec_lua(&rpc, "return 1").await.as_i64() == Some(1),
        "still alive"
    );
}

#[tokio::test]
async fn foldnestmax_zero_is_accepted() {
    // vim allows `foldnestmax=0` (it collapses every fold); it used to be rejected
    // here as "must be positive" alongside the genuinely-1-minimum options.
    let (rpc, mut incoming) = start().await;
    let msg = set_message(&rpc, &mut incoming, "foldnestmax=0").await;
    assert!(
        !msg.contains("E487"),
        "`foldnestmax=0` is legal in vim, got {msg:?}"
    );
}

/// `:set a? b?` reports **both**, the way vim does — one message line carrying every
/// query in the command. Each token echoed on its own, so only the last one was
/// visible and the earlier answers were lost off the bottom of the screen.
#[tokio::test]
async fn a_multi_option_query_reports_every_option() {
    let (rpc, mut incoming) = start().await;
    let msg = set_message(&rpc, &mut incoming, "scrolloff? wrap?").await;
    assert!(
        msg.contains("scrolloff=0") && msg.contains("nowrap"),
        "both queries on the message line, got {msg:?}"
    );
    // A single query is unchanged.
    let msg = set_message(&rpc, &mut incoming, "wrap?").await;
    assert_eq!(msg, "nowrap");
    // An unknown name among them still fails loud.
    let msg = set_message(&rpc, &mut incoming, "wrap? nosuchopt?").await;
    assert!(
        msg.contains("E518"),
        "unknown option still loud, got {msg:?}"
    );
}
