//! The editor embedded in a mitsuami app (`bemtvi_gui::embed`): mounted, driven through
//! its handle, quit, unmounted and mounted again. Runs on mitsuami's headless backend,
//! whose `GpuSurface` has no window to present to, so the editor runs without a renderer:
//! what's tested is the editor behind the surface and its lifecycle, not its pixels.
//! Each editor's session is hermetic — no config, no shada.

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bemtvi_gui::embed::{
    Config, Editor, EditorEvent, EditorHandle, SessionOptions, Shada, Value,
    SINGLE_FILE_DISABLED_COMMANDS, SINGLE_FILE_DISABLED_PICKERS,
};
use bemtvi_gui::Session;
use mitsuami::prelude::*;
use mitsuami_test::{by_role, TestApp};

const TIMEOUT: Duration = Duration::from_secs(20);

fn session() -> Session {
    SessionOptions::new()
        .config(Config::None)
        .shada(Shada::Off)
        .spawn()
        .expect("start a session")
}

/// An editor on a fresh session; `exits` counts its `on_exit` calls.
fn editor(handle: &EditorHandle, exits: &Rc<Cell<u32>>) -> Editor {
    let exits = exits.clone();
    Editor::new(session(), move || exits.set(exits.get() + 1)).handle(handle.clone())
}

/// Poll `future` while the UI keeps turning: what the editor's IO thread sends comes in
/// through UI-thread tasks.
async fn wait<T>(app: &TestApp, future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        assert!(Instant::now() < deadline, "the editor did not answer");
        app.settle().await;
        std::thread::sleep(Duration::from_millis(2));
    }
}

async fn until(app: &TestApp, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.settle().await;
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `close(&handle)`, failing the run if it doesn't come back: a join that never ends
/// (a server waiting on a connection nobody drops) would otherwise hang the suite.
fn close(handle: &EditorHandle) -> bool {
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if finished.recv_timeout(TIMEOUT).is_err() {
            eprintln!("EditorHandle::close did not return within {TIMEOUT:?}");
            std::process::exit(1);
        }
    });
    let panicked = handle.close();
    let _ = done.send(());
    let _ = watchdog.join();
    panicked
}

async fn lua(app: &TestApp, handle: &EditorHandle, code: &str) -> Value {
    wait(app, handle.exec_lua(code))
        .await
        .unwrap_or_else(|e| panic!("{code}: {e:#}"))
}

/// The current buffer's lines, as one string per line.
async fn lines(app: &TestApp, handle: &EditorHandle) -> Vec<String> {
    let value = lua(app, handle, "return btv.buf.lines(0, 0, -1)").await;
    value
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("a line").to_owned())
        .collect()
}

/// The editor behind the surface runs: Lua runs in it and edits its buffer.
#[mitsuami_test::test]
async fn runs_the_editor_it_mounts(app: TestApp) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || Column::new().child(editor(&h, &e)));
    app.settle().await;

    assert!(handle.is_running());
    assert_eq!(lua(&app, &handle, "return 1 + 2").await, Value::from(3));
    handle.command("normal! ihello embed").unwrap();
    assert_eq!(lines(&app, &handle).await, ["hello embed"]);
    // A chunk that raises is an error, not a value.
    let err = wait(&app, handle.exec_lua("error('nope')")).await;
    assert!(
        format!("{:#}", err.unwrap_err()).contains("nope"),
        "the Lua error comes back"
    );
    assert_eq!(exits.get(), 0);
    assert!(!close(&handle), "no server thread panicked");
}

/// `:qa` quits the editor: the host's `on_exit` runs once, and the handle says it quit.
#[mitsuami_test::test]
async fn quitting_calls_on_exit_once(app: TestApp) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || Column::new().child(editor(&h, &e)));
    app.settle().await;
    // A round trip first, so the editor is up before it's asked to quit.
    lua(&app, &handle, "return true").await;

    handle.request_quit().unwrap();

    until(&app, "on_exit", || exits.get() > 0).await;
    // The IO thread says so twice (on `bemtvi_exit`, and as it ends): heard once.
    for _ in 0..20 {
        app.settle().await;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(exits.get(), 1);
    assert!(!handle.is_running());
    assert!(
        handle.command("echo 1").is_err(),
        "a quit editor takes no commands"
    );
    assert!(!close(&handle));
}

