//! `btv.file_associations` and the GUI's one-time offer to register (Windows file
//! associations: Explorer's "Open with" and bemtvi's document icon).
//!
//! Hermetic: every test points the keys at a scratch `HKCU\Software\bemtvi-test-…`
//! key it deletes afterwards (`btv.file_associations._classes`), and keeps the answer
//! file in a temp dir (`_answer_path`), so the real `Software\Classes` and
//! `stdpath("state")` are never touched. The registry is read back with `reg query`,
//! the way any other program would see it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, lua_bool, poll_true, q, settle_ms, spawn, temp_root,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// A fresh temp dir for one test.
fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = temp_root().join(format!(
        "fileassoc-{tag}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Start a server with the offer armed (as `bemtvi-gui`'s launch does) or not, point
/// the keys and the answer file at this test's own places, then attach — the attach is
/// what shows the offer.
async fn start(
    offer: bool,
    classes: &str,
    answer: &Path,
    init: ServerInit,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = spawn(ServerInit {
        offer_file_associations: offer,
        ..init
    });
    exec_lua(
        &rpc,
        &format!(
            "btv.file_associations._classes = [[{classes}]]\n\
             btv.file_associations._answer_path = [[{}]]",
            answer.display()
        ),
    )
    .await;
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

async fn offer_showing(rpc: &Rpc) -> bool {
    lua_bool(rpc, "return vim.bo.filetype == 'btvfileassoc'").await == Some(true)
}

/// How a `btv.file_associations` call's promise settled.
struct Settled {
    ok: bool,
    /// The error's message when it rejected.
    message: String,
    /// The extension count: the result's, or (a partial failure) the error's.
    extensions: u64,
}

/// Run `btv.file_associations.<call>()` and wait for its promise to settle: the pass
/// runs off the server's Lua thread, so the call itself returns before it's done.
async fn settle_call(rpc: &Rpc, call: &str) -> Settled {
    exec_lua(
        rpc,
        &format!(
            "_G.__fa_out = nil\n\
             btv.file_associations.{call}():next(function(r)\n\
               _G.__fa_out = {{ ok = true, message = '', extensions = r.extensions }}\n\
             end, function(e)\n\
               _G.__fa_out = {{ ok = false, message = tostring(type(e) == 'table' and e.message or e),\n\
                 extensions = type(e) == 'table' and e.extensions or 0 }}\n\
             end)"
        ),
    )
    .await;
    assert!(
        poll_true(rpc, "return _G.__fa_out ~= nil").await,
        "{call}() never settled"
    );
    Settled {
        ok: lua_bool(rpc, "return _G.__fa_out.ok").await == Some(true),
        message: exec_lua(rpc, "return _G.__fa_out.message")
            .await
            .as_str()
            .unwrap_or_default()
            .to_string(),
        extensions: exec_lua(rpc, "return _G.__fa_out.extensions")
            .await
            .as_u64()
            .unwrap_or_default(),
    }
}

/// Poll until the answer file holds `want` (its whole text), or give up.
async fn answer_becomes(answer: &Path, want: &str) -> bool {
    for _ in 0..250 {
        if std::fs::read_to_string(answer).is_ok_and(|s| s == want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A `bemtvi-gui` session (the launch flag on) whose offer was already answered, so
/// none shows: for the tests that drive `register` / the commands directly.
async fn start_gui_answered(classes: &str, answer: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(answer, "declined\n\n").unwrap();
    start(true, classes, answer, ServerInit::default()).await
}

// Off Windows there is nothing to register with: the call fails loud, naming why,
// rather than pretending it worked.
#[cfg(not(windows))]
#[tokio::test]
async fn register_off_windows_fails_loud() {
    let dir = temp_dir("offwin");
    let (rpc, _incoming) =
        start_gui_answered(r"Software\bemtvi-test-unused", &dir.join("answer")).await;
    let r = settle_call(&rpc, "register").await;
    assert!(!r.ok, "register can't succeed off Windows");
    assert!(r.message.contains("Windows only"), "got {:?}", r.message);
}

// Without the GUI's launch flag (the TUI, an embedding app, every other suite) the
// offer never shows.
#[tokio::test]
async fn no_offer_unless_the_launch_asks() {
    let dir = temp_dir("unarmed");
    let answer = dir.join("answer");
    let (rpc, _incoming) = start(
        false,
        r"Software\bemtvi-test-unused",
        &answer,
        ServerInit::default(),
    )
    .await;
    settle_ms(&rpc, 300).await;
    assert!(
        !offer_showing(&rpc).await,
        "no offer without offer_file_associations"
    );
    assert!(!answer.exists(), "nothing recorded");
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::process::Command;

    /// A scratch `HKCU\Software\bemtvi-test-…\Classes`, deleted (whole) on drop.
    struct Scratch {
        top: String,
        classes: String,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let top = format!(r"Software\bemtvi-test-{tag}-{}", std::process::id());
            let _ = reg(&["delete", &format!(r"HKCU\{top}"), "/f"]);
            Scratch {
                classes: format!(r"{top}\Classes"),
                top,
            }
        }

        fn key(&self, sub: &str) -> String {
            format!(r"HKCU\{}\{sub}", self.classes)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = reg(&["delete", &format!(r"HKCU\{}", self.top), "/f"]);
        }
    }

    fn reg(args: &[&str]) -> std::process::Output {
        Command::new("reg")
            .args(args)
            .output()
            .expect("run reg.exe")
    }

    /// The `REG_SZ` text of `key`'s value `name` (`None`: the default value), or `None`
    /// when it isn't there. Parsed after the type column, since the `(Default)` label
    /// is localized.
    fn value(key: &str, name: Option<&str>) -> Option<String> {
        let out = match name {
            Some(n) => reg(&["query", key, "/v", n]),
            None => reg(&["query", key, "/ve"]),
        };
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.split_once("REG_SZ").map(|(_, v)| v.trim().to_string()))
    }

    /// Whether `key` has a value `name` (of any type).
    fn has_value(key: &str, name: &str) -> bool {
        reg(&["query", key, "/v", name]).status.success()
    }

    fn key_exists(key: &str) -> bool {
        reg(&["query", key]).status.success()
    }

    /// The program the server runs in — the one the keys must name.
    fn exe() -> String {
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn exe_name() -> String {
        std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    // Only `bemtvi-gui` registers itself: the TUI (`bemtvi.exe`) or an app embedding
    // the editor would point every type at a program without the document icon (and,
    // for the TUI, a console window). They fail loud, naming why, and write nothing.
    #[tokio::test]
    async fn registering_needs_the_gui() {
        let scratch = Scratch::new("notgui");
        let dir = temp_dir("notgui");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start(false, &scratch.classes, &answer, ServerInit::default()).await;
        for call in ["register", "unregister"] {
            let r = settle_call(&rpc, call).await;
            assert!(!r.ok, "{call} is refused");
            assert!(
                r.message.contains("bemtvi-gui"),
                "{call}: got {:?}",
                r.message
            );
        }
        feed(&rpc, ":RegisterFileTypes<CR>");
        settle_ms(&rpc, 300).await;
        assert!(!key_exists(&scratch.key("bemtvi.rs")), "nothing written");
        assert!(!answer.exists(), "nothing recorded");
    }

    #[tokio::test]
    async fn register_adds_open_with_entries_with_the_document_icon() {
        let scratch = Scratch::new("register");
        let dir = temp_dir("register");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start_gui_answered(&scratch.classes, &answer).await;

        let r = settle_call(&rpc, "register").await;
        assert!(r.ok, "register: {}", r.message);
        let n = r.extensions;
        assert!(n > 100, "the whole extension table, got {n}");

        let exe = exe();
        let icon = format!("\"{exe}\",-2");
        let command = format!("\"{exe}\" \"%1\"");
        // A recognized extension: listed under its OpenWithProgids, with a ProgID
        // naming its filetype, the document icon and the open command.
        assert!(has_value(&scratch.key(r".rs\OpenWithProgids"), "bemtvi.rs"));
        assert_eq!(
            value(&scratch.key("bemtvi.rs"), None).as_deref(),
            Some("Rust file")
        );
        // `.c` and `.C` share one key; it is named for what bemtvi types `.c` as.
        assert_eq!(
            value(&scratch.key("bemtvi.c"), None).as_deref(),
            Some("C file")
        );
        assert_eq!(
            value(&scratch.key(r"bemtvi.rs\DefaultIcon"), None).as_deref(),
            Some(icon.as_str())
        );
        assert_eq!(
            value(&scratch.key(r"bemtvi.rs\shell\open\command"), None).as_deref(),
            Some(command.as_str())
        );
        // Plain text, which no grammar claims, is in too.
        assert!(has_value(
            &scratch.key(r".txt\OpenWithProgids"),
            "bemtvi.txt"
        ));
        // Mixed-case table entries fold into one key (`Rnw` / `rnw`).
        assert!(has_value(
            &scratch.key(r".rnw\OpenWithProgids"),
            "bemtvi.rnw"
        ));
        // The exe itself: its name, the icon, and every text type's "Open with".
        let app = scratch.key(&format!(r"Applications\{}", exe_name()));
        assert_eq!(
            value(&app, Some("FriendlyAppName")).as_deref(),
            Some("bemtvi")
        );
        assert_eq!(
            value(&format!(r"{app}\DefaultIcon"), None).as_deref(),
            Some(icon.as_str())
        );
        assert!(key_exists(&scratch.key(&format!(
            r"SystemFileAssociations\text\OpenWithList\{}",
            exe_name()
        ))));

        let r = settle_call(&rpc, "unregister").await;
        assert!(r.ok, "unregister: {}", r.message);
        assert_eq!(r.extensions, n, "every type it registered");
        assert!(!has_value(
            &scratch.key(r".rs\OpenWithProgids"),
            "bemtvi.rs"
        ));
        assert!(!key_exists(&scratch.key("bemtvi.rs")));
        // The `.rs` key register created (the scratch had none) goes with it.
        assert!(!key_exists(&scratch.key(".rs")));
        assert!(!key_exists(&format!(r"{app}\DefaultIcon")));
        assert!(!key_exists(&scratch.key(&format!(
            r"SystemFileAssociations\text\OpenWithList\{}",
            exe_name()
        ))));
        // The exe's open command stays: it is what a choice made in "Open with" runs.
        assert_eq!(
            value(&format!(r"{app}\shell\open\command"), None).as_deref(),
            Some(command.as_str())
        );
    }

    /// Set `key`'s value (`None`: the default value) to the string `data`, creating it.
    fn set(key: &str, name: Option<&str>, data: &str) {
        let mut args = vec!["add", key];
        match name {
            Some(n) => args.extend(["/v", n]),
            None => args.push("/ve"),
        }
        args.extend(["/d", data, "/f"]);
        assert!(reg(&args).status.success(), "reg {args:?}");
    }

    // A type the user pointed at this exe before registering (Explorer's "Browse…"
    // makes an `md_auto_file` ProgID, and keeps choosing it for this exe afterwards)
    // has no icon of its own, so its files showed the app icon. Registering gives such
    // a ProgID the document icon too — only when it opens with this very exe and has
    // no icon of its own — and unregistering takes back only the icon it gave.
    #[tokio::test]
    async fn register_gives_the_icon_to_a_type_already_opening_with_this_exe() {
        let scratch = Scratch::new("auto");
        let dir = temp_dir("auto");
        let answer = dir.join("answer");
        let command = format!("\"{}\" \"%1\"", exe());
        let icon = format!("\"{}\",-2", exe());
        // `.md` resolves to a ProgID that opens with this exe, with no icon.
        set(&scratch.key(".md"), None, "md_auto_file");
        set(
            &scratch.key(r"md_auto_file\shell\open\command"),
            None,
            &command,
        );
        // Listed in `.py`'s OpenWithProgids, opening with this exe (unquoted path).
        set(&scratch.key(r".py\OpenWithProgids"), Some("py_mine"), "");
        set(
            &scratch.key(r"py_mine\shell\open\command"),
            None,
            &format!("{} %1", exe()),
        );
        // Another program's type: left alone.
        set(&scratch.key(".json"), None, "other.json");
        set(
            &scratch.key(r"other.json\shell\open\command"),
            None,
            r#""C:\elsewhere\code.exe" "%1""#,
        );
        // This exe, but with an icon of its own: kept.
        set(&scratch.key(".toml"), None, "toml_iconed");
        set(
            &scratch.key(r"toml_iconed\shell\open\command"),
            None,
            &command,
        );
        set(
            &scratch.key(r"toml_iconed\DefaultIcon"),
            None,
            r"C:\mine.ico",
        );

        let (rpc, _incoming) = start_gui_answered(&scratch.classes, &answer).await;
        let r = settle_call(&rpc, "register").await;
        assert!(r.ok, "register: {}", r.message);

        assert_eq!(
            value(&scratch.key(r"md_auto_file\DefaultIcon"), None).as_deref(),
            Some(icon.as_str())
        );
        assert_eq!(
            value(&scratch.key(r"py_mine\DefaultIcon"), None).as_deref(),
            Some(icon.as_str())
        );
        assert!(!key_exists(&scratch.key(r"other.json\DefaultIcon")));
        assert_eq!(
            value(&scratch.key(r"toml_iconed\DefaultIcon"), None).as_deref(),
            Some(r"C:\mine.ico")
        );

        let r = settle_call(&rpc, "unregister").await;
        assert!(r.ok, "unregister: {}", r.message);
        assert!(!key_exists(&scratch.key(r"md_auto_file\DefaultIcon")));
        assert!(!key_exists(&scratch.key(r"py_mine\DefaultIcon")));
        assert_eq!(
            value(&scratch.key(r"toml_iconed\DefaultIcon"), None).as_deref(),
            Some(r"C:\mine.ico")
        );
        // The ProgID itself is the user's choice and stays.
        assert_eq!(
            value(&scratch.key(r"md_auto_file\shell\open\command"), None).as_deref(),
            Some(command.as_str())
        );
    }

    // The commands do the same on demand, and record the answer so the offer never
    // shows after either.
    #[tokio::test]
    async fn commands_register_and_unregister_and_record_the_answer() {
        let scratch = Scratch::new("commands");
        let dir = temp_dir("commands");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start_gui_answered(&scratch.classes, &answer).await;

        feed(&rpc, ":RegisterFileTypes<CR>");
        assert!(answer_becomes(&answer, &format!("registered\n{}\n", exe())).await);
        assert!(has_value(&scratch.key(r".py\OpenWithProgids"), "bemtvi.py"));

        feed(&rpc, ":UnregisterFileTypes<CR>");
        assert!(answer_becomes(&answer, "declined\n\n").await);
        assert!(!has_value(
            &scratch.key(r".py\OpenWithProgids"),
            "bemtvi.py"
        ));
    }

    #[tokio::test]
    async fn the_first_attach_offers_and_enter_registers() {
        let scratch = Scratch::new("accept");
        let dir = temp_dir("accept");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;

        assert!(
            poll_true(&rpc, "return vim.bo.filetype == 'btvfileassoc'").await,
            "the GUI's first start offers to register"
        );
        assert!(
            !key_exists(&scratch.key("bemtvi.rs")),
            "nothing before the answer"
        );
        feed(&rpc, "<CR>");
        assert!(answer_becomes(&answer, &format!("registered\n{}\n", exe())).await);
        assert!(has_value(&scratch.key(r".rs\OpenWithProgids"), "bemtvi.rs"));
        assert!(!offer_showing(&rpc).await, "the offer closed");
    }

    #[tokio::test]
    async fn declining_is_remembered() {
        let scratch = Scratch::new("decline");
        let dir = temp_dir("decline");
        let answer = dir.join("answer");
        {
            let (rpc, _incoming) =
                start(true, &scratch.classes, &answer, ServerInit::default()).await;
            assert!(poll_true(&rpc, "return vim.bo.filetype == 'btvfileassoc'").await);
            feed(&rpc, "<Esc>");
            assert!(answer_becomes(&answer, "declined\n\n").await);
            assert!(
                !key_exists(&scratch.key("bemtvi.rs")),
                "declining registers nothing"
            );
        }
        // The next start has its answer: no offer.
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;
        settle_ms(&rpc, 300).await;
        assert!(
            !offer_showing(&rpc).await,
            "an answered offer never shows again"
        );
        assert!(!key_exists(&scratch.key("bemtvi.rs")));
    }

    // A registration whose program has moved (the zip unpacked somewhere new) is
    // renewed at startup, without asking again: the keys never name a missing exe.
    #[tokio::test]
    async fn a_moved_program_is_registered_again_at_startup() {
        let scratch = Scratch::new("moved");
        let dir = temp_dir("moved");
        let answer = dir.join("answer");
        std::fs::write(&answer, "registered\nC:\\old\\place\\bemtvi-gui.exe\n").unwrap();
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;

        assert!(answer_becomes(&answer, &format!("registered\n{}\n", exe())).await);
        let command = format!("\"{}\" \"%1\"", exe());
        assert_eq!(
            value(&scratch.key(r"bemtvi.rs\shell\open\command"), None).as_deref(),
            Some(command.as_str())
        );
        assert!(!offer_showing(&rpc).await, "renewing asks nothing");
    }

    // A registration naming another copy that is still there (the installed GUI, while a
    // dev build runs) is that copy's: starting this one leaves the keys alone.
    #[tokio::test]
    async fn another_copy_still_present_keeps_its_registration() {
        let scratch = Scratch::new("other-copy");
        let dir = temp_dir("other-copy");
        let answer = dir.join("answer");
        let other = dir.join("bemtvi-gui.exe");
        std::fs::write(&other, b"").unwrap();
        let saved = format!("registered\n{}\n", other.display());
        std::fs::write(&answer, &saved).unwrap();
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;

        settle_ms(&rpc, 300).await;
        assert_eq!(std::fs::read_to_string(&answer).unwrap(), saved);
        assert!(!key_exists(&scratch.key("bemtvi.rs")), "nothing re-pointed");
        assert!(
            !offer_showing(&rpc).await,
            "an answered offer never shows again"
        );
    }

    // Closing the offer some other way than its keys (`:q`) answers it "not now", like
    // `<Esc>`, rather than leaving it pending and offering again next start.
    #[tokio::test]
    async fn closing_the_offer_window_declines() {
        let scratch = Scratch::new("close");
        let dir = temp_dir("close");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;

        assert!(poll_true(&rpc, "return vim.bo.filetype == 'btvfileassoc'").await);
        feed(&rpc, ":q<CR>");
        assert!(answer_becomes(&answer, "declined\n\n").await);
        assert!(!key_exists(&scratch.key("bemtvi.rs")));
    }

    // On a fresh setup the plugin welcome shows first; the file-type offer waits until
    // it is answered rather than stacking its float on top.
    #[tokio::test]
    async fn the_offer_waits_for_the_plugin_welcome() {
        let scratch = Scratch::new("welcome");
        let dir = temp_dir("welcome");
        let answer = dir.join("answer");
        let cfg = dir.join("config");
        let root = dir.join("data");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("init.lua"),
            format!(
                "btv.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})\n",
                q(&root),
                q(&cfg)
            ),
        )
        .unwrap();
        let init = ServerInit {
            config_dir: Some(cfg.clone()),
            runtimepath: vec![cfg.clone()],
            offer_default_recommended: true,
            ..Default::default()
        };
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, init).await;

        assert!(poll_true(&rpc, "return vim.bo.filetype == 'btvpluginswelcome'").await);
        settle_ms(&rpc, 300).await;
        assert!(
            lua_bool(
                &rpc,
                "for _, b in ipairs(btv.buf.list()) do
                   if btv.bo[b].filetype == 'btvfileassoc' then return false end
                 end
                 return vim.bo.filetype == 'btvpluginswelcome'"
            )
            .await
                == Some(true),
            "while the plugin welcome is unanswered, the file-type offer isn't even mounted"
        );
        feed(&rpc, "<Esc>");
        assert!(
            poll_true(&rpc, "return vim.bo.filetype == 'btvfileassoc'").await,
            "the file-type offer follows the answered welcome"
        );
        // One key answers it: the offer mounts clear of the welcome's teardown, so the
        // first key pressed in it isn't lost.
        feed(&rpc, "<Esc>");
        assert!(answer_becomes(&answer, "declined\n\n").await);
    }

    // The pass makes thousands of registry calls, so it runs off the server's Lua thread:
    // `register()` hands back a pending promise at once, and the editor keeps answering
    // while the keys are written.
    #[tokio::test]
    async fn register_runs_off_the_editor_thread() {
        let scratch = Scratch::new("offtick");
        let dir = temp_dir("offtick");
        let answer = dir.join("answer");
        let (rpc, _incoming) = start_gui_answered(&scratch.classes, &answer).await;

        let state = exec_lua(
            &rpc,
            "_G.__fa_p = btv.file_associations.register()\n\
             return btv.promise.is_promise(_G.__fa_p) and _G.__fa_p._state or 'not a promise'",
        )
        .await;
        assert_eq!(
            state.as_str(),
            Some("pending"),
            "register() returns before the pass ends"
        );
        assert!(
            poll_true(&rpc, "return _G.__fa_p._state == 'fulfilled'").await,
            "the pass finishes"
        );
        assert!(has_value(&scratch.key(r".rs\OpenWithProgids"), "bemtvi.rs"));
    }

    /// Make `HKCU\<classes>\<sub>` a volatile key: Windows refuses to create a lasting
    /// key under one, so anything `register` writes beneath it fails.
    fn make_volatile(scratch: &Scratch, sub: &str) {
        // The classes key itself must be a lasting one (`CreateSubKey` would make every
        // missing parent volatile too), so only `sub` refuses what goes under it.
        let parent = format!(r"HKCU\{}", scratch.classes);
        assert!(
            reg(&["add", &parent, "/f"]).status.success(),
            "create {parent}"
        );
        let path = format!(r"{}\{sub}", scratch.classes);
        let script = format!(
            "[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('{path}', $true, \
             [Microsoft.Win32.RegistryOptions]::Volatile).Close()"
        );
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .expect("run powershell");
        assert!(out.status.success(), "make {path} volatile: {out:?}");
    }

    // One type whose keys can't be written doesn't stop the others: the pass carries on,
    // then fails loud, naming that type and saying how many did register.
    #[tokio::test]
    async fn an_unwritable_type_does_not_stop_the_rest() {
        let scratch = Scratch::new("partial");
        let dir = temp_dir("partial");
        let answer = dir.join("answer");
        make_volatile(&scratch, ".rs");
        let (rpc, _incoming) = start_gui_answered(&scratch.classes, &answer).await;

        let r = settle_call(&rpc, "register").await;
        assert!(!r.ok, "a type that couldn't be registered fails loud");
        assert!(
            r.message.contains(".rs:"),
            "names the type: {:?}",
            r.message
        );
        assert!(
            r.extensions > 100,
            "the rest registered, got {}",
            r.extensions
        );
        assert!(has_value(&scratch.key(r".py\OpenWithProgids"), "bemtvi.py"));
        assert!(has_value(
            &scratch.key(r".txt\OpenWithProgids"),
            "bemtvi.txt"
        ));
    }

    // Accepting the offer when a type can't be registered still records the answer (with
    // the exe the other types now name), so the offer doesn't come back every start.
    #[tokio::test]
    async fn a_partial_registration_still_answers_the_offer() {
        let scratch = Scratch::new("partial-offer");
        let dir = temp_dir("partial-offer");
        let answer = dir.join("answer");
        make_volatile(&scratch, ".rs");
        {
            let (rpc, _incoming) =
                start(true, &scratch.classes, &answer, ServerInit::default()).await;
            assert!(poll_true(&rpc, "return vim.bo.filetype == 'btvfileassoc'").await);
            feed(&rpc, "<CR>");
            assert!(answer_becomes(&answer, &format!("registered\n{}\n", exe())).await);
            assert!(has_value(&scratch.key(r".py\OpenWithProgids"), "bemtvi.py"));
        }
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, ServerInit::default()).await;
        settle_ms(&rpc, 300).await;
        assert!(
            !offer_showing(&rpc).await,
            "no offer after a partial registration"
        );
    }

    // The offer waits on the plugin welcome's promise, and waiting marks a rejection
    // handled: the welcome's own failure must still reach the user, not vanish into
    // the waiter.
    #[tokio::test]
    async fn a_failed_plugin_welcome_is_still_reported() {
        let scratch = Scratch::new("welcomefail");
        let dir = temp_dir("welcomefail");
        let answer = dir.join("answer");
        let cfg = dir.join("config");
        let root = dir.join("data");
        // The manager's config "dir" is a file, so writing the chosen set fails.
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, "").unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("init.lua"),
            format!(
                "btv.plugins.setup_manager({{ root = \"{}\", config = \"{}\" }})\n",
                q(&root),
                q(&not_a_dir)
            ),
        )
        .unwrap();
        let init = ServerInit {
            config_dir: Some(cfg.clone()),
            runtimepath: vec![cfg.clone()],
            offer_default_recommended: true,
            ..Default::default()
        };
        let (rpc, _incoming) = start(true, &scratch.classes, &answer, init).await;
        exec_lua(
            &rpc,
            r#"btv._test_notes = {}
               local notify = btv.notify
               btv.notify = function(msg, ...)
                 table.insert(btv._test_notes, tostring(msg))
                 return notify(msg, ...)
               end"#,
        )
        .await;

        assert!(poll_true(&rpc, "return vim.bo.filetype == 'btvpluginswelcome'").await);
        feed(&rpc, "<CR>");
        assert!(
            poll_true(
                &rpc,
                r#"for _, m in ipairs(btv._test_notes) do
                     if m:find("first-run welcome failed", 1, true) then return true end
                   end
                   return false"#
            )
            .await,
            "the welcome's failure is reported; notes: {:?}",
            exec_lua(&rpc, "return table.concat(btv._test_notes, ' | ')").await
        );
    }
}
