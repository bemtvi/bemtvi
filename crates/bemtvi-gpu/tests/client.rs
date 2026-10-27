//! The client with no toolkit and no window: a real server behind a [`Client`] attached
//! headless, driven by its keys, its IO thread's frames and its timers, the way a host
//! drives it. What it asks of its host ([`Effect`]s) is read back, and answered.
//!
//! These are the paths that lived in the mitsuami view before the client was its own,
//! where no test could reach them (the headless mitsuami surface never attaches a grid,
//! so it never gets a frame).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bemtvi_gpu::{
    spawn_session_with, Answer, Client, ClientOptions, Config, Effect, InputEvent, IoThread,
    KeyCode, Launch, Modifiers, Shada,
};
use bemtvi_server::ConfigSource;
use rmpv::Value;

const TIMEOUT: Duration = Duration::from_secs(10);

const NONE: Modifiers = Modifiers {
    shift: false,
    control: false,
    alt: false,
    meta: false,
};

/// A client on a fresh session (no config, no shada) editing `file`, attached headless
/// at 80×24, with what it has asked for so far.
struct Fixture {
    client: Client,
    io: Option<IoThread>,
    effects: Vec<Effect>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(io) = self.io.take() {
            io.join();
        }
    }
}

fn start(file: Option<&Path>, launch: Launch) -> Fixture {
    start_with(file, launch, ClientOptions::default(), false)
}

/// [`start`] with the client's own options, on a session that says it's `remote` (an
/// edit-host's) or not.
fn start_with(
    file: Option<&Path>,
    launch: Launch,
    options: ClientOptions,
    remote: bool,
) -> Fixture {
    let launch = Launch {
        config: Config::None,
        shada: Shada::Off,
        ..launch
    };
    let session = spawn_session_with(
        None,
        file.map(|f| f.to_string_lossy().into_owned()),
        ConfigSource::Local,
        launch,
    )
    .expect("start a session");
    let session = bemtvi_gpu::Session { remote, ..session };
    let (mut client, io) = Client::new(session, options, Arc::new(|| {}));
    client.attach_headless(80, 24);
    Fixture {
        client,
        io: Some(io),
        effects: Vec::new(),
    }
}

impl Fixture {
    /// One turn of a host's loop: drain the IO thread, run the timers that came due,
    /// collect what the client asked for.
    fn turn(&mut self) {
        self.client.pump();
        self.client.tick(Instant::now());
        self.effects.extend(self.client.take_effects());
    }

    async fn until(&mut self, what: &str, done: impl Fn(&mut Fixture) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.turn();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Keep turning for `wait`, for a "nothing happens" assertion.
    async fn idle(&mut self, wait: Duration) {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            self.turn();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn key(&mut self, code: KeyCode, text: Option<&str>, modifiers: Modifiers) {
        let unmodified = match code {
            KeyCode::Character(c) => Some(c),
            _ => None,
        };
        self.client.input(&InputEvent::Key {
            code,
            pressed: true,
            text: text.map(str::to_owned),
            unmodified,
            modifiers,
        });
        self.turn();
    }

    /// Type `text` and then `<CR>` faster than the editor draws: no frame is taken in
    /// between, so what the client last drew is behind what the editor has.
    fn type_ahead(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.client.input(&InputEvent::Key {
                code: KeyCode::Character(c),
                pressed: true,
                text: Some(s),
                unmodified: Some(c),
                modifiers: NONE,
            });
        }
        self.client.input(&InputEvent::Key {
            code: KeyCode::Enter,
            pressed: true,
            text: None,
            unmodified: None,
            modifiers: NONE,
        });
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.key(KeyCode::Character(c), Some(&s), NONE);
        }
    }

    async fn lua(&mut self, code: &str) -> Value {
        let result = self.client.handle().exec_lua(code).await;
        self.turn();
        result.unwrap_or_else(|e| panic!("{code}: {e:#}"))
    }

    /// Wait for the first frame (the attach is answered with one).
    async fn framed(&mut self) {
        self.until("the first frame", |f| !f.client.view().windows.is_empty())
            .await;
    }

    /// Type `:{line}`, and wait until the command line shows it.
    async fn cmdline(&mut self, line: &str) {
        self.type_text(&format!(":{line}"));
        let want = line.to_owned();
        self.until(&format!("the command line `:{line}`"), move |f| {
            f.client.view().command_mode && f.client.view().cmdline == want
        })
        .await;
    }

