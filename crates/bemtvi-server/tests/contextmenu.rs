//! `btv.contextmenu`: the right-click menu built from groups, in every session
//! (`docs/plans/2026-10-25-context-menu-groups.md`). Each buffer has an ordered list of
//! group names; each group gives its rows; the groups with none are dropped and a
//! separator goes between the rest.
//!
//! Most cases read the rows through `btv.contextmenu.entries(buf)`, the same rows a
//! right-click shows; the gesture cases drive the real right-click.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach_with_caps, command, exec_lua, feed, feed_mouse, lines, map_get, menu_items, menu_of,
    mode, poll_menu, poll_true, spawn, write_temp, FakeClipboard,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A GUI session editing `content`, with a fake clipboard and no line numbers (so a
/// screen column is a buffer column).
async fn start_gui(content: &str) -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>) {
    let path = write_temp("contextmenu", "txt", content);
    let clip = FakeClipboard::default();
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(path),
        clipboard: bemtvi_server::ClipboardProvider::Custom(Box::new(clip.clone())),
        ..Default::default()
    });
    let caps = vec![
        (Value::from("client"), Value::from("gui")),
        (Value::from("keyboard_protocol"), Value::Boolean(true)),
    ];
    attach_with_caps(&rpc, 80, 24, caps).await;
    command(&rpc, "set nonumber norelativenumber").await;
    (rpc, clip, incoming)
}

/// The same, with `'modeless'` on.
async fn start_modeless(content: &str) -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>) {
    let (rpc, clip, inc) = start_gui(content).await;
    command(&rpc, "set modeless").await;
    (rpc, clip, inc)
}

fn v(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| s.to_string()).collect()
}

/// The rows `btv.contextmenu.entries(buf)` gives, one string each: `"-"` for a
/// separator, `"Label"`, `"Label [detail]"` with a detail, and a trailing `" (off)"`
/// for a greyed-out row. A rejection comes back as `Err` with its message.
async fn rows(rpc: &Rpc, buf: &str) -> Result<Vec<String>, String> {
    exec_lua(
        rpc,
        &format!(
            "_G.rows, _G.err = nil, nil
             btv.contextmenu.entries({buf}):next(function(es)
               local out = {{}}
               for _, e in ipairs(es) do
                 local s = e.separator and '-' or e.label
                 if e.detail then s = s .. ' [' .. e.detail .. ']' end
                 if e.enabled == false and not e.separator then s = s .. ' (off)' end
                 out[#out + 1] = s
               end
               _G.rows = out
             end, function(e) _G.err = tostring(e) end)"
        ),
    )
    .await;
    assert!(
        poll_true(rpc, "return _G.rows ~= nil or _G.err ~= nil").await,
        "entries() never settled"
    );
    let err = exec_lua(rpc, "return _G.err").await;
    if let Some(e) = err.as_str() {
        return Err(e.to_string());
    }
    let got = exec_lua(rpc, "return _G.rows").await;
    Ok(got
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default())
}

/// Register two small groups, `a` and `b`, of one row each.
async fn two_groups(rpc: &Rpc) {
    exec_lua(
        rpc,
        "btv.contextmenu.groups.a = { items = { { label = 'A', action = function() end } } }
         btv.contextmenu.groups.b = { items = { { label = 'B', action = function() end } } }",
    )
    .await;
}

// ----- groups and lists --------------------------------------------------------

#[tokio::test]
async fn groups_show_in_list_order_with_a_separator_between_them() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    two_groups(&rpc).await;
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'b', 'a' })").await;
    assert_eq!(rows(&rpc, "0").await, Ok(v(&["B", "-", "A"])));
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'a', 'b' })").await;
    assert_eq!(rows(&rpc, "0").await, Ok(v(&["A", "-", "B"])));
}

#[tokio::test]
async fn a_group_with_no_rows_leaves_no_separator_behind() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    two_groups(&rpc).await;
    exec_lua(
        &rpc,
        "btv.contextmenu.groups.none = { items = {} }
         btv.contextmenu.groups.hidden_rows = { items = {
           { label = 'X', action = function() end, when = false },
         } }
         btv.contextmenu.set(0, { 'none', 'a', 'hidden_rows', 'none', 'b', 'none' })",
    )
    .await;
    assert_eq!(rows(&rpc, "0").await, Ok(v(&["A", "-", "B"])));
}

