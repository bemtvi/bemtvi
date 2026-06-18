//! The `nx.*` namespace foundation: `nx` is the canonical config surface, and a
//! write through it drives the *same* machinery as the `vim.*` alias. Each test
//! exercises a real effect (a value reaching the core, an autocmd firing, a user
//! command running, a mapping triggering) rather than asserting pointer equality,
//! so it proves `nx.*` is wired, not just aliased to a table.
//!
//! The entity natives (`nx.buf` / `nx.win` / `nx.cursor` / `nx.hl` / `nx.ns`,
//! migrated from `vim.api.nvim_*`) are exercised the same way: drive the canonical
//! `nx.*` name, then confirm the `nvim_*` alias observes the same effect.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{drain_to_latest_redraw, exec_lua, feed, field_str, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

#[tokio::test]
async fn nx_o_reaches_the_core_and_vim_o_reads_it_back() {
    let (rpc, mut incoming) = start().await;

    // Set through the canonical `nx.o`; the value must reach the core (the same
    // path `vim.o` drives), read back through the `vim.o` alias, and reach the UI.
    exec_lua(&rpc, "nx.o.guifont = 'Fira Code:h14'").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.guifont").await.as_str(),
        Some("Fira Code:h14"),
        "a value set through nx.o reads back through the vim.o alias"
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        field_str(&frame, "guifont"),
        "Fira Code:h14",
        "nx.o reaches the core and the redraw, like vim.o"
    );
}

#[tokio::test]
async fn nx_g_and_vim_g_share_one_store() {
    let (rpc, _incoming) = start().await;
    // Written through one name, visible through the other — the same variable store.
    exec_lua(&rpc, "nx.g.nx_marker = 42").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_marker").await.as_i64(),
        Some(42)
    );
    exec_lua(&rpc, "vim.g.vim_marker = 7").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.g.vim_marker").await.as_i64(),
        Some(7)
    );
}

#[tokio::test]
async fn nx_on_registers_an_autocmd_that_fires() {
    let (rpc, _incoming) = start().await;

    // Register via the canonical `nx.on(event, opts, fn)`; firing the event (the
    // manual `nvim_exec_autocmds` path) must run the handler.
    exec_lua(
        &rpc,
        "nx.on('User', { pattern = 'NxTest' }, function() vim.g.nx_fired = true end)",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'NxTest' })",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_fired").await.as_bool(),
        Some(true),
        "an autocmd registered through nx.on fires"
    );
}

#[tokio::test]
async fn nx_autocmd_clear_removes_matching_autocmds_and_nvim_alias_agrees() {
    let (rpc, _incoming) = start().await;

    // Two autocmds on distinct events; nx.autocmd.clear with an event filter drops
    // only the matching one. The cleared handler must no longer fire.
    exec_lua(
        &rpc,
        "nx.on('User', { pattern = 'NxClr' }, function() vim.g.nx_clr_fired = true end)\n\
         nx.on('User', { pattern = 'NxKeep' }, function() vim.g.nx_keep_fired = true end)",
    )
    .await;
    exec_lua(
        &rpc,
        "nx.autocmd.clear({ event = 'User', pattern = 'NxClr' })",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'NxClr' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'NxKeep' })",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_clr_fired").await.as_bool(),
        None,
        "the autocmd cleared through nx.autocmd.clear no longer fires"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_keep_fired").await.as_bool(),
        Some(true),
        "a non-matching autocmd survives the clear"
    );

    // The nvim_clear_autocmds alias is the same native — clearing through it drops
    // the survivor too.
    exec_lua(&rpc, "vim.api.nvim_clear_autocmds({ event = 'User' })").await;
    exec_lua(&rpc, "vim.g.nx_keep_fired = false").await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'NxKeep' })",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_keep_fired").await.as_bool(),
        Some(false),
        "nvim_clear_autocmds aliases nx.autocmd.clear (the survivor is gone too)"
    );
}

#[tokio::test]
async fn vim_fn_migrated_to_nx_natives_with_aliases() {
    let (rpc, _incoming) = start().await;

    // The canonical nx.* native and its vim.fn.* alias are the same function.
    // nx.jumplist.get (the requested example): an empty window jumplist round-trips.
    assert_eq!(
        exec_lua(
            &rpc,
            "local j = nx.jumplist.get(0); return tostring(#j[1]) .. ',' .. tostring(j[2])"
        )
        .await
        .as_str(),
        Some("0,0"),
        "nx.jumplist.get returns an empty list and index 0 for a fresh window"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return tostring(nx.jumplist.get == vim.fn.getjumplist)"
        )
        .await
        .as_str(),
        Some("true"),
        "vim.fn.getjumplist is an alias of nx.jumplist.get"
    );

    // A string helper: nx.str.chars and the vim.fn.strchars alias agree.
    assert_eq!(
        exec_lua(&rpc, "return nx.str.chars('héllo')")
            .await
            .as_i64(),
        Some(5),
        "nx.str.chars counts codepoints"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.strchars('héllo')")
            .await
            .as_i64(),
        Some(5),
        "vim.fn.strchars aliases nx.str.chars"
    );

    // A noun with state: nx.env.set then nx.env.get / vim.fn.getenv read it back.
    exec_lua(&rpc, "nx.env.set('NX_FN_MARK', 'yes')").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.env.get('NX_FN_MARK')")
            .await
            .as_str(),
        Some("yes"),
        "nx.env.set / nx.env.get round-trip"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getenv('NX_FN_MARK')")
            .await
            .as_str(),
        Some("yes"),
        "vim.fn.getenv aliases nx.env.get"
    );
}