    /// Wait for the editor to ask for the open dialog, and take the request.
    async fn open_dialog(&mut self) -> Effect {
        self.until("the open dialog", |f| {
            f.effects
                .iter()
                .any(|e| matches!(e, Effect::OpenDialog { .. }))
        })
        .await;
        self.take(|e| matches!(e, Effect::OpenDialog { .. }))
            .expect("the open dialog")
    }

    fn take(&mut self, pick: impl Fn(&Effect) -> bool) -> Option<Effect> {
        let at = self.effects.iter().position(pick)?;
        Some(self.effects.remove(at))
    }
}

#[tokio::test]
async fn frames_arrive_and_keys_type() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.type_text("ihello");
    f.key(KeyCode::Escape, None, NONE);
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.lines(0, 0, -1)[1]").await != Value::from("hello") {
        assert!(Instant::now() < deadline, "the keys typed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        f.effects.contains(&Effect::Redraw),
        "frames ask for a repaint"
    );
}

/// `<CR>` on `:e` asks the host for the open dialog; the file it answers with is
/// opened, escaped, so a `|` or `%` in its name is part of the name.
#[tokio::test]
async fn the_open_dialog_is_the_hosts_and_its_answer_opens_the_file() {
    let dir = bemtvi_test_harness::temp_dir("client_dialog");
    let picked = dir.join("a %b|lua btv.g.ran = true");
    std::fs::write(&picked, "picked\n").unwrap();
    let mut f = start(None, Launch::default());
    f.framed().await;

    f.cmdline("e").await;
    f.key(KeyCode::Enter, None, NONE);
    let Effect::OpenDialog { request, .. } = f.open_dialog().await else {
        unreachable!()
    };
    f.client.answer(request, Answer::Path(Some(picked.clone())));
    let want = picked.to_string_lossy().into_owned();
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.name(0)").await != Value::from(want.as_str()) {
        assert!(Instant::now() < deadline, "the picked file opened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        f.lua("return btv.buf.lines(0, 0, -1)[1]").await,
        Value::from("picked")
    );
    assert_eq!(f.lua("return btv.g.ran == true").await, Value::from(false));
}

/// A paste gesture asks the host for the clipboard, and its text goes in.
#[tokio::test]
async fn a_paste_reads_the_hosts_clipboard() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.type_text("i");
    let control_shift = Modifiers {
        control: true,
        shift: true,
        ..NONE
    };
    f.key(KeyCode::Character('v'), None, control_shift);
    let Some(Effect::ReadClipboard { request }) =
        f.take(|e| matches!(e, Effect::ReadClipboard { .. }))
    else {
        panic!("no clipboard read asked for: {:?}", f.effects);
    };
    f.client
        .answer(request, Answer::Text(Some("from the clipboard".into())));
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.lines(0, 0, -1)[1]").await != Value::from("from the clipboard") {
        assert!(Instant::now() < deadline, "the clipboard was pasted");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Type `:workspace {dir}<CR>` into an editor that may switch sessions, and say whether
/// the session switched (the Lua state it had is gone).
async fn workspace_switches(disabled: &[&str]) -> bool {
    workspace_switches_with(Launch {
        session_switching: true,
        disabled_commands: disabled.iter().map(|c| c.to_string()).collect(),
        ..Launch::default()
    })
    .await
}