#[tokio::test]
async fn editing_one_buffers_list_leaves_the_others_alone() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    two_groups(&rpc).await;
    // `btv.cmd` runs when the chunk ends, so the new buffer is current only in the
    // next one.
    exec_lua(&rpc, "_G.first = btv.buf.current() btv.cmd('enew')").await;
    exec_lua(
        &rpc,
        "_G.second = btv.buf.current()
         btv.contextmenu.set(_G.first, { 'a' })
         btv.contextmenu.set(_G.second, { 'a' })
         table.insert(btv.contextmenu.list(_G.second), 'b')",
    )
    .await;
    assert_eq!(rows(&rpc, "_G.first").await, Ok(v(&["A"])));
    assert_eq!(rows(&rpc, "_G.second").await, Ok(v(&["A", "-", "B"])));
}

/// A deleted buffer's list goes with it: nothing keyed by its number outlives it.
#[tokio::test]
async fn a_deleted_buffers_list_is_dropped() {
    let (rpc, _clip, _inc) = start_gui(
        "hello
",
    )
    .await;
    exec_lua(&rpc, "btv.cmd('enew')").await;
    exec_lua(
        &rpc,
        "_G.gone = btv.buf.current()
         btv.contextmenu.set(_G.gone, { 'commands' })
         btv.cmd('bdelete! ' .. _G.gone)",
    )
    .await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.contextmenu.list, _G.gone) return tostring(e)",
    )
    .await;
    assert!(err.as_str().unwrap_or("").contains("no buffer"), "{err:?}");
    let left = exec_lua(&rpc, "return btv.contextmenu._lists_count()").await;
    assert_eq!(left.as_u64(), Some(0), "only the deleted buffer had a list");
}

#[tokio::test]
async fn a_buffer_starts_from_the_default_for_its_kind() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "_G.editable = table.concat(btv.contextmenu.list(0), ',')
         btv.cmd('enew')
         btv.cmd('setlocal nomodifiable')",
    )
    .await;
    let lists = exec_lua(
        &rpc,
        "return { _G.editable, table.concat(btv.contextmenu.list(0), ',') }",
    )
    .await;
    let got: Vec<&str> = lists
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(got, ["undo,clipboard,lsp,commands", "clipboard"]);
}

/// A read-only KIND (here a plugin view) counts as read-only even though its
/// `'modifiable'` option was never touched: it reads `nomodifiable` through `btv.bo`.
#[tokio::test]
async fn a_plugin_view_starts_from_the_read_only_default() {
    let (rpc, _clip, _inc) = start_gui(
        "hello
",
    )
    .await;
    exec_lua(
        &rpc,
        "_G.vw = btv.view.create{}
         _G.vw:set_lines{ 'alpha' }
         _G.vw:mount{ dock = 'left', size = 20 }",
    )
    .await;
    assert!(poll_true(&rpc, "return _G.vw:bufnr() ~= nil").await);
    assert_eq!(rows(&rpc, "_G.vw:bufnr()").await, Ok(v(&["Select All"])));
}

#[tokio::test]
async fn editing_a_default_changes_later_buffers_only() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "_G.before = table.concat(btv.contextmenu.list(0), ',')
         table.insert(btv.contextmenu.default, 1, 'mine')
         _G.still = table.concat(btv.contextmenu.list(0), ',')
         btv.cmd('enew')",
    )
    .await;
    let lists = exec_lua(
        &rpc,
        "return { _G.before, _G.still, table.concat(btv.contextmenu.list(0), ',') }",
    )
    .await;
    let got: Vec<&str> = lists
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(
        got,
        [
            "undo,clipboard,lsp,commands",
            "undo,clipboard,lsp,commands",
            "mine,undo,clipboard,lsp,commands"
        ]
    );
}

#[tokio::test]
async fn a_default_groups_items_can_be_edited() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "table.insert(btv.contextmenu.groups.commands.items, {
           label = 'Save', detail = 'mine', action = function() end,
         })
         table.remove(btv.contextmenu.groups.clipboard.items, 1) -- Cut",
    )
    .await;
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'clipboard', 'commands' })").await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&[
            "Copy",
            "Paste",
            "Select All",
            "-",
            "Command Palette",
            "Save [mine]"
        ]))
    );
}