#[tokio::test]
async fn nx_command_defines_a_runnable_user_command() {
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        "nx.command('NxPing', function() vim.g.nx_pinged = true end, {})",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxPing')").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_pinged").await.as_bool(),
        Some(true),
        "a :command defined through nx.command runs"
    );
}

#[tokio::test]
async fn nx_keymap_set_drives_a_normal_mode_mapping() {
    let (rpc, _incoming) = start().await;

    // A normal-mode mapping registered through nx.keymap must trigger on input.
    exec_lua(
        &rpc,
        "nx.keymap.set('n', 'Q', function() vim.g.nx_mapped = true end)",
    )
    .await;
    feed(&rpc, "Q");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_mapped").await.as_bool(),
        Some(true),
        "a mapping set through nx.keymap.set fires on the mapped key"
    );
}

#[tokio::test]
async fn nx_hl_define_reaches_get_and_nvim_alias() {
    let (rpc, _incoming) = start().await;

    // Define through the canonical nx.hl.define; nx.hl.get and the nvim_get_hl
    // alias must both read the group back with its `bold` attribute.
    exec_lua(
        &rpc,
        "nx.hl.define(0, 'NxTestHl', { fg = '#ff0000', bold = true })",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return nx.hl.get(0, { name = 'NxTestHl' }).bold")
            .await
            .as_bool(),
        Some(true),
        "nx.hl.define reaches nx.hl.get"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'NxTestHl' }).bold"
        )
        .await
        .as_bool(),
        Some(true),
        "the nvim_get_hl alias reads what nx.hl.define wrote"
    );
}

#[tokio::test]
async fn nx_user_command_create_runs_and_nvim_alias_agrees() {
    let (rpc, _incoming) = start().await;

    // Define through the canonical nx.user_command.create; `:NxUcA` must run it.
    exec_lua(
        &rpc,
        "nx.user_command.create('NxUcA', function() vim.g.nx_uc_a = true end, {})",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxUcA')").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_uc_a").await.as_bool(),
        Some(true),
        "a command defined through nx.user_command.create runs"
    );

    // The nvim_create_user_command alias must be the same native — a command
    // defined through it dispatches identically.
    exec_lua(
        &rpc,
        "vim.api.nvim_create_user_command('NxUcB', function() vim.g.nx_uc_b = true end, {})",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxUcB')").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_uc_b").await.as_bool(),
        Some(true),
        "the nvim_create_user_command alias defines the same kind of command"
    );
}

#[tokio::test]
async fn nx_autocmd_create_fires_then_del_stops_it() {
    let (rpc, _incoming) = start().await;

    // Register through the canonical nx.autocmd.create; firing via nx.autocmd.exec
    // must run the handler.
    let id = exec_lua(
        &rpc,
        "vim.g.nx_au_count = 0
         return nx.autocmd.create('User', { pattern = 'NxAu',
           callback = function() vim.g.nx_au_count = vim.g.nx_au_count + 1 end })",
    )
    .await
    .as_i64()
    .expect("nx.autocmd.create returns an id");
    exec_lua(&rpc, "nx.autocmd.exec('User', { pattern = 'NxAu' })").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_au_count").await.as_i64(),
        Some(1),
        "an autocmd registered through nx.autocmd.create fires"
    );

    // nx.autocmd.del removes it; a second fire must NOT run the handler again.
    exec_lua(&rpc, &format!("nx.autocmd.del({id})")).await;
    exec_lua(&rpc, "nx.autocmd.exec('User', { pattern = 'NxAu' })").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_au_count").await.as_i64(),
        Some(1),
        "nx.autocmd.del stops the autocmd from firing again"
    );

    // The nvim_get_autocmds alias observes the same (now empty) registry slice.
    assert_eq!(
        exec_lua(
            &rpc,
            "return #vim.api.nvim_get_autocmds({ event = 'User' })"
        )
        .await
        .as_i64(),
        Some(0),
        "nvim_get_autocmds sees the deletion nx.autocmd.del made"
    );
}

#[tokio::test]
async fn nx_option_set_get_round_trip_and_nvim_alias_agrees() {
    let (rpc, _incoming) = start().await;

    // Set a buffer-scoped option by name through the canonical nx.option.set; both
    // nx.option.get and the nvim_get_option_value alias must read it back.
    exec_lua(&rpc, "nx.option.set('nx_marker_opt', 'hi', { buf = 0 })").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.option.get('nx_marker_opt', { buf = 0 })")
            .await
            .as_str(),
        Some("hi"),
        "nx.option.set / nx.option.get round-trip"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_option_value('nx_marker_opt', { buf = 0 })"
        )
        .await
        .as_str(),
        Some("hi"),
        "the nvim_get_option_value alias reads what nx.option.set wrote"
    );
}