async fn workspace_switches_with(launch: Launch) -> bool {
    let dir = bemtvi_test_harness::temp_dir("client_workspace");
    let mut f = start(None, launch);
    f.framed().await;
    f.lua("btv.g.first_session = true").await;
    f.cmdline(&format!("workspace {}", dir.display())).await;
    f.key(KeyCode::Enter, None, NONE);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        f.turn();
        // Mid-switch the old connection closes before the client has the new one: not
        // an answer yet.
        let first = f
            .client
            .handle()
            .exec_lua("return btv.g.first_session == true")
            .await;
        if first.is_ok_and(|v| v != Value::from(true)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn workspace_switches_the_session() {
    assert!(workspace_switches(&[]).await);
}

/// A `:workspace` the app disabled doesn't run, so it doesn't switch either.
#[tokio::test]
async fn a_disabled_workspace_does_not_switch() {
    assert!(!workspace_switches(&["workspace"]).await);
}

/// An editor launched with session switching off doesn't switch on `:workspace`.
#[tokio::test]
async fn workspace_does_not_switch_with_switching_off() {
    assert!(
        !workspace_switches_with(Launch {
            session_switching: false,
            ..Launch::default()
        })
        .await
    );
}

/// The modeless caret's blink is a deadline: due within one half of the blink, a tick
/// at it asks for a repaint, and the next one is later (a host sleeping until it never
/// spins).
#[tokio::test]
async fn the_caret_blink_is_a_deadline_that_moves_on() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.lua("btv.cmd('set modeless')").await;
    f.until("the modeless caret", |f| f.client.view().caret)
        .await;
    let now = Instant::now();
    let due = f
        .client
        .next_deadline()
        .expect("a caret has a blink deadline");
    assert!(
        due <= now + bemtvi_gpu::CARET_BLINK,
        "due within a half: {due:?} vs {now:?}"
    );
    f.client.take_effects();
    f.client.tick(due);
    assert!(
        f.client.take_effects().contains(&Effect::Redraw),
        "the flip repaints"
    );
    let next = f.client.next_deadline().expect("still blinking");
    assert!(next > due, "the deadline moved on: {next:?} vs {due:?}");
    // Ticking again at the same instant does nothing more: the deadline is ahead.
    f.client.tick(due);
    assert!(!f.client.take_effects().contains(&Effect::Redraw));
}

/// A mapped prefix the server holds back is resolved by the client's `timeoutlen`
/// flush, from `tick`, once the keys pause.
#[tokio::test]
async fn the_timeoutlen_flush_comes_from_tick() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.lua("vim.o.timeoutlen = 50; btv.keymap.set('i', 'jk', '<Esc>')")
        .await;
    // A frame after the option, so the client knows the new `'timeoutlen'`.
    f.idle(Duration::from_millis(100)).await;
    f.type_text("ij");
    assert!(
        f.client.next_deadline().is_some(),
        "a key arms the flush deadline"
    );
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.lines(0, 0, -1)[1]").await != Value::from("j") {
        assert!(Instant::now() < deadline, "the held-back j went in");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `:qa` ends the editor: the host hears `Exit`, once, and the handle refuses commands
/// from then on.
#[tokio::test]
async fn exit_is_heard_once() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.cmdline("qa").await;
    f.key(KeyCode::Enter, None, NONE);
    f.until("Exit", |f| f.effects.contains(&Effect::Exit)).await;
    f.idle(Duration::from_millis(200)).await;
    assert_eq!(f.effects.iter().filter(|e| **e == Effect::Exit).count(), 1);
    assert!(f.client.exited());
    assert!(f.client.handle().command("echo 1").is_err());
}

/// A picked file whose name holds a line break (a name Linux allows) is still one file:
/// the line after the break isn't run as a command of its own.
#[cfg(unix)]
#[tokio::test]
async fn a_picked_name_with_a_line_break_is_one_file() {
    let dir = bemtvi_test_harness::temp_dir("client_dialog_newline");
    let picked = dir.join("a\nlua btv.g.ran = true");
    std::fs::write(&picked, "picked\n").unwrap();
    let mut f = start(None, Launch::default());
    f.framed().await;

    f.cmdline("e").await;
    f.key(KeyCode::Enter, None, NONE);
    let Effect::OpenDialog { request, .. } = f.open_dialog().await else {
        unreachable!()
    };
    f.client.answer(request, Answer::Path(Some(picked.clone())));
    let want = picked.to_string_lossy().into_owned();
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.name(0)").await != Value::from(want.as_str()) {
        assert!(
            Instant::now() < deadline,
            "the picked file opened: {:?}",
            f.lua("return btv.buf.name(0)").await
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.lua("return btv.g.ran == true").await, Value::from(false));
}

/// A session switch that fails says why on the message line, and the why is only text:
/// a `|` in it (a URL's, a path's) doesn't end the message and run the rest as a
/// command.
#[tokio::test]
async fn a_failed_switch_reports_its_text_and_runs_none_of_it() {
    let mut f = start(
        None,
        Launch {
            session_switching: true,
            ..Launch::default()
        },
    );
    f.framed().await;
    // No connector claims it and it's no target the client dials: the client reports it.
    // A digit after an escaped mark, and a letter beyond ASCII, come through as typed.
    let url = "x://h|lua btv.g.ran = true --/9 \u{e9}";
    f.lua(&format!("btv.connect.connect({url:?})")).await;
    f.until("the failure on the message line", |f| {
        f.client.view().message.contains("not a connect target")
    })
    .await;
    let view = f.client.view();
    assert!(view.message_error, "an error: {:?}", view.message);
    assert!(
        view.message.contains(url),
        "the whole URL: {:?}",
        view.message
    );
    assert_eq!(f.lua("return btv.g.ran == true").await, Value::from(false));
}