#[tokio::test]
async fn a_group_can_be_hidden() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(&rpc, "btv.contextmenu.groups.undo.when = false").await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&["Copy", "Paste", "Select All", "-", "Command Palette"]))
    );
    exec_lua(
        &rpc,
        "btv.contextmenu.groups.undo.when = function(ctx) return ctx.modifiable end",
    )
    .await;
    assert_eq!(rows(&rpc, "0").await.unwrap()[0], "Undo");
}

/// A group that answers with a promise, and answers LATER than the group after it:
/// the menu waits for both and keeps list order.
#[tokio::test]
async fn an_async_group_is_waited_for_and_keeps_its_place() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "btv.contextmenu.groups.slow = { items = function(ctx)
           return btv.promise.delay(60):next(function()
             return { { label = 'Slow ' .. ctx.buf, action = function() end } }
           end)
         end }
         btv.contextmenu.groups.fast = { items = function()
           return btv.promise.delay(5):next(function()
             return { { label = 'Fast', action = function() end } }
           end)
         end }
         btv.contextmenu.set(0, { 'slow', 'fast' })",
    )
    .await;
    let buf = exec_lua(&rpc, "return btv.buf.current()").await;
    let slow = format!("Slow {}", buf.as_u64().unwrap());
    assert_eq!(rows(&rpc, "0").await, Ok(v(&[&slow, "-", "Fast"])));
}

#[tokio::test]
async fn an_unknown_group_fails_loud_with_its_name() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'clipboard', 'nosuch' })").await;
    let err = rows(&rpc, "0").await.unwrap_err();
    assert!(err.contains("no group named nosuch"), "{err}");
}

#[tokio::test]
async fn an_unknown_field_fails_loud_with_the_group() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "btv.contextmenu.groups.typo = { items = { { lable = 'X', action = function() end } } }
         btv.contextmenu.groups.typo2 = { itmes = {} }",
    )
    .await;
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'typo' })").await;
    let err = rows(&rpc, "0").await.unwrap_err();
    assert!(
        err.contains("btv.contextmenu.groups.typo.items[1]") && err.contains("lable"),
        "{err}"
    );
    exec_lua(&rpc, "btv.contextmenu.set(0, { 'typo2' })").await;
    let err = rows(&rpc, "0").await.unwrap_err();
    assert!(
        err.contains("btv.contextmenu.groups.typo2") && err.contains("itmes"),
        "{err}"
    );
}

#[tokio::test]
async fn a_bad_list_fails_loud() {
    let (rpc, _clip, _inc) = start_gui("hello\n").await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.contextmenu.set, 0, { 'a', 3 }) return tostring(e)",
    )
    .await;
    assert!(err.as_str().unwrap_or("").contains("group name"), "{err:?}");
}

/// `ctx` reaches every hook: here a row's `detail` reports what it was given.
#[tokio::test]
async fn hooks_get_the_click_context() {
    let (rpc, _clip, _inc) = start_gui("hello world\n").await;
    exec_lua(
        &rpc,
        "btv.contextmenu.groups.ctx = { items = { { label = 'ctx', action = function() end,
           detail = function(c)
             return table.concat({ c.mode, tostring(c.selection), tostring(c.modifiable),
               tostring(c.modeless), tostring(c.buf == btv.buf.current()),
               tostring(c.win == btv.win.current()), tostring(c.mouse ~= nil) }, ',')
           end } } }
         btv.contextmenu.set(0, { 'ctx' })",
    )
    .await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&["ctx [n,false,true,false,true,true,true]"]))
    );
    feed(&rpc, "vl");
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&["ctx [v,true,true,false,true,true,true]"]))
    );
}

// ----- the default groups -------------------------------------------------------

/// A vim session: no key hints (vim's keys are verbs plus motions), and Cut only over
/// a selection.
#[tokio::test]
async fn a_vim_session_gets_the_menu_without_hints() {
    let (rpc, _clip, _inc) = start_gui("hello world\n").await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&[
            "Undo",
            "Redo",
            "-",
            "Copy",
            "Paste",
            "Select All",
            "-",
            "Command Palette"
        ]))
    );
    feed(&rpc, "vl");
    assert_eq!(
        rows(&rpc, "0").await.unwrap()[3..5],
        ["Cut".to_string(), "Copy".to_string()]
    );
}