/// A modified buffer keeps `:qa` from quitting (E37), as the window's close button
/// relies on.
#[mitsuami_test::test]
async fn request_quit_refuses_while_a_buffer_is_modified(app: TestApp) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || Column::new().child(editor(&h, &e)));
    app.settle().await;
    handle.command("normal! iunsaved").unwrap();
    assert_eq!(lines(&app, &handle).await, ["unsaved"]);

    handle.request_quit().unwrap();
    // Another round trip lands after the `:qa`.
    assert_eq!(lines(&app, &handle).await, ["unsaved"]);
    assert_eq!(exits.get(), 0);
    assert!(handle.is_running());

    handle.quit_now().unwrap();
    until(&app, "on_exit", || exits.get() > 0).await;
    assert!(!close(&handle));
}

/// Unmounting the editor stops its threads: `close` finds them winding down and joins
/// them, and the handle can go on a new editor.
#[mitsuami_test::test]
async fn unmounting_stops_the_editor(app: TestApp) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let shown = signal(true);
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || Column::new().child(Show::new(shown, move || editor(&h, &e))));
    app.settle().await;
    handle.command("normal! ifirst").unwrap();
    assert_eq!(lines(&app, &handle).await, ["first"]);

    shown.set(false);
    app.settle().await;
    assert!(!handle.is_running());
    assert!(handle.command("echo 1").is_err(), "nothing is mounted");
    assert!(!close(&handle));

    // Mounted again: a new editor, a new buffer.
    shown.set(true);
    app.settle().await;
    assert!(handle.is_running());
    assert_eq!(lines(&app, &handle).await, [""]);
    assert!(!close(&handle));
}

/// The threads in this process, on Linux.
#[cfg(target_os = "linux")]
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("/proc/self/task")
        .count()
}

/// Mounting and unmounting an editor again and again leaves no threads behind.
#[cfg(target_os = "linux")]
#[mitsuami_test::test]
async fn remounting_leaves_no_threads_behind(app: TestApp) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let shown = signal(true);
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || Column::new().child(Show::new(shown, move || editor(&h, &e))));
    app.settle().await;
    lua(&app, &handle, "return true").await;
    // One cycle first, so whatever the process starts once (and keeps) is counted in.
    shown.set(false);
    app.settle().await;
    assert!(!close(&handle));
    let before = threads();

    for _ in 0..20 {
        shown.set(true);
        app.settle().await;
        lua(&app, &handle, "return true").await;
        shown.set(false);
        app.settle().await;
    }
    assert!(!close(&handle));

    until(&app, "the editors' threads to end", || threads() <= before).await;
}

/// Two editors in one window are two editors: each has its own buffer, and quitting
/// one leaves the other up.
#[mitsuami_test::test]
async fn two_editors_side_by_side(app: TestApp) {
    let (left, right) = (EditorHandle::new(), EditorHandle::new());
    let (left_exits, right_exits) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let (l, r) = (left.clone(), right.clone());
    let (le, re) = (left_exits.clone(), right_exits.clone());
    app.mount(move || Row::new().children((editor(&l, &le), editor(&r, &re))));
    app.settle().await;

    left.command("normal! ileft").unwrap();
    right.command("normal! iright").unwrap();
    assert_eq!(lines(&app, &left).await, ["left"]);
    assert_eq!(lines(&app, &right).await, ["right"]);

    left.quit_now().unwrap();
    until(&app, "the left editor's on_exit", || left_exits.get() > 0).await;
    assert!(right.is_running());
    assert_eq!(right_exits.get(), 0);
    assert_eq!(lines(&app, &right).await, ["right"]);
    assert!(!close(&left));
    assert!(!close(&right));
}

/// Mount an editor on `session` and wait for it to answer.
async fn mount(app: &TestApp, session: Session) -> (EditorHandle, Rc<Cell<u32>>) {
    let handle = EditorHandle::new();
    let exits = Rc::new(Cell::new(0));
    let (h, e) = (handle.clone(), exits.clone());
    app.mount(move || {
        Column::new().child(Editor::new(session, move || e.set(e.get() + 1)).handle(h.clone()))
    });
    app.settle().await;
    lua(app, &handle, "return true").await;
    (handle, exits)
}

/// Set an environment variable for the rest of this (single-threaded) test run's setup;
/// what reads it reads it while a session starts.
fn set_env(name: &str, value: impl AsRef<std::ffi::OsStr>) {
    std::env::set_var(name, value);
}