/// A file dialog still open when the editor switches sessions answers for the session
/// it was asked in: its pick doesn't run in the new one (whose files may be on another
/// machine, and whose command line the old `:e` never touched).
#[tokio::test]
async fn a_dialog_answered_after_a_session_switch_does_not_run_in_the_new_session() {
    let dir = bemtvi_test_harness::temp_dir("client_dialog_switch");
    let picked = dir.join("picked.txt");
    std::fs::write(&picked, "picked\n").unwrap();
    let mut f = start(
        None,
        Launch {
            session_switching: true,
            ..Launch::default()
        },
    );
    f.framed().await;

    f.cmdline("e").await;
    f.key(KeyCode::Enter, None, NONE);
    let Effect::OpenDialog { request, .. } = f.open_dialog().await else {
        unreachable!()
    };
    // While the dialog is up, the editor moves onto a new session.
    f.key(KeyCode::Escape, None, NONE);
    f.lua("btv.g.first_session = true").await;
    f.cmdline(&format!("workspace {}", dir.display())).await;
    f.key(KeyCode::Enter, None, NONE);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        f.turn();
        let first = f
            .client
            .handle()
            .exec_lua("return btv.g.first_session == true")
            .await;
        if first.is_ok_and(|v| v != Value::from(true)) {
            break;
        }
        assert!(Instant::now() < deadline, "the session switched");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f.framed().await;

    f.client.answer(request, Answer::Path(Some(picked.clone())));
    f.idle(Duration::from_millis(300)).await;
    let name = f.lua("return btv.buf.name(0)").await;
    assert_ne!(
        name,
        Value::from(picked.to_string_lossy().as_ref()),
        "the old session's pick opened in the new one"
    );
}

/// A surface attached after the last one went starts with the platform's defaults, not
/// what the client asked of the old one: what the editor still wants of it (input
/// methods off under `'imdisable'`) is asked for again.
#[tokio::test]
async fn a_new_surface_hears_what_the_old_one_was_asked() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.lua("vim.o.imdisable = true").await;
    f.until("input methods off", |f| {
        f.effects.contains(&Effect::InputMethod(false))
    })
    .await;

    f.client.detach_surface();
    f.effects.clear();
    f.client.attach_headless(80, 24);
    f.turn();
    assert!(
        f.effects.contains(&Effect::InputMethod(false)),
        "the new surface keeps input methods off: {:?}",
        f.effects
    );
}

/// The editor given a directory to open in (`ClientOptions::open_dir`, what
/// `bemtvi-gui somedir` passes) on a local session asks for the open dialog there.
#[tokio::test]
async fn open_dir_asks_for_the_dialog_there() {
    let dir = bemtvi_test_harness::temp_dir("client_open_dir");
    let options = ClientOptions {
        open_dir: Some(dir.clone()),
        ..ClientOptions::default()
    };
    let mut f = start_with(None, Launch::default(), options, false);
    f.turn();
    let Some(Effect::OpenDialog { start_dir, .. }) =
        f.take(|e| matches!(e, Effect::OpenDialog { .. }))
    else {
        panic!("no open dialog asked for: {:?}", f.effects);
    };
    assert_eq!(start_dir, Some(dir));
}