#[tokio::test]
async fn a_modeless_session_shows_the_layout_keys() {
    let (rpc, _clip, _inc) = start_modeless("hello world\n").await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&[
            "Undo [Ctrl+Z]",
            "Redo [Ctrl+Shift+Z]",
            "-",
            "Cut [Ctrl+X]",
            "Copy [Ctrl+C]",
            "Paste [Ctrl+V]",
            "Select All [Ctrl+A]",
            "-",
            "Command Palette [Ctrl+Shift+P]"
        ]))
    );
}

#[tokio::test]
async fn a_read_only_buffer_offers_select_all_and_copy_over_a_selection() {
    let (rpc, _clip, _inc) = start_gui("hello world\n").await;
    command(&rpc, "setlocal nomodifiable").await;
    assert_eq!(rows(&rpc, "0").await, Ok(v(&["Select All"])));
    feed(&rpc, "vl");
    assert_eq!(rows(&rpc, "0").await, Ok(v(&["Copy", "Select All"])));
}

#[tokio::test]
async fn lsp_rows_show_with_a_server_and_grey_out_what_it_cant_do() {
    let (rpc, _clip, _inc) = start_gui("hello world\n").await;
    exec_lua(
        &rpc,
        "btv.lsp.clients = function()
           return { { supports_method = function(_, m) return m ~= 'textDocument/rename' end } }
         end
         btv.contextmenu.set(0, { 'lsp' })",
    )
    .await;
    assert_eq!(
        rows(&rpc, "0").await,
        Ok(v(&[
            "Go to Definition",
            "Find References",
            "Rename Symbol (off)",
            "Format Document",
            "Code Actions"
        ]))
    );
    exec_lua(&rpc, "btv.lsp.clients = function() return {} end").await;
    assert_eq!(rows(&rpc, "0").await, Ok(v(&[])));
}

// ----- the right-click, end to end -----------------------------------------------

/// Right-click at (row, col), wait for the menu, and click its row labelled `label`.
/// The box's border sits on the clicked cell, so row `i` (1-based) of the list is
/// painted at `row + i`.
async fn choose(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, at: (usize, usize), label: &str) {
    feed_mouse(rpc, "right", "press", at.0, at.1);
    let menu = menu_of(&poll_menu(rpc, inc).await.expect("a menu opens"));
    let labels = menu_items(&menu);
    let i = labels
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("no row {label} in {labels:?}"));
    feed_mouse(rpc, "left", "press", at.0 + 1 + i, at.1 + 2);
    feed_mouse(rpc, "left", "release", at.0 + 1 + i, at.1 + 2);
}