/// `Config::Dir` sources the app's own `init.lua`.
#[mitsuami_test::test]
async fn config_dir_runs_its_init(app: TestApp) {
    let dir = bemtvi_test_harness::temp_dir("embed_config");
    std::fs::write(dir.join("init.lua"), "btv.g.embed_config = 'the app'\n").unwrap();
    let session = SessionOptions::new()
        .config(Config::Dir(dir))
        .shada(Shada::Off)
        .spawn()
        .unwrap();
    let (handle, _) = mount(&app, session).await;

    assert_eq!(
        lua(&app, &handle, "return btv.g.embed_config").await,
        Value::from("the app")
    );
    assert!(!close(&handle));
}

/// `Config::None` runs no `init.lua` — not even the user's, wherever `$BEMTVI_CONFIG`
/// points.
#[mitsuami_test::test]
async fn config_none_runs_no_init(app: TestApp) {
    let dir = bemtvi_test_harness::temp_dir("embed_user_config");
    std::fs::write(dir.join("init.lua"), "btv.g.embed_config = 'the user'\n").unwrap();
    let before = std::env::var_os("BEMTVI_CONFIG");
    set_env("BEMTVI_CONFIG", &dir);
    let session = SessionOptions::new()
        .config(Config::None)
        .shada(Shada::Off)
        .spawn();
    match before {
        Some(v) => set_env("BEMTVI_CONFIG", v),
        None => std::env::remove_var("BEMTVI_CONFIG"),
    }
    let (handle, _) = mount(&app, session.unwrap()).await;

    assert_eq!(
        lua(&app, &handle, "return btv.g.embed_config").await,
        Value::Nil
    );
    assert!(!close(&handle));
}

