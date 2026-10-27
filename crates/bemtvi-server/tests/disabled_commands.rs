//! Disabled commands (`ServerInit::disabled_commands`): an app embedding the editor
//! takes commands away, and the editor refuses them with `E5800` — by any spelling, on
//! every route (typed, a mapping, `btv.cmd`, an autocmd's `command=`, `:silent`,
//! `:tab` / `:vertical`), stopping a `|` chain — while everything else still works.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, lines, message_after, poll_menu, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A server editing `start.txt` in a fresh dir (with `other.txt` beside it), running
/// `init_lua`, with `disabled` taken away.
async fn start(
    init_lua: &str,
    disabled: &[&str],
) -> (Rpc, UnboundedReceiver<Incoming>, std::path::PathBuf) {
    let dir = temp_dir("disabled_commands");
    std::fs::write(dir.join("init.lua"), init_lua).unwrap();
    std::fs::write(dir.join("start.txt"), "start\n").unwrap();
    std::fs::write(dir.join("other.txt"), "other\n").unwrap();
    let init = ServerInit {
        file: Some(dir.join("start.txt").to_string_lossy().into_owned()),
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        disabled_commands: disabled.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming, dir)
}

/// The current buffer's file name, without its directory.
async fn current_file(rpc: &Rpc) -> String {
    lines(rpc).await; // a barrier: everything sent before has run
    let name = exec_lua(rpc, "return btv.buf.name(0)").await;
    let name = name.as_str().unwrap_or_default().to_owned();
    std::path::Path::new(&name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(name)
}

async fn g(rpc: &Rpc, var: &str) -> Value {
    lines(rpc).await;
    exec_lua(rpc, &format!("return btv.g.{var}")).await
}

#[tokio::test]
async fn a_typed_command_is_refused_by_every_spelling() {
    let (rpc, mut incoming, _dir) = start("", &["edit"]).await;

    let said = message_after(&rpc, &mut incoming, ":e other.txt<CR>").await;
    assert!(said.contains("E5800: :e is disabled"), "{said:?}");
    assert_eq!(current_file(&rpc).await, "start.txt");

    let said = message_after(&rpc, &mut incoming, ":edit other.txt<CR>").await;
    assert!(said.contains("E5800: :edit is disabled"), "{said:?}");
    assert_eq!(current_file(&rpc).await, "start.txt");
}

#[tokio::test]
async fn disabling_one_spelling_disables_the_command() {
    // `e` disables `:edit`; `tabnew` disables `:tabedit`, which is the same command.
    let (rpc, _incoming, _dir) = start("", &["e", "tabnew"]).await;

    feed(&rpc, ":edit other.txt<CR>");
    assert_eq!(current_file(&rpc).await, "start.txt");
    feed(&rpc, ":tabedit other.txt<CR>");
    assert_eq!(current_file(&rpc).await, "start.txt");
}

#[tokio::test]
async fn every_route_is_refused() {
    let init = "btv.keymap.set('n', '<F5>', ':edit other.txt<CR>')\n\
                btv.on('User', { pattern = 'Go', command = 'edit other.txt' })\n";
    let (rpc, _incoming, _dir) = start(init, &["edit", "split"]).await;

    // A plugin's `btv.cmd`.
    exec_lua(&rpc, "btv.cmd('edit other.txt')").await;
    assert_eq!(current_file(&rpc).await, "start.txt", "btv.cmd");
    // A mapping.
    feed(&rpc, "<F5>");
    assert_eq!(current_file(&rpc).await, "start.txt", "a mapping");
    // An autocmd's `command=`.
    feed(&rpc, ":doautocmd User Go<CR>");
    assert_eq!(current_file(&rpc).await, "start.txt", "an autocmd");
    // Under `:silent`, `:tab` and `:vertical`.
    feed(&rpc, ":silent edit other.txt<CR>");
    assert_eq!(current_file(&rpc).await, "start.txt", ":silent");
    feed(&rpc, ":tab edit other.txt<CR>");
    assert_eq!(current_file(&rpc).await, "start.txt", ":tab");
    feed(&rpc, ":vertical split other.txt<CR>");
    lines(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return #btv.win.list()").await,
        Value::from(1),
        ":vertical split"
    );
    // From the `:` line through the RPC (`btv_command`).
    rpc.request("btv_command", vec![Value::from("edit other.txt")])
        .await
        .unwrap();
    assert_eq!(current_file(&rpc).await, "start.txt", "btv_command");
}

#[tokio::test]
async fn a_refused_command_stops_its_chain() {
    let (rpc, _incoming, _dir) = start("", &["edit"]).await;

    feed(&rpc, ":edit other.txt | lua btv.g.after = 1<CR>");
    assert_eq!(g(&rpc, "after").await, Value::Nil);
    // (A chain whose commands are all allowed runs through.)
    feed(&rpc, ":echo 'x' | lua btv.g.through = 1<CR>");
    assert_eq!(g(&rpc, "through").await, Value::from(1));
}

#[tokio::test]
async fn the_unnamed_commands_can_be_disabled() {
    let (rpc, mut incoming, _dir) = start("", &["=", "normal", "lua", "&"]).await;

    let said = message_after(&rpc, &mut incoming, ":=1 + 1<CR>").await;
    assert!(said.contains("E5800"), ":= {said:?}");
    feed(&rpc, ":normal! Ahi<CR>");
    assert_eq!(lines(&rpc).await, ["start"], ":normal");
    feed(&rpc, ":lua btv.g.ran = 1<CR>");
    assert_eq!(g(&rpc, "ran").await, Value::Nil, ":lua");
    let said = message_after(&rpc, &mut incoming, ":lua= 1<CR>").await;
    assert!(said.contains("E5800"), ":lua= {said:?}");
    feed(&rpc, ":s/start/changed/<CR>:&&<CR>");
    lines(&rpc).await;
    let said = message_after(&rpc, &mut incoming, ":&<CR>").await;
    assert!(said.contains("E5800: :& is disabled"), ":& {said:?}");
}

#[tokio::test]
async fn a_server_command_and_a_user_command_are_refused() {
    let init = "btv.command('Hello', function() btv.g.hello = 1 end)\n";
    let (rpc, mut incoming, dir) = start(init, &["Hello", "cd"]).await;

    let said = message_after(&rpc, &mut incoming, ":Hello<CR>").await;
    assert!(said.contains("E5800: :Hello is disabled"), "{said:?}");
    assert_eq!(g(&rpc, "hello").await, Value::Nil);

    let parent = dir.parent().unwrap().to_string_lossy().into_owned();
    let before = exec_lua(&rpc, "return vim.fn.getcwd()").await;
    let said = message_after(&rpc, &mut incoming, &format!(":chdir {parent}<CR>")).await;
    assert!(said.contains("E5800: :chdir is disabled"), "{said:?}");
    assert_eq!(exec_lua(&rpc, "return vim.fn.getcwd()").await, before);
}

#[tokio::test]
async fn what_is_not_disabled_still_works() {
    let init = "btv.command('Hello', function() btv.g.hello = 1 end)\n";
    let (rpc, _incoming, _dir) = start(init, &["lua"]).await;

    feed(&rpc, ":edit other.txt<CR>");
    assert_eq!(current_file(&rpc).await, "other.txt");
    feed(&rpc, ":Hello<CR>");
    assert_eq!(g(&rpc, "hello").await, Value::from(1));
}

#[tokio::test]
async fn lua_can_tell_what_is_disabled() {
    let (rpc, _incoming, _dir) = start("", &["edit"]).await;

    for (name, disabled) in [
        ("e", true),
        ("edit", true),
        (":e!", true),
        ("write", false),
        ("Nope", false),
    ] {
        assert_eq!(
            exec_lua(&rpc, &format!("return btv.command_disabled('{name}')")).await,
            Value::from(disabled),
            "{name}"
        );
    }
}

#[tokio::test]
async fn completion_leaves_disabled_commands_out() {
    let (rpc, mut incoming, _dir) = start("btv.cmdline_complete.setup {}", &["edit"]).await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming).await.expect("a menu");
    let items = bemtvi_test_harness::menu_items(&bemtvi_test_harness::menu_of(&map));
    assert!(!items.contains(&"edit".to_string()), "{items:?}");
    assert!(items.contains(&"enew".to_string()), "{items:?}");
}

/// A disabled user command whose name has a digit or an `_` in it is refused too: the
/// core reads a built-in's name up to the first non-letter, which isn't where a user
/// command's name ends.
#[tokio::test]
async fn a_user_command_with_a_digit_or_underscore_is_refused() {
    let (rpc, mut incoming, _dir) = start(
        "btv.command('Run2', function() btv.g.ran2 = true end)\n\
         btv.command('Run_it', function() btv.g.ran_it = true end)\n",
        &["Run2", "Run_it"],
    )
    .await;

    let said = message_after(&rpc, &mut incoming, ":Run2<CR>").await;
    assert!(said.contains("E5800: :Run2 is disabled"), "{said:?}");
    assert_eq!(g(&rpc, "ran2").await, Value::Nil);
    let said = message_after(&rpc, &mut incoming, ":Run_it<CR>").await;
    assert!(said.contains("E5800: :Run_it is disabled"), "{said:?}");
    assert_eq!(g(&rpc, "ran_it").await, Value::Nil);
}