async fn clip_eventually(clip: &FakeClipboard) -> Option<(String, bool)> {
    for _ in 0..100 {
        if let Some(got) = clip.peek() {
            return Some(got);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

async fn lines_eventually(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    let mut got = lines(rpc).await;
    for _ in 0..100 {
        if got == v(want) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        got = lines(rpc).await;
    }
    got
}

#[tokio::test]
async fn a_right_click_in_a_vim_session_opens_the_menu_at_the_pointer() {
    let (rpc, _clip, mut inc) = start_gui("hello world\n").await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    let at = |k: &str| map_get(&menu, k).and_then(Value::as_u64);
    assert_eq!((at("row"), at("col")), (Some(0), Some(4)));
    assert_eq!(menu_items(&menu)[0], "Undo");
}

#[tokio::test]
async fn vim_copy_over_a_selection_yanks_it_to_the_clipboard() {
    let (rpc, clip, mut inc) = start_gui("hello world\n").await;
    feed(&rpc, "vll");
    choose(&rpc, &mut inc, (0, 1), "Copy").await;
    assert_eq!(
        clip_eventually(&clip).await,
        Some(("hel".to_string(), false))
    );
    assert_eq!(lines(&rpc).await, v(&["hello world"]));
}

#[tokio::test]
async fn vim_copy_with_nothing_selected_yanks_the_line() {
    let (rpc, clip, mut inc) = start_gui("hello world\nnext\n").await;
    choose(&rpc, &mut inc, (0, 4), "Copy").await;
    assert_eq!(
        clip_eventually(&clip).await,
        Some(("hello world\n".to_string(), true))
    );
}

#[tokio::test]
async fn vim_cut_deletes_the_selection_into_the_clipboard() {
    let (rpc, clip, mut inc) = start_gui("hello world\n").await;
    feed(&rpc, "ve");
    choose(&rpc, &mut inc, (0, 2), "Cut").await;
    assert_eq!(lines_eventually(&rpc, &[" world"]).await, v(&[" world"]));
    assert_eq!(clip.peek(), Some(("hello".to_string(), false)));
}

#[tokio::test]
async fn vim_paste_puts_the_clipboard_at_the_click() {
    let (rpc, clip, mut inc) = start_gui("hello world\n").await;
    clip.seed("XY", false);
    choose(&rpc, &mut inc, (0, 6), "Paste").await;
    assert_eq!(
        lines_eventually(&rpc, &["hello XYworld"]).await,
        v(&["hello XYworld"])
    );
}

#[tokio::test]
async fn vim_paste_from_insert_mode_stays_in_insert() {
    let (rpc, clip, mut inc) = start_gui("hello world\n").await;
    clip.seed("XY", false);
    feed(&rpc, "i");
    choose(&rpc, &mut inc, (0, 6), "Paste").await;
    assert_eq!(
        lines_eventually(&rpc, &["hello XYworld"]).await,
        v(&["hello XYworld"])
    );
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn vim_select_all_selects_every_line() {
    let (rpc, _clip, mut inc) = start_gui("one\ntwo\nthree\n").await;
    choose(&rpc, &mut inc, (1, 1), "Select All").await;
    assert!(poll_true(&rpc, "return btv.mode().mode == 'V'").await);
    let sel = exec_lua(&rpc, "return { vim.fn.line('v'), vim.fn.line('.') }").await;
    let got: Vec<u64> = sel
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap())
        .collect();
    assert_eq!(got, [1, 3]);
}

#[tokio::test]
async fn vim_undo_undoes_the_last_change() {
    let (rpc, _clip, mut inc) = start_gui("hello\n").await;
    feed(&rpc, "Aworld<Esc>");
    assert_eq!(lines(&rpc).await, v(&["helloworld"]));
    choose(&rpc, &mut inc, (0, 2), "Undo").await;
    assert_eq!(lines_eventually(&rpc, &["hello"]).await, v(&["hello"]));
}

#[tokio::test]
async fn a_rows_action_gets_the_context_and_may_return_a_promise() {
    let (rpc, _clip, mut inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "_G.done = nil
         btv.contextmenu.groups.later = { items = { { label = 'Later', action = function(ctx)
           return btv.promise.delay(20):next(function() _G.ran = ctx.mouse.column end)
         end } } }
         btv.contextmenu.set(0, { 'later' })
         -- open() itself, from a key: it settles once the action has.
         btv.keymap.set('n', '<F5>', function()
           btv.contextmenu.open():next(function() _G.done = _G.ran end)
         end)",
    )
    .await;
    feed_mouse(&rpc, "left", "press", 0, 3);
    feed_mouse(&rpc, "left", "release", 0, 3);
    feed(&rpc, "<F5>");
    poll_menu(&rpc, &mut inc).await.expect("a menu opens");
    // A menu at the pointer opens with no row lit: `<Down>` lights the first.
    feed(&rpc, "<Down><CR>");
    let ok = poll_true(&rpc, "return _G.done == 4").await;
    assert!(
        ok,
        "open() settled after the action, which saw the click: ran={:?} done={:?}",
        exec_lua(&rpc, "return _G.ran").await,
        exec_lua(&rpc, "return _G.done").await
    );
}

#[tokio::test]
async fn an_empty_list_opens_nothing() {
    let (rpc, _clip, mut inc) = start_gui("hello\n").await;
    exec_lua(&rpc, "btv.contextmenu.set(0, {})").await;
    feed_mouse(&rpc, "right", "press", 0, 2);
    assert!(poll_menu(&rpc, &mut inc).await.is_none(), "no menu opens");
}

#[tokio::test]
async fn a_buffer_local_right_click_map_replaces_the_menu() {
    let (rpc, _clip, mut inc) = start_gui("hello\n").await;
    exec_lua(
        &rpc,
        "_G.mapped = 0
         btv.keymap.set('n', '<RightMouse>', function() _G.mapped = _G.mapped + 1 end, { buffer = 0 })",
    )
    .await;
    feed_mouse(&rpc, "right", "press", 0, 2);
    assert!(poll_true(&rpc, "return _G.mapped == 1").await);
    assert!(poll_menu(&rpc, &mut inc).await.is_none(), "no menu opens");
}

#[tokio::test]
async fn a_menu_opened_from_a_key_after_a_click_opens_at_the_cursor() {
    let (rpc, _clip, mut inc) = start_gui("hello world\nsecond line\n").await;
    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<F5>', function() btv.contextmenu.open() end)",
    )
    .await;
    // An earlier click leaves `getmousepos()` on (1, 8); the key comes after it, so the
    // menu belongs at the text cursor, not at that old click.
    feed_mouse(&rpc, "left", "press", 1, 8);
    feed_mouse(&rpc, "left", "release", 1, 8);
    feed(&rpc, "gg0<F5>");
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert_eq!(
        map_get(&menu, "editor_relative"),
        None,
        "a key-opened menu sits under the text cursor: {menu:?}"
    );
}

#[tokio::test]
async fn a_right_click_off_an_open_picker_opens_the_menu() {
    let (rpc, _clip, mut inc) = start_gui("hello world\n").await;
    exec_lua(&rpc, "btv.picker.open('commands')").await;
    poll_menu(&rpc, &mut inc).await.expect("the picker opens");
    // The picker is centered: the top-left cell is off it.
    feed_mouse(&rpc, "right", "press", 0, 0);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert_eq!(menu_items(&menu)[0], "Undo");
    // And it stays open: nothing settled it as cancelled behind the picker's back.
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("still open"));
    assert_eq!(menu_items(&menu)[0], "Undo");
}

#[tokio::test]
async fn a_pointer_menu_says_so_and_a_picker_does_not() {
    let (rpc, _clip, mut inc) = start_gui("hello world\n").await;
    feed_mouse(&rpc, "right", "press", 0, 4);
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("a menu opens"));
    assert_eq!(
        map_get(&menu, "pointer").and_then(Value::as_bool),
        Some(true)
    );
    feed(&rpc, "<Esc>");
    exec_lua(&rpc, "btv.picker.open('commands')").await;
    let menu = menu_of(&poll_menu(&rpc, &mut inc).await.expect("the picker opens"));
    assert_eq!(map_get(&menu, "pointer"), None);
}