#[tokio::test]
async fn nvim_win_option_alias_round_trips_through_the_window_scope() {
    let (rpc, _incoming) = start().await;

    // The deprecated window-scoped option API carries no implementation of its
    // own — it wraps nvim_set/get_option_value with the scope pinned to a window.
    // A set through nvim_win_set_option must read back through nvim_win_get_option
    // and land in the canonical nx.wo scope (relativenumber defaults on).
    exec_lua(
        &rpc,
        "vim.api.nvim_win_set_option(0, 'relativenumber', false)",
    )
    .await;
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_win_get_option(0, 'relativenumber')"
        )
        .await
        .as_bool(),
        Some(false),
        "nvim_win_get_option reads what nvim_win_set_option wrote"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.wo[0].relativenumber")
            .await
            .as_bool(),
        Some(false),
        "the write lands in the canonical nx.wo window scope"
    );
}

#[tokio::test]
async fn nx_exec_captures_autocmd_listing_and_nvim_alias_agrees() {
    let (rpc, _incoming) = start().await;

    // Register an autocmd via the ex front-end through nx.exec (no capture), then
    // capture the `:autocmd` listing back — the lualine dedupe pattern.
    exec_lua(&rpc, "nx.exec('autocmd User NxExecPat echo hi', false)").await;
    assert_eq!(
        exec_lua(
            &rpc,
            "return (nx.exec('autocmd User', true):find('NxExecPat') ~= nil)"
        )
        .await
        .as_bool(),
        Some(true),
        "nx.exec captures the autocmd listing it registered"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return (vim.api.nvim_exec('autocmd User', true):find('NxExecPat') ~= nil)"
        )
        .await
        .as_bool(),
        Some(true),
        "the nvim_exec alias captures the same listing"
    );
}

#[tokio::test]
async fn nx_str_width_measures_cells_natively() {
    let (rpc, _incoming) = start().await;
    // ASCII is one cell per char.
    assert_eq!(
        exec_lua(&rpc, "return nx.str.width('hello')")
            .await
            .as_i64(),
        Some(5)
    );
    // Wide (CJK) graphemes count as two cells each — the native unicode-width
    // measure, not a byte/char count (which would report 3 / 9).
    assert_eq!(
        exec_lua(&rpc, "return nx.str.width('日本語')")
            .await
            .as_i64(),
        Some(6),
        "wide CJK graphemes count as two cells (not bytes or chars)"
    );
    // A zero-width combining mark adds nothing: 'e' + combining acute.
    assert_eq!(
        exec_lua(&rpc, "return nx.str.width('e\\u{0301}')")
            .await
            .as_i64(),
        Some(1),
        "a combining mark contributes zero cells"
    );

    // The neovim-compat aliases (nx.strwidth / nvim_strwidth) now route to the
    // same native helper — they used to run a coarser pure-Lua heuristic that
    // sized this combining mark as a second cell (returning 2).
    for expr in [
        "return nx.strwidth('e\\u{0301}')",
        "return vim.api.nvim_strwidth('e\\u{0301}')",
    ] {
        assert_eq!(
            exec_lua(&rpc, expr).await.as_i64(),
            Some(1),
            "nx.strwidth / nvim_strwidth share the native measure: {expr}"
        );
    }
}

#[tokio::test]
async fn nx_align_pads_lines_to_width() {
    let (rpc, _incoming) = start().await;
    // left: text at the start, padding on the right.
    assert_eq!(
        exec_lua(&rpc, "return nx.align.left('hi', 6)")
            .await
            .as_str(),
        Some("hi    ")
    );
    // right: padding on the left, text at the end.
    assert_eq!(
        exec_lua(&rpc, "return nx.align.right('hi', 6)")
            .await
            .as_str(),
        Some("    hi")
    );
    // center: padding split, odd leftover cell goes to the right (4 slack → 2/2).
    assert_eq!(
        exec_lua(&rpc, "return nx.align.center('hi', 6)")
            .await
            .as_str(),
        Some("  hi  ")
    );
    // center with an odd slack (5) sends the extra cell right: 2 left, 3 right.
    assert_eq!(
        exec_lua(&rpc, "return nx.align.center('hi', 7)")
            .await
            .as_str(),
        Some("  hi   ")
    );
}

#[tokio::test]
async fn nx_align_never_truncates_and_sizes_wide_glyphs() {
    let (rpc, _incoming) = start().await;
    // A line already at or beyond the target width is returned unchanged.
    assert_eq!(
        exec_lua(&rpc, "return nx.align.center('toolong', 3)")
            .await
            .as_str(),
        Some("toolong"),
        "align never truncates — it only ever adds spaces"
    );
    // Padding is sized by display cells: '日' is two cells, so width 6 leaves 4
    // slack, centered 2/2 — a byte/char measure would mis-pad.
    assert_eq!(
        exec_lua(&rpc, "return nx.align.center('日', 6)")
            .await
            .as_str(),
        Some("  日  "),
        "wide glyphs are padded by cell width, not byte length"
    );
}