/// Every shada store file under `dir`.
fn store_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if bemtvi_server::is_store_file(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Start a session with `shada`, its state dir under `state`, edit, and quit it.
async fn edit_and_quit(app: &TestApp, state: &std::path::Path, shada: Shada) {
    let before = (
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("LOCALAPPDATA"),
    );
    set_env("XDG_STATE_HOME", state);
    set_env("LOCALAPPDATA", state);
    let session = SessionOptions::new()
        .config(Config::None)
        .shada(shada)
        .spawn();
    for (name, value) in [("XDG_STATE_HOME", before.0), ("LOCALAPPDATA", before.1)] {
        match value {
            Some(v) => set_env(name, v),
            None => std::env::remove_var(name),
        }
    }
    let (handle, exits) = mount(app, session.unwrap()).await;
    handle.command("normal! ihistory").unwrap();
    handle.command("let @a = 'a register'").unwrap();
    lua(app, &handle, "return true").await;
    handle.quit_now().unwrap();
    until(app, "on_exit", || exits.get() > 0).await;
    // The exit sequence (its shada flush) has run once the server thread is joined.
    assert!(!close(&handle));
}

/// `Shada::Namespace` keeps a store of its own under the shada dir's `ns/<name>`.
#[mitsuami_test::test]
async fn shada_namespace_keeps_its_own_store(app: TestApp) {
    let state = bemtvi_test_harness::temp_dir("embed_state");
    edit_and_quit(&app, &state, Shada::Namespace("embedtest".into())).await;

    let files = store_files(&state);
    assert!(!files.is_empty(), "a store was written under {state:?}");
    assert!(
        files
            .iter()
            .all(|f| f.components().any(|c| c.as_os_str() == "embedtest")),
        "only in the namespace: {files:?}"
    );
}

/// `Shada::Off` persists nothing.
#[mitsuami_test::test]
async fn shada_off_writes_nothing(app: TestApp) {
    let state = bemtvi_test_harness::temp_dir("embed_state_off");
    edit_and_quit(&app, &state, Shada::Off).await;

    assert_eq!(store_files(&state), Vec::<std::path::PathBuf>::new());
}

/// A namespace that isn't a safe path component fails the session's start.
#[mitsuami_test::test]
async fn an_unsafe_shada_namespace_fails_the_start(_app: TestApp) {
    let started = SessionOptions::new()
        .config(Config::None)
        .shada(Shada::Namespace("../elsewhere".into()))
        .spawn();
    let err = started.err().expect("refused");
    assert!(format!("{err:#}").contains("namespace"), "{err:#}");
}

/// Off by default, the session can't switch: `:workspace` isn't a command, and a
/// plugin's reconnect or a `:connect` is refused in the editor, saying why.
#[mitsuami_test::test]
async fn session_switching_is_off_by_default(app: TestApp) {
    let (handle, exits) = mount(&app, session()).await;

    assert_eq!(
        lua(
            &app,
            &handle,
            "return btv.user_command.get().workspace ~= nil"
        )
        .await,
        Value::from(false),
        "no :workspace"
    );
    let reconnect = lua(
        &app,
        &handle,
        "local ok, err = pcall(btv.session.reconnect, \
           { transport = { kind = 'quic', addr = 'bemtvi://127.0.0.1:1/t?cert=00' } }) \
         return { ok, tostring(err) }",
    )
    .await;
    let reconnect = reconnect.as_array().unwrap();
    assert_eq!(reconnect[0], Value::from(false));
    assert!(
        reconnect[1]
            .as_str()
            .unwrap()
            .contains("can't switch sessions"),
        "{reconnect:?}"
    );
    // `:connect` reports it (the command's errors are notifications).
    let connect = lua(
        &app,
        &handle,
        "local said, notify = nil, btv.notify \
         btv.notify = function(msg) said = msg end \
         btv.connect.connect('example.invalid') \
         btv.notify = notify \
         return said",
    )
    .await;
    assert!(
        connect
            .as_str()
            .is_some_and(|m| m.contains("can't switch sessions")),
        "{connect:?}"
    );
    assert!(handle.is_running());
    assert_eq!(exits.get(), 0);
    assert!(!close(&handle));
}

/// Turned on, `:workspace` is a command (the window swaps the session on `<CR>`).
#[mitsuami_test::test]
async fn session_switching_on_registers_workspace(app: TestApp) {
    let session = SessionOptions::new()
        .config(Config::None)
        .shada(Shada::Off)
        .session_switching(true)
        .spawn()
        .unwrap();
    let (handle, _) = mount(&app, session).await;

    assert_eq!(
        lua(
            &app,
            &handle,
            "return btv.user_command.get().workspace ~= nil"
        )
        .await,
        Value::from(true)
    );
    assert!(!close(&handle));
}

/// An editor whose app is `testapp`, recording what it tells the app.
async fn mount_hosted(
    app: &TestApp,
    session: Session,
) -> (EditorHandle, Rc<std::cell::RefCell<Vec<EditorEvent>>>) {
    let handle = EditorHandle::new();
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let (h, ev) = (handle.clone(), events.clone());
    app.mount(move || {
        Column::new().child(
            Editor::new(session, || {})
                .handle(h.clone())
                .on_event(move |event| ev.borrow_mut().push(event)),
        )
    });
    app.settle().await;
    lua(app, &handle, "return true").await;
    (handle, events)
}

fn hosted_session() -> Session {
    SessionOptions::new()
        .config(Config::None)
        .shada(Shada::Off)
        .host("testapp")
        .spawn()
        .unwrap()
}

fn field<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    payload
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// What the editor's Lua sends its host reaches the app's `on_event`, and the app can
/// drive the editor again from in there.
#[mitsuami_test::test]
async fn the_app_hears_what_the_editor_sends(app: TestApp) {
    let (handle, events) = mount_hosted(&app, hosted_session()).await;
    assert_eq!(
        lua(&app, &handle, "return btv.host.name()").await,
        Value::from("testapp")
    );

    lua(&app, &handle, "btv.host.send('preview', { file = 'a.md' })").await;
    until(&app, "the host message", || !events.borrow().is_empty()).await;
    let EditorEvent::Host { name, payload } = events.borrow()[0].clone();
    assert_eq!(name, "preview");
    assert_eq!(field(&payload, "file"), Some(&Value::from("a.md")));
    assert!(!close(&handle));
}

/// The handler runs with the editor free: it can message the editor back.
#[mitsuami_test::test]
async fn the_app_can_answer_from_its_handler(app: TestApp) {
    let handle = EditorHandle::new();
    let (h, answer) = (handle.clone(), handle.clone());
    app.mount(move || {
        Column::new().child(
            Editor::new(hosted_session(), || {})
                .handle(h.clone())
                .on_event(move |_| answer.command("lua btv.g.answered = 'yes'").unwrap()),
        )
    });
    app.settle().await;

    lua(&app, &handle, "btv.host.send('ping')").await;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if lua(&app, &handle, "return btv.g.answered").await == Value::from("yes") {
            break;
        }
        assert!(Instant::now() < deadline, "the handler answered");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!close(&handle));
}