#[tokio::test]
async fn a_key_hint_reads_every_modifier_spelling() {
    let (rpc, _clip, _inc) = start_modeless("hello\n").await;
    let got = exec_lua(
        &rpc,
        "local f = function() end
         btv.keymap.set('modeless', '<M-q>', f)
         local g = function() end
         btv.keymap.set('modeless', '<c-s-k>', g)
         return { btv.modeless.key_hint(f) or 'nil', btv.modeless.key_hint(g) or 'nil' }",
    )
    .await;
    let got: Vec<&str> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(got, ["Alt+Q", "Ctrl+Shift+K"]);
}

#[tokio::test]
async fn a_right_click_off_a_select_mode_selection_ends_it() {
    let (rpc, _clip, mut inc) = start_gui("foo bar baz qux\n").await;
    // Visual over `foo`, switched to Select: no modeless selection behind it.
    feed(&rpc, "vll<C-g>");
    assert_eq!(mode(&rpc).await, "s");
    feed_mouse(&rpc, "right", "press", 0, 12);
    poll_menu(&rpc, &mut inc).await.expect("a menu opens");
    assert_eq!(
        exec_lua(&rpc, "return btv.mode().mode").await.as_str(),
        Some("n"),
        "the selection ended rather than stretching to the click"
    );
    let cur = exec_lua(&rpc, "return vim.fn.col('.')").await;
    assert_eq!(cur.as_u64(), Some(13));
}

#[tokio::test]
async fn a_right_click_inside_a_select_mode_selection_keeps_it() {
    let (rpc, _clip, mut inc) = start_gui("foo bar baz qux\n").await;
    feed(&rpc, "vll<C-g>");
    feed_mouse(&rpc, "right", "press", 0, 1);
    poll_menu(&rpc, &mut inc).await.expect("a menu opens");
    let got = exec_lua(
        &rpc,
        "return { btv.mode().mode, vim.fn.getpos('v')[3], vim.fn.col('.') }",
    )
    .await;
    let got: Vec<String> = got
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.to_string())
        .collect();
    assert_eq!(got, ["\"s\"", "1", "3"]);
}
