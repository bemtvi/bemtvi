//! The single-file lock (`ServerInit::single_file`): the editor edits its startup file
//! and no other. Opening, switching to or writing another file, a new file buffer, and
//! deleting the file's own buffer all fail with `E5801` — by command, by plugin API
//! (`btv.open`), by a quickfix jump — while views of the same file, the quickfix window,
//! the `:messages` panel and writes to the file itself work as ever.

use std::path::PathBuf;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, await_lines, exec_lua, feed, lines, map_get, menu_items, menu_of, message_after,
    panel_is_open, poll_menu, spawn, spawn_with_daemon_fs_init, temp_dir, DaemonFs,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

struct Fixture {
    rpc: Rpc,
    incoming: UnboundedReceiver<Incoming>,
    start: PathBuf,
    other: PathBuf,
}

async fn start(single_file: bool) -> Fixture {
    let dir = temp_dir("single_file");
    let (start, other) = (dir.join("start.txt"), dir.join("other.txt"));
    std::fs::write(&start, "start\n").unwrap();
    std::fs::write(&other, "other\n").unwrap();
    let init = ServerInit {
        file: Some(start.to_string_lossy().into_owned()),
        single_file,
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 120, 30).await;
    Fixture {
        rpc,
        incoming,
        start,
        other,
    }
}

impl Fixture {
    async fn current_file(&self) -> PathBuf {
        lines(&self.rpc).await;
        let name = exec_lua(&self.rpc, "return btv.buf.name(0)").await;
        PathBuf::from(name.as_str().unwrap_or_default())
    }

    async fn windows(&self) -> Value {
        lines(&self.rpc).await;
        exec_lua(&self.rpc, "return #btv.win.list()").await
    }

    async fn said(&mut self, keys: &str) -> String {
        message_after(&self.rpc, &mut self.incoming, keys).await
    }
}

fn locked(said: &str) -> bool {
    said.contains("E5801: this editor only edits") && said.contains("start.txt")
}

#[tokio::test]
async fn no_other_file_opens() {
    let mut f = start(true).await;
    let other = f.other.display().to_string();

    for keys in [
        format!(":e {other}<CR>"),
        format!(":drop {other}<CR>"),
        format!(":tab edit {other}<CR>"),
    ] {
        let said = f.said(&keys).await;
        assert!(locked(&said), "{keys}: {said:?}");
        assert_eq!(f.current_file().await, f.start, "{keys}");
    }
    let said = f.said(&format!(":sp {other}<CR>")).await;
    assert!(locked(&said), ":sp {said:?}");
    assert_eq!(f.windows().await, Value::from(1), "no stray split");
    let said = f.said(&format!(":tabedit {other}<CR>")).await;
    assert!(locked(&said), ":tabedit {said:?}");
    assert_eq!(f.current_file().await, f.start);
}

#[tokio::test]
async fn no_new_file_buffer() {
    let mut f = start(true).await;

    for keys in [":enew<CR>", ":new<CR>", ":vnew<CR>", ":tabnew<CR>"] {
        let said = f.said(keys).await;
        assert!(locked(&said), "{keys}: {said:?}");
        assert_eq!(f.current_file().await, f.start, "{keys}");
        assert_eq!(f.windows().await, Value::from(1), "{keys}");
    }
}

#[tokio::test]
async fn a_plugin_and_a_jump_cannot_open_another_file_either() {
    let f = start(true).await;

    exec_lua(&f.rpc, &format!("btv.open([[{}]])", f.other.display())).await;
    assert_eq!(f.current_file().await, f.start, "btv.open");

    exec_lua(
        &f.rpc,
        &format!(
            "btv.qf.setqflist({{ {{ filename = [[{}]], lnum = 1, text = 'x' }} }})",
            f.other.display()
        ),
    )
    .await;
    feed(&f.rpc, ":cfirst<CR>");
    assert_eq!(f.current_file().await, f.start, "a quickfix jump");
}

#[tokio::test]
async fn no_switching_to_another_file_buffer() {
    let mut f = start(true).await;
    // The only way another file buffer exists in a locked editor: the app made one.
    let made = f
        .rpc
        .request(
            "nvim_create_buf",
            vec![Value::from(true), Value::from(false)],
        )
        .await
        .unwrap();
    let id = made.as_u64().unwrap();

    let said = f.said(&format!(":b {id}<CR>")).await;
    assert!(locked(&said), ":b {said:?}");
    assert_eq!(f.current_file().await, f.start);
    let said = f.said(":bnext<CR>").await;
    assert!(locked(&said), ":bnext {said:?}");
    assert_eq!(f.current_file().await, f.start);
}

