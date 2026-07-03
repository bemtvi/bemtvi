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
use nxvim_test_harness::{
    cursor_u64, drain_to_latest_redraw, exec_lua, feed, field_str, map_get, start_attached,
    wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The text of each `status` segment of `windows[0]` (the per-window status bar),
/// in order. A segment whose `style` is `Nil` is an unstyled connector.
fn status_texts(map: &[(Value, Value)]) -> Vec<String> {
    let Some(Value::Array(segs)) = window0_field(map, "status") else {
        return Vec::new();
    };
    segs.iter()
        .filter_map(|seg| match seg {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
                .map(String::from),
            _ => None,
        })
        .collect()
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
async fn a_user_command_can_be_invoked_with_a_bang() {
    let (rpc, _incoming) = start().await;

    // `:NxBang!` must dispatch to the registered user command — the trailing
    // `!` is the command's bang, not part of its name, so the user-command
    // lookup must match on the bare name (it used to fall through to
    // `E492: Not an editor command: NxBang!`).
    exec_lua(
        &rpc,
        "nx.command('NxBang', function() vim.g.nx_banged = true end, { bang = true })",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxBang!')").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.g.nx_banged").await.as_bool(),
        Some(true),
        "a user command invoked with a trailing bang runs"
    );
}

#[tokio::test]
async fn a_user_command_callback_observes_opts_bang() {
    let (rpc, _incoming) = start().await;

    // The callback's `opts.bang` must reflect the invocation: true for `:Cmd!`,
    // false for `:Cmd` — not a hard-coded false.
    exec_lua(
        &rpc,
        "_G.bangs = {}\n\
         nx.command('NxBangSee', function(o)\n\
         \x20 _G.bangs[#_G.bangs + 1] = tostring(o.bang)\n\
         end, { bang = true })",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('NxBangSee!')").await;
    exec_lua(&rpc, "vim.cmd('NxBangSee')").await;
    assert_eq!(
        exec_lua(&rpc, "return table.concat(_G.bangs, ',')")
            .await
            .as_str(),
        Some("true,false"),
        "opts.bang reflects whether the invocation carried a trailing !"
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
async fn nx_cursor_set_moves_the_cursor_and_get_and_nvim_alias_agree() {
    let (rpc, _incoming) = start().await;

    // Two lines so a (row, col) target has somewhere to land.
    feed(&rpc, "ihello world<CR>second line<Esc>");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");

    // Drive the canonical setter: 1-based row, 0-based col (nx.cursor.get's
    // convention). Row 1, col 6 → the 'w' of "world".
    exec_lua(&rpc, "nx.cursor.set({ 1, 6 })").await;

    // The nx getter reads the new position back...
    let row = exec_lua(&rpc, "return nx.cursor.get()[1]").await.as_u64();
    let col = exec_lua(&rpc, "return nx.cursor.get()[2]").await.as_u64();
    assert_eq!(
        (row, col),
        (Some(1), Some(6)),
        "nx.cursor.get reflects the nx.cursor.set move"
    );

    // ...and the actual window cursor moved, observed through the nvim alias.
    assert_eq!(
        cursor_u64(&rpc).await,
        (1, 6),
        "nx.cursor.set reached the core; nvim_win_get_cursor agrees"
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

/// The `style` palette id of the first `status` segment whose text contains
/// `needle`, or `None` when that segment carries no style (the wire `Nil` — an
/// unresolved / base-look cell).
fn status_style_of(map: &[(Value, Value)], needle: &str) -> Option<u64> {
    let Value::Array(segs) = window0_field(map, "status")? else {
        return None;
    };
    segs.iter().find_map(|seg| {
        let Value::Map(m) = seg else { return None };
        let text = m
            .iter()
            .find(|(k, _)| k.as_str() == Some("text"))
            .and_then(|(_, v)| v.as_str())?;
        if !text.contains(needle) {
            return None;
        }
        m.iter()
            .find(|(k, _)| k.as_str() == Some("style"))
            .and_then(|(_, v)| v.as_u64())
    })
}

#[tokio::test]
async fn statusline_segment_render_defining_a_highlight_resolves_same_frame() {
    let (rpc, mut incoming) = start().await;

    // A segment whose render DEFINES a highlight group and uses it in the same pass
    // (a powerline plugin's lazily-created separator/transition groups work this
    // way). The group is queued after the tick's highlight fold, so without folding
    // the render's defines before projecting, the very first frame resolves it to
    // Nil — the uncoloured-separator flicker. The first frame the cell appears must
    // already carry a resolved (non-Nil) style.
    exec_lua(
        &rpc,
        "nx.statusline.segment{ name = 'dynsep', render = function()\n\
           nx.hl.define(0, 'DynSepHl', { fg = '#ff0000', bg = '#00ff00' })\n\
           return { { text = 'SEP', hl = 'DynSepHl' } }\n\
         end }\n\
         nx.statusline.setup{ left = { 'dynsep' }, separator = '' }",
    )
    .await;
    let map = wait_redraw(&mut incoming, |m| {
        status_texts(m).iter().any(|t| t.contains("SEP"))
    })
    .await;
    assert!(
        status_style_of(&map, "SEP").is_some(),
        "the segment's own highlight must resolve on the first frame it appears, \
         not flicker uncoloured: {:?}",
        status_texts(&map)
    );
}

#[tokio::test]
async fn statusline_default_separator_inserts_unstyled_connectors() {
    let (rpc, mut incoming) = start().await;

    // A plain `mode` segment layout: the default connector puts an unstyled (Nil
    // style) leading space before the mode cell — the white gap a powerline bar
    // must avoid.
    exec_lua(&rpc, "nx.statusline.setup{ left = { 'mode' } }").await;
    let map = wait_redraw(&mut incoming, |m| {
        status_texts(m).iter().any(|t| t.contains("NORMAL"))
    })
    .await;
    let texts = status_texts(&map);
    assert_eq!(
        texts.first().map(String::as_str),
        Some(" "),
        "the default layout leads with an unstyled connector space: {texts:?}"
    );
}

#[tokio::test]
async fn statusline_empty_separator_drops_the_connector_white_gap() {
    let (rpc, mut incoming) = start().await;

    // `separator = ""` — the powerline / nxvim-line contract: no leading, trailing,
    // or inter-segment connector, so the bar is a seamless coloured run with no
    // unstyled white gaps. The mode cell is the very first segment.
    exec_lua(
        &rpc,
        "nx.statusline.setup{ left = { 'mode' }, separator = '' }",
    )
    .await;
    let map = wait_redraw(&mut incoming, |m| {
        status_texts(m).iter().any(|t| t.contains("NORMAL"))
    })
    .await;
    // The first segment is the mode cell itself (right-padded with fill to the bar
    // width is fine) — crucially it does NOT begin with a space, and no standalone
    // unstyled connector segment exists.
    let texts = status_texts(&map);
    assert!(
        texts.first().is_some_and(|t| t.starts_with("NORMAL")),
        "no leading connector before the mode cell: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.trim().is_empty()),
        "no standalone unstyled connector space remains: {texts:?}"
    );
}

#[tokio::test]
async fn nx_hl_exists_returns_boolean_vim_alias_returns_number() {
    let (rpc, _incoming) = start().await;

    // nx.hl.exists is native: a real boolean, true once the group is defined,
    // false (not nil) for an undefined group.
    exec_lua(&rpc, "nx.hl.define(0, 'NxExistsHl', { fg = '#ff0000' })").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.hl.exists('NxExistsHl')")
            .await
            .as_bool(),
        Some(true),
        "nx.hl.exists is true for a defined group"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.hl.exists('NxMissingHl')")
            .await
            .as_bool(),
        Some(false),
        "nx.hl.exists is false (boolean, not nil) for an undefined group"
    );

    // vim.fn.hlexists keeps the vimscript 1/0 contract for plugin compat.
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.hlexists('NxExistsHl')")
            .await
            .as_i64(),
        Some(1),
        "vim.fn.hlexists answers 1 for a defined group"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.hlexists('NxMissingHl')")
            .await
            .as_i64(),
        Some(0),
        "vim.fn.hlexists answers 0 for an undefined group"
    );
}

#[tokio::test]
async fn vim_fn_exists_detects_a_user_command() {
    let (rpc, _incoming) = start().await;

    // An undefined command is 0; vimscript `exists(':Cmd')` reports 2 for a defined
    // command (its "exact match" value), so a plugin's `exists(':Foo') == 2` probe works.
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.exists(':NxExistsCmd')")
            .await
            .as_i64(),
        Some(0),
        "an undefined :Cmd is 0"
    );
    exec_lua(
        &rpc,
        "vim.api.nvim_create_user_command('NxExistsCmd', function() end, {})",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.exists(':NxExistsCmd')")
            .await
            .as_i64(),
        Some(2),
        "a defined user command exists (2, neovim's exact-match value)"
    );

    // A buffer-local command counts for the current buffer, like at dispatch.
    exec_lua(
        &rpc,
        "vim.api.nvim_buf_create_user_command(0, 'NxBufExistsCmd', function() end, {})",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.exists(':NxBufExistsCmd')")
            .await
            .as_i64(),
        Some(2),
        "a buffer-local command exists for the current buffer"
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

#[tokio::test]
async fn nx_hash_returns_known_digests() {
    let (rpc, _incoming) = start().await;

    // Known-answer vectors (NIST / RFC 1321) pin the digests so the wiring is
    // proven against an external oracle, not just self-consistent.
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.sha1('abc')").await.as_str(),
        Some("a9993e364706816aba3e25717850c26c9cd0d89d"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.sha256('abc')")
            .await
            .as_str(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.sha512('abc')")
            .await
            .as_str(),
        Some(
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        ),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.md5('abc')").await.as_str(),
        Some("900150983cd24fb0d6963f7d28e17f72"),
    );

    // The empty string hashes to each algorithm's canonical empty digest.
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.sha1('')").await.as_str(),
        Some("da39a3ee5e6b4b0d3255bfef95601890afd80709"),
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.md5('')").await.as_str(),
        Some("d41d8cd98f00b204e9800998ecf8427e"),
    );
}

#[tokio::test]
async fn nx_hash_handles_binary_input() {
    let (rpc, _incoming) = start().await;

    // A NUL-embedded byte string must hash by its raw bytes, not a truncated /
    // UTF-8-validated view. sha256 of the three bytes {0x00, 0xff, 0x41}.
    assert_eq!(
        exec_lua(&rpc, "return nx.hash.sha256('\\0\\255A')")
            .await
            .as_str(),
        Some("a90a10503fbfc95789ff38a1bb5039cb71869ab9c0eb1cb51c4a9099f2933c6b"),
    );
}

#[tokio::test]
async fn nx_hash_new_incremental_matches_one_shot() {
    let (rpc, _incoming) = start().await;

    // Feeding bytes in pieces must equal hashing the whole string at once — the core
    // promise of an incremental hasher (so you can hash a stream as it arrives).
    assert_eq!(
        exec_lua(
            &rpc,
            "local h = nx.hash.new('sha256')\n\
             h:update('ab'); h:update(''); h:update('c')\n\
             return h:hexdigest()"
        )
        .await
        .as_str(),
        exec_lua(&rpc, "return nx.hash.sha256('abc')")
            .await
            .as_str(),
        "chunked updates equal the one-shot digest of the same bytes"
    );

    // hexdigest() does NOT consume the hasher: read an intermediate digest, then keep
    // feeding. The intermediate equals sha256('ab'); the final equals sha256('abc').
    assert_eq!(
        exec_lua(
            &rpc,
            "local h = nx.hash.new('sha256')\n\
             h:update('ab')\n\
             local mid = h:hexdigest()\n\
             h:update('c')\n\
             return mid .. ',' .. h:hexdigest()"
        )
        .await
        .as_str(),
        Some(
            "fb8e20fc2e4c3f248c60c39bd652f3c1347298bb977b8b4d5903b85055620603,\
             ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ),
        "hexdigest is non-consuming — an intermediate read does not disturb the running state"
    );

    // An unknown algorithm fails loud at construction, not with a bad digest later.
    assert_eq!(
        exec_lua(
            &rpc,
            "local ok, err = pcall(nx.hash.new, 'crc32'); return (not ok) and tostring(err) or 'NO_ERROR'"
        )
        .await
        .as_str()
        .map(|s| s.contains("unknown algorithm")),
        Some(true),
        "nx.hash.new rejects an unknown algorithm"
    );
}

#[tokio::test]
async fn nx_uuid_is_a_unique_v4_uuid() {
    let (rpc, _incoming) = start().await;

    // The public nx.uuid() (a Lua wrapper over the nx._uuid bridge) returns a canonical
    // 8-4-4-4-12 v4 UUID: 36 chars, hyphens at the fixed spots, version nibble '4', and
    // a variant nibble in [89ab]. Pattern-check rather than equality (it's random).
    assert_eq!(
        exec_lua(
            &rpc,
            "local u = nx.uuid()\n\
             return tostring(#u == 36\n\
               and u:sub(9,9) == '-' and u:sub(14,14) == '-'\n\
               and u:sub(19,19) == '-' and u:sub(24,24) == '-'\n\
               and u:sub(15,15) == '4'\n\
               and u:match('^[0-9a-f-]+$') ~= nil\n\
               and u:sub(20,20):match('[89ab]') ~= nil)"
        )
        .await
        .as_str(),
        Some("true"),
        "nx.uuid() returns a canonical lowercase v4 UUID"
    );

    // Two calls differ — it's actually random, not a fixed stub.
    assert_eq!(
        exec_lua(&rpc, "return tostring(nx.uuid() ~= nx.uuid())")
            .await
            .as_str(),
        Some("true"),
        "successive nx.uuid() calls are unique"
    );
}

#[tokio::test]
async fn nx_rust_backed_utility_wrappers_resolve() {
    let (rpc, _incoming) = start().await;

    // The documented Lua wrappers (prelude/nx.lua) over the Rust nx._* bridges must all
    // exist and have the right shape — proving the prelude loaded and each public name
    // still points at a live native. Callable wrappers are checked as functions; the
    // read-only ones are invoked and their value/type asserted.
    assert_eq!(
        exec_lua(
            &rpc,
            "local fn = {}\n\
             for _, n in ipairs({ 'echo', 'argv', 'reexec', 'now_ms', 'runtime_file', 'open' }) do\n\
               fn[#fn+1] = n .. '=' .. type(nx[n])\n\
             end\n\
             fn[#fn+1] = 'layer.focus=' .. type(nx.layer.focus)\n\
             fn[#fn+1] = 'layer.main=' .. type(nx.layer.main)\n\
             fn[#fn+1] = 'terminal.open=' .. type(nx.terminal.open)\n\
             return table.concat(fn, ',')"
        )
        .await
        .as_str(),
        Some(
            "echo=function,argv=function,reexec=function,now_ms=function,\
             runtime_file=function,open=function,layer.focus=function,\
             layer.main=function,terminal.open=function"
        ),
    );

    // The read-only utilities return real values through the wrapper.
    assert_eq!(
        exec_lua(
            &rpc,
            "return type(nx.argv()) .. ',' .. type(nx.now_ms()) .. ',' ..\n\
                    type(nx.runtime_file('lsp/nonesuch.lua', false)) .. ',' ..\n\
                    type(nx.workspace.dir == nil and 'function' or nx.workspace.dir()) .. ',' ..\n\
                    tostring(nx.workspace.active())"
        )
        .await
        .as_str(),
        // no --workspace launch: dir() is nil (type 'nil'), active() is false.
        Some("table,number,table,nil,false"),
    );

    // nx.echo reaches the message line, and its nvim_echo alias is the same native.
    exec_lua(&rpc, "nx.echo('swept-hello')").await;
    assert_eq!(
        exec_lua(&rpc, "return type(vim.api.nvim_echo)")
            .await
            .as_str(),
        Some("function"),
        "nvim_echo alias survives the wrapper split"
    );
}

// ----- nx.notify / nx.inspect / stdlib regressions ---------------------------

/// `nx.notify(msg, "error")` — the string severity spelling several surfaces use
/// (decor providers, picker/complete source errors) — must paint the red error
/// line exactly like the numeric `vim.log.levels.ERROR`, not degrade to a plain
/// print.
#[tokio::test]
async fn nx_notify_string_error_level_paints_the_error_line() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.notify('boom happened', 'error')").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(field_str(&frame, "message"), "boom happened");
    assert_eq!(
        map_get(&frame, "message_error").and_then(Value::as_bool),
        Some(true),
        "a string 'error' severity must reach the error line, not a plain print"
    );
}

/// `nx.inspect` on a self-referencing table renders `<cycle>` instead of
/// overflowing the stack — plugins inspect arbitrary state (parent-linked trees).
#[tokio::test]
async fn nx_inspect_handles_cycles() {
    let (rpc, _incoming) = start().await;
    let out = exec_lua(
        &rpc,
        "local t = { name = 'root' }\n\
         t.me = t\n\
         local ok, s = pcall(nx.inspect, t)\n\
         return tostring(ok) .. ':' .. (ok and (s:find('<cycle>', 1, true) and 'marked' or s) or tostring(s))",
    )
    .await;
    assert_eq!(out.as_str(), Some("true:marked"));
}

/// A separator pattern that matches an empty string would leave the split scan
/// in place forever; vim.split must fail loud (neovim's "Infinite loop detected"
/// contract) instead of hanging the editor.
#[tokio::test]
async fn vim_split_zero_width_separator_pattern_fails_loud_not_hang() {
    let (rpc, _incoming) = start().await;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        exec_lua(
            &rpc,
            "local ok, err = pcall(vim.split, 'abc', 'x*')\n\
             return tostring(ok) .. ':' .. tostring(err)",
        ),
    )
    .await
    .expect("vim.split with a zero-width separator must error, not spin forever");
    let s = out.as_str().unwrap_or_default().to_string();
    assert!(
        s.starts_with("false:") && s.contains("empty string"),
        "expected a loud named error, got {s:?}"
    );
}

/// The ordinary split shapes still hold after the zero-width guard (incl.
/// trimempty's leading/trailing removal, which now shifts once instead of
/// re-shifting per removal).
#[tokio::test]
async fn vim_split_trimempty_drops_leading_and_trailing_empties() {
    let (rpc, _incoming) = start().await;
    let out = exec_lua(
        &rpc,
        "return table.concat(vim.split(',,a,b,,', ',', { trimempty = true }), '|')",
    )
    .await;
    assert_eq!(out.as_str(), Some("a|b"));
    let (rpc2, _inc2) = start().await;
    let all_empty = exec_lua(
        &rpc2,
        "return tostring(#vim.split(',,,', ',', { trimempty = true }))",
    )
    .await;
    assert_eq!(all_empty.as_str(), Some("0"));
}

/// `vim.fn.bufnr(name)` prefers the exactly-named buffer over a suffix match —
/// and never lets `pairs` iteration order pick between them.
#[tokio::test]
async fn bufnr_prefers_an_exact_name_over_a_suffix_match() {
    let (rpc, _incoming) = start().await;
    let dir = nxvim_test_harness::temp_dir("bufnr_exact");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("init.lua"), "top\n").unwrap();
    std::fs::write(dir.join("sub").join("init.lua"), "nested\n").unwrap();

    // Open the suffix-matching buffer FIRST so it holds the lower bufnr (the one a
    // first-match-wins scan would return), then the exactly-named one.
    exec_lua(&rpc, &format!("vim.cmd('cd {}')", dir.display())).await;
    exec_lua(&rpc, "vim.cmd('edit sub/init.lua')").await;
    exec_lua(&rpc, "vim.cmd('edit init.lua')").await;

    let name = exec_lua(
        &rpc,
        "local nr = vim.fn.bufnr('init.lua')\n\
         return (nx._bufs[nr] or {}).name or ('no buffer ' .. tostring(nr))",
    )
    .await;
    assert_eq!(
        name.as_str(),
        Some("init.lua"),
        "bufnr('init.lua') must resolve to the exactly-named buffer, not the sub/init.lua suffix match"
    );
}