/// `btv.host.forward` turns edits into `changed` events for the app.
#[mitsuami_test::test]
async fn forwarded_edits_reach_the_app(app: TestApp) {
    let (handle, events) = mount_hosted(&app, hosted_session()).await;
    lua(&app, &handle, "btv.host.forward({ 'changed' })").await;

    handle.command("normal! ihello").unwrap();
    until(&app, "a changed event", || !events.borrow().is_empty()).await;
    let EditorEvent::Host { name, payload } = events.borrow()[0].clone();
    assert_eq!(name, "changed");
    assert_eq!(field(&payload, "modified"), Some(&Value::from(true)));
    assert!(!close(&handle));
}

/// `open` edits a file; `focus` gives the editor the keyboard.
#[mitsuami_test::test]
async fn open_and_focus(app: TestApp) {
    let (handle, _) = mount(&app, session()).await;
    let path = bemtvi_test_harness::temp_path("embed open");
    std::fs::write(&path, "from disk\n").unwrap();

    handle.open(&path).unwrap();
    assert_eq!(lines(&app, &handle).await, ["from disk"]);
    handle.focus().unwrap();
    app.settle().await;
    app.expect(by_role(Role::Image, "bemtvi"))
        .to_be_focused()
        .await;
    assert!(!close(&handle));
}

/// `open` takes the path as one file name, whatever is in it: a `|` doesn't run what
/// follows it, and `%` / `#` aren't the current and alternate file.
#[cfg(unix)]
#[mitsuami_test::test]
async fn open_takes_the_path_as_it_is(app: TestApp) {
    let (handle, _) = mount(&app, session()).await;
    let dir = bemtvi_test_harness::temp_dir("embed_open_as_is");
    let path = dir.join("a %b #c|lua btv.g.ran = true");
    std::fs::write(&path, "as it is\n").unwrap();

    handle.open(&path).unwrap();
    assert_eq!(lines(&app, &handle).await, ["as it is"]);
    assert_eq!(
        lua(&app, &handle, "return btv.g.ran == true").await,
        Value::from(false),
        "nothing after the | ran"
    );
    assert!(!close(&handle));
}

/// `user_commands_from_startup_only`: once startup ends, Lua the user types can't define
/// a command, and the app's own `exec_lua` still can.
#[mitsuami_test::test]
async fn user_commands_only_from_startup(app: TestApp) {
    let dir = bemtvi_test_harness::temp_dir("embed_startup_cmds");
    std::fs::write(
        dir.join("init.lua"),
        "btv.on('PluginsLoaded', function() btv.g.loaded = true end)\n",
    )
    .unwrap();
    let session = SessionOptions::new()
        .config(Config::Dir(dir))
        .shada(Shada::Off)
        .user_commands_from_startup_only(true)
        .spawn()
        .unwrap();
    let (handle, _) = mount(&app, session).await;
    let deadline = Instant::now() + TIMEOUT;
    while lua(&app, &handle, "return btv.g.loaded == true").await != Value::from(true) {
        assert!(Instant::now() < deadline, "startup ends");
        std::thread::sleep(Duration::from_millis(10));
    }

    handle
        .command("lua btv.command('Typed', function() end)")
        .unwrap();
    assert_eq!(
        lua(&app, &handle, "return btv.user_command.get().Typed ~= nil").await,
        Value::from(false)
    );
    assert_eq!(
        lua(
            &app,
            &handle,
            "btv.command('FromApp', function() end) return btv.user_command.get().FromApp ~= nil"
        )
        .await,
        Value::from(true)
    );
    assert!(!close(&handle));
}

/// A note to edit, a file beside it, and a config defining a command, for the
/// single-file preset.
fn single_file_fixture() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = bemtvi_test_harness::temp_dir("embed_single_file");
    let (note, other, config) = (
        dir.join("note.txt"),
        dir.join("other.txt"),
        dir.join("config"),
    );
    std::fs::write(&note, "note\n").unwrap();
    std::fs::write(&other, "other\n").unwrap();
    std::fs::create_dir(&config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "btv.command('Stamp', function() btv.g.stamped = 1 end)\n\
         btv.on('PluginsLoaded', function() btv.g.loaded = true end)\n",
    )
    .unwrap();
    (note, other, config)
}