#[tokio::test]
async fn writes_go_to_the_file_only() {
    let mut f = start(true).await;
    let elsewhere = f.other.with_file_name("new.txt");

    let said = f.said(&format!(":w {}<CR>", elsewhere.display())).await;
    assert!(locked(&said), ":w other {said:?}");
    assert!(!elsewhere.exists(), "nothing written there");
    assert_eq!(f.current_file().await, f.start, "the buffer keeps its name");

    feed(&f.rpc, "Achanged<Esc>:w<CR>");
    lines(&f.rpc).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::fs::read_to_string(&f.start).unwrap() != "startchanged\n" {
        assert!(std::time::Instant::now() < deadline, ":w writes the file");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Naming the file itself is the file.
    feed(&f.rpc, &format!("Ax<Esc>:w {}<CR>", f.start.display()));
    lines(&f.rpc).await;
    assert_eq!(f.current_file().await, f.start);
}

#[tokio::test]
async fn its_buffer_stays() {
    let mut f = start(true).await;

    let said = f.said(":bdelete!<CR>").await;
    assert!(locked(&said), ":bd {said:?}");
    assert_eq!(f.current_file().await, f.start);
}

#[tokio::test]
async fn everything_else_works() {
    let mut f = start(true).await;

    // A second view of the same file, and re-editing it.
    feed(&f.rpc, ":sp<CR>");
    assert_eq!(f.windows().await, Value::from(2), ":sp");
    let said = f.said(&format!(":e {}<CR>", f.start.display())).await;
    assert!(!said.contains("E5801"), ":e itself {said:?}");
    feed(&f.rpc, ":only<CR>");
    // The quickfix window and the messages panel.
    feed(&f.rpc, ":copen<CR>");
    assert_eq!(f.windows().await, Value::from(2), ":copen");
    assert_eq!(
        exec_lua(&f.rpc, "return btv.bo[0].buftype").await,
        Value::from("quickfix")
    );
    feed(&f.rpc, ":cclose<CR>:messages<CR>");
    lines(&f.rpc).await;
    assert!(panel_is_open(&f.rpc).await, ":messages");
}

#[tokio::test]
async fn unlocked_by_default() {
    let f = start(false).await;

    feed(&f.rpc, &format!(":e {}<CR>", f.other.display()));
    assert_eq!(f.current_file().await, f.other);
}

#[tokio::test]
async fn a_lock_without_a_file_fails_to_start() {
    let init = ServerInit {
        single_file: true,
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    let answer = rpc.request("nvim_get_mode", vec![]).await;
    assert!(answer.is_err(), "the server never came up: {answer:?}");
}

/// A write's target is checked by the path the OS will write, not only by the name the
/// editor matches buffers on: `dir/link/../start.txt` names `start.txt` lexically, but
/// with `link` a symlink into a subdirectory the OS writes beside where `link` points.
#[cfg(unix)]
#[tokio::test]
async fn a_write_cannot_leave_through_a_symlink() {
    let f = start(true).await;
    let dir = f.start.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::os::unix::fs::symlink(dir.join("a/b"), dir.join("link")).unwrap();
    let sneaky = dir.join("link/../start.txt");
    let landed = dir.join("a/start.txt");

    feed(&f.rpc, &format!("Ax<Esc>:w {}<CR>", sneaky.display()));
    lines(&f.rpc).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::fs::read_to_string(&f.start).unwrap() != "startx\n" {
        assert!(
            std::time::Instant::now() < deadline,
            "the write went to start.txt"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!landed.exists(), "nothing written outside the file");
    assert_eq!(f.current_file().await, f.start, "the buffer keeps its name");
}

/// Only the file's own buffer is written to it: another buffer (only the app can make
/// one) written under the file's name would clobber it and take its name.
#[tokio::test]
async fn another_buffer_is_not_written_to_the_file() {
    let mut f = start(true).await;
    let made = f
        .rpc
        .request(
            "nvim_create_buf",
            vec![Value::from(true), Value::from(false)],
        )
        .await
        .unwrap();
    // Into a window that isn't the current one (the current one's buffer can't be
    // switched to another file buffer), then focus that window.
    feed(&f.rpc, ":sp<CR>");
    lines(&f.rpc).await;
    let other_win = exec_lua(
        &f.rpc,
        "for _, w in ipairs(btv.win.list()) do \
           if w ~= btv.win.current() then return w end \
         end",
    )
    .await;
    f.rpc
        .request("nvim_win_set_buf", vec![other_win.clone(), made])
        .await
        .unwrap();
    exec_lua(&f.rpc, &format!("btv.win.set_current({other_win})")).await;
    feed(&f.rpc, "imade<Esc>");
    lines(&f.rpc).await;

    let said = f.said(&format!(":w! {}<CR>", f.start.display())).await;
    assert!(locked(&said), ":w the file {said:?}");
    lines(&f.rpc).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_to_string(&f.start).unwrap(), "start\n");
    assert_eq!(
        exec_lua(&f.rpc, "return btv.buf.name(0)").await,
        Value::from(""),
        "the made buffer keeps no name"
    );
}

/// A picker's `<C-t>` / `<C-x>` on another file's item leaves no empty tab or stray split
/// behind.
#[tokio::test]
async fn a_picker_tab_or_split_jump_leaves_nothing_behind() {
    let f = start(true).await;
    for mode in ["tab", "split", "vsplit"] {
        exec_lua(
            &f.rpc,
            &format!("btv._jump_to([[{}]], 0, 0, '{mode}')", f.other.display()),
        )
        .await;
        assert_eq!(f.current_file().await, f.start, "{mode}");
        assert_eq!(f.windows().await, Value::from(1), "{mode}: no stray window");
        assert_eq!(
            exec_lua(&f.rpc, "return #btv.tabpage.list()").await,
            Value::from(1),
            "{mode}: no stray tab"
        );
    }
}

/// A picker's preview doesn't read another file either: a location in one (a global
/// mark, a jumplist entry from the shada) shows that it's out of reach, not its text.
#[tokio::test]
async fn a_preview_does_not_read_another_file() {
    let mut f = start(true).await;
    std::fs::write(&f.other, "secret\n").unwrap();
    exec_lua(
        &f.rpc,
        &format!(
            "btv.picker.source({{ name = 'elsewhere', preview = 'file', \
               items = function(ctx) ctx.push({{ text = 'other', path = [[{}]] }}) end }}) \
             btv.picker.open('elsewhere')",
            f.other.display()
        ),
    )
    .await;
    let map = poll_menu(&f.rpc, &mut f.incoming)
        .await
        .expect("the picker opens");
    let menu = menu_of(&map);
    let Some(Value::Map(preview)) = map_get(&menu, "preview") else {
        panic!("a preview pane: {menu:?}");
    };
    let shown = format!("{:?}", map_get(preview, "lines"));
    assert!(!shown.contains("secret"), "{shown}");
    assert!(shown.contains("E5801"), "{shown}");
}

/// The lock holds in a daemon session too, where the file and every other are the
/// daemon's: another isn't opened or written there, and the file itself is.
#[tokio::test]
async fn the_lock_holds_over_a_daemon() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/note.txt", "note\n"),
        ("/virtual/other.txt", "other\n"),
    ]);
    let init = ServerInit {
        file: Some("/virtual/note.txt".to_string()),
        single_file: true,
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn_with_daemon_fs_init(fake.clone(), init).await;
    await_lines(&rpc, &["note"]).await;

    let said = message_after(&rpc, &mut incoming, ":e /virtual/other.txt<CR>").await;
    assert!(
        said.contains("E5801") && said.contains("note.txt"),
        "{said:?}"
    );
    await_lines(&rpc, &["note"]).await;
    let said = message_after(&rpc, &mut incoming, ":w /virtual/elsewhere.txt<CR>").await;
    assert!(said.contains("E5801"), "{said:?}");

    feed(&rpc, "Ax<Esc>:w<CR>");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while fake.content("/virtual/note.txt").as_deref() != Some("notex\n") {
        assert!(std::time::Instant::now() < deadline, "the file was written");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(fake.content("/virtual/elsewhere.txt"), None);
}

/// Completing a file argument offers the file and nothing else: no picker over the
/// filesystem, whose names the editor would only refuse. A directory argument (`:cd`)
/// offers nothing.
#[tokio::test]
async fn completing_a_file_offers_only_the_file() {
    let dir = temp_dir("single_file_complete");
    let start = dir.join("start.txt");
    std::fs::write(&start, "start\n").unwrap();
    std::fs::write(dir.join("other.txt"), "other\n").unwrap();
    let init = ServerInit {
        file: Some(start.to_string_lossy().into_owned()),
        single_file: true,
        cmdline_complete_default: true,
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 120, 30).await;

    for keys in [":e <Tab>", ":w <Tab>", ":sp <Tab>"] {
        feed(&rpc, keys);
        let map = poll_menu(&rpc, &mut incoming).await.expect("a menu");
        assert_eq!(
            menu_items(&menu_of(&map)),
            vec![start.display().to_string()],
            "{keys}: only the file"
        );
        let title = match map_get(&map, "menu") {
            Some(Value::Map(menu)) => map_get(menu, "title").cloned(),
            _ => None,
        };
        assert_ne!(title, Some(Value::from("Select file")), "{keys}: no picker");
        feed(&rpc, "<Esc><Esc>");
        lines(&rpc).await;
    }
    feed(&rpc, ":cd <Tab>");
    assert!(
        poll_menu(&rpc, &mut incoming).await.is_none(),
        ":cd offers no directory"
    );
}