/// Where the open dialog is no answer to a directory — an edit-host session, whose
/// files are on another machine than the dialog's, or an app that turned the dialogs
/// off — `open_dir` opens it the way `:e {dir}` typed does, in the editor.
async fn open_dir_is_edited_as_typed(options: ClientOptions, remote: bool) {
    let dir = bemtvi_test_harness::temp_dir("client_open_dir_typed");
    std::fs::write(dir.join("listed.txt"), "listed\n").unwrap();
    let options = ClientOptions {
        open_dir: Some(dir.clone()),
        ..options
    };
    let mut f = start_with(None, Launch::default(), options, remote);
    f.framed().await;
    assert!(
        !f.effects
            .iter()
            .any(|e| matches!(e, Effect::OpenDialog { .. })),
        "no local dialog: {:?}",
        f.effects
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let listed = f
            .lua("return table.concat(btv.buf.lines(0, 0, -1), '\\n')")
            .await;
        if listed.as_str().is_some_and(|l| l.contains("listed.txt")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the directory listed: {listed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn open_dir_on_an_edit_host_session_is_edited_as_typed() {
    open_dir_is_edited_as_typed(ClientOptions::default(), true).await;
}

#[tokio::test]
async fn open_dir_with_dialogs_off_is_edited_as_typed() {
    let options = ClientOptions {
        native_dialogs: false,
        ..ClientOptions::default()
    };
    open_dir_is_edited_as_typed(options, false).await;
}

/// Closing the editor while a `:connect` is still dialing doesn't wait for the dial: a
/// daemon that never answers (a dead host, an ssh hop stuck on its handshake) would
/// otherwise hold the close until the dial gave up.
#[tokio::test]
async fn closing_mid_connect_does_not_wait_for_the_dial() {
    // A UDP port that takes the QUIC handshake's packets and never answers them.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    let port = silent.local_addr().expect("addr").port();
    let mut f = start(
        None,
        Launch {
            session_switching: true,
            ..Launch::default()
        },
    );
    f.framed().await;
    let cert = vec!["ab"; 32].join(":");
    let url = format!("bemtvi://127.0.0.1:{port}/token?cert={cert}");
    f.lua(&format!("btv.connect.connect({url:?})")).await;
    // The dial is under way: the handshake's first packet has reached the port.
    let mut buf = [0u8; 2048];
    silent
        .set_read_timeout(Some(TIMEOUT))
        .expect("read timeout");
    silent
        .recv_from(&mut buf)
        .expect("the dial reaches the port");
    let io = f.io.take().expect("io thread");
    let closing = Instant::now();
    io.join();
    assert!(
        closing.elapsed() < Duration::from_secs(2),
        "the close waited {:?} on the dial",
        closing.elapsed()
    );
}

/// `:eo` typed and entered before its frame arrives still asks for the open dialog: the
/// editor decides on the line it runs, not the client on the one it last drew.
#[tokio::test]
async fn a_dialog_line_typed_ahead_of_its_frame_asks_for_the_dialog() {
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.type_ahead(":eo");
    f.open_dialog().await;
}

/// `:workspace {dir}` typed and entered before its frame arrives still switches.
#[tokio::test]
async fn a_workspace_typed_ahead_of_its_frame_switches() {
    let dir = bemtvi_test_harness::temp_dir("client_workspace_ahead");
    let mut f = start(
        None,
        Launch {
            session_switching: true,
            ..Launch::default()
        },
    );
    f.framed().await;
    f.lua("btv.g.first_session = true").await;
    f.type_ahead(&format!(":workspace {}", dir.display()));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        f.turn();
        let first = f
            .client
            .handle()
            .exec_lua("return btv.g.first_session == true")
            .await;
        if first.is_ok_and(|v| v != Value::from(true)) {
            break;
        }
        assert!(Instant::now() < deadline, "the session switched");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `:e {dir}` asks for the open dialog in that directory, the editor resolving a
/// relative one against its own working directory; `:e {file}` opens the file.
#[tokio::test]
async fn a_directory_to_edit_opens_the_dialog_there() {
    let root = bemtvi_test_harness::temp_dir("client_dialog_dir");
    let root = std::fs::canonicalize(&root).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("file.txt"), "a file\n").unwrap();
    let mut f = start(None, Launch::default());
    f.framed().await;
    f.lua(&format!("btv.cmd({:?})", format!("cd {}", root.display())))
        .await;
    f.type_ahead(":e sub");
    let Effect::OpenDialog { start_dir, .. } = f.open_dialog().await else {
        unreachable!()
    };
    assert_eq!(start_dir, Some(root.join("sub")));
    // (Absolute: a relative file still resolves against the process's directory, which
    // the other tests' `:workspace` sessions move.)
    f.type_ahead(&format!(":e {}", root.join("file.txt").display()));
    let deadline = Instant::now() + TIMEOUT;
    while f.lua("return btv.buf.lines(0, 0, -1)[1]").await != Value::from("a file") {
        assert!(Instant::now() < deadline, "the file opened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !f.effects
            .iter()
            .any(|e| matches!(e, Effect::OpenDialog { .. })),
        "a file asks for no dialog: {:?}",
        f.effects
    );
}