async fn started(app: &TestApp, handle: &EditorHandle) {
    let deadline = Instant::now() + TIMEOUT;
    while lua(app, handle, "return btv.g.loaded == true").await != Value::from(true) {
        assert!(Instant::now() < deadline, "startup ends");
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn current_name(app: &TestApp, handle: &EditorHandle) -> String {
    let name = lua(app, handle, "return btv.buf.name(0)").await;
    name.as_str().unwrap_or_default().to_owned()
}

/// `SessionOptions::single_file`: the file and nothing else, no Lua or processes, the
/// config's own commands still there.
#[mitsuami_test::test]
async fn the_single_file_preset(app: TestApp) {
    let (note, other, config) = single_file_fixture();
    let session = SessionOptions::single_file(note.to_string_lossy())
        .config(Config::Dir(config))
        .shada(Shada::Off)
        .spawn()
        .unwrap();
    let (handle, _) = mount(&app, session).await;
    started(&app, &handle).await;
    let note_name = note.to_string_lossy().into_owned();

    // Another file: refused by the lock.
    handle
        .command(&format!("edit {}", other.display()))
        .unwrap();
    assert_eq!(current_name(&app, &handle).await, note_name);
    // Lua and processes: disabled.
    handle.command("lua btv.g.ran = 1").unwrap();
    assert_eq!(lua(&app, &handle, "return btv.g.ran").await, Value::Nil);
    handle.command("terminal").unwrap();
    assert_eq!(
        lua(&app, &handle, "return #btv.win.list()").await,
        Value::from(1)
    );
    for name in SINGLE_FILE_DISABLED_COMMANDS {
        assert_eq!(
            lua(
                &app,
                &handle,
                &format!("return btv.command_disabled([[{name}]])")
            )
            .await,
            Value::from(true),
            "{name}"
        );
    }
    // Installing a grammar runs processes (a download, the C compiler): taken too.
    assert_eq!(
        lua(
            &app,
            &handle,
            "return { btv.command_disabled('TSInstall'), btv.command_disabled('TSUpdate') }"
        )
        .await,
        Value::Array(vec![Value::from(true), Value::from(true)])
    );
    // The plugin manager and LSP are off; the pickers that reach past the file are gone.
    assert_eq!(
        lua(
            &app,
            &handle,
            "return { btv.plugins.disabled(), btv.lsp.disabled() }"
        )
        .await,
        Value::Array(vec![Value::from(true), Value::from(true)])
    );
    for name in SINGLE_FILE_DISABLED_PICKERS {
        assert_eq!(
            lua(
                &app,
                &handle,
                &format!("return btv.picker.disabled('{name}')")
            )
            .await,
            Value::from(true),
            "{name}"
        );
    }
    // The config's command, `:e!` (revert) and `:w` still work.
    handle.command("Stamp").unwrap();
    assert_eq!(
        lua(&app, &handle, "return btv.g.stamped").await,
        Value::from(1)
    );
    handle.command("normal! Achanged").unwrap();
    assert_eq!(lines(&app, &handle).await, ["notechanged"]);
    handle.command("edit!").unwrap();
    assert_eq!(lines(&app, &handle).await, ["note"]);
    handle.command("normal! Asaved").unwrap();
    handle.command("write").unwrap();
    lua(&app, &handle, "return true").await;
    let deadline = Instant::now() + TIMEOUT;
    while std::fs::read_to_string(&note).unwrap() != "notesaved\n" {
        assert!(Instant::now() < deadline, ":w writes the note");
        app.settle().await;
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!close(&handle));
}

/// `enable_commands` gives a disabled command back.
#[mitsuami_test::test]
async fn the_preset_gives_commands_back(app: TestApp) {
    let (note, _, config) = single_file_fixture();
    let session = SessionOptions::single_file(note.to_string_lossy())
        .config(Config::Dir(config))
        .shada(Shada::Off)
        .enable_commands(["lua"])
        .spawn()
        .unwrap();
    let (handle, _) = mount(&app, session).await;

    handle.command("lua btv.g.ran = 1").unwrap();
    assert_eq!(lua(&app, &handle, "return btv.g.ran").await, Value::from(1));
    assert_eq!(
        lua(&app, &handle, "return btv.command_disabled('terminal')").await,
        Value::from(true),
        "the rest stay disabled"
    );
    assert!(!close(&handle));
}

mitsuami_test::main!();
