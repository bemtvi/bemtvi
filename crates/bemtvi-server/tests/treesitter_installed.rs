//! `btv.treesitter.installed` and `btv.filetype.match`, what a plugin asks before it asks
//! for a grammar. A file of its own: it points the data dir at a fixture, for the process.

use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;

#[tokio::test]
async fn installed_lists_what_can_highlight() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("ts_installed");
    std::fs::create_dir_all(dir.join("parser")).unwrap();
    for lang in ["fixturelang", "fixturebare", "fixturertp", "fixtureafter"] {
        std::fs::write(dir.join("parser").join(format!("{lang}.so")), "").unwrap();
    }
    // The engine refuses this name, however complete the grammar looks.
    std::fs::write(dir.join("parser/fixture.old.so"), "").unwrap();
    std::fs::create_dir_all(dir.join("queries/fixture.old")).unwrap();
    std::fs::write(dir.join("queries/fixture.old/highlights.scm"), "").unwrap();
    // One has a query beside its parser and two get theirs from the runtimepath.
    // `fixturebare` has none, and `fixtureorphan` has a query but no parser.
    std::fs::create_dir_all(dir.join("queries/fixturelang")).unwrap();
    std::fs::write(dir.join("queries/fixturelang/highlights.scm"), "").unwrap();
    let rtp = temp_dir("ts_installed_rtp");
    for sub in [
        "queries/fixturertp",
        "after/queries/fixtureafter",
        "queries/fixtureorphan",
    ] {
        std::fs::create_dir_all(rtp.join(sub)).unwrap();
        std::fs::write(rtp.join(sub).join("highlights.scm"), "").unwrap();
    }
    // SAFETY: both tests hold `serial_lock`, and no server is running yet, so no other
    // thread is reading the environment.
    unsafe { std::env::set_var("BEMTVI_DATA_DIR", &dir) };

    let (rpc, _incoming) = start_attached(
        ServerInit {
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    exec_lua(
        &rpc,
        "btv.treesitter.installed():next(function(langs) btv.g.ts_langs = langs end)",
    )
    .await;
    assert!(
        poll_true(&rpc, "return btv.g.ts_langs ~= nil").await,
        "the promise settled"
    );
    let langs = exec_lua(&rpc, "return btv.g.ts_langs").await;
    let langs: Vec<&str> = langs
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(langs, ["fixtureafter", "fixturelang", "fixturertp"]);
}

#[tokio::test]
async fn match_reads_only_the_name() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    for (path, want) in [
        ("src/app.rs", Some("rust")),
        ("no/such/dir/Makefile", Some("make")),
        ("a/.bashrc", Some("bash")),
        ("a/.env.local", Some("bash")),
        ("notes.nonesuch", None),
    ] {
        let got = exec_lua(&rpc, &format!("return btv.filetype.match({path:?})")).await;
        assert_eq!(got.as_str(), want, "{path}");
    }
    // A name on disk needn't be UTF-8, and the extension still says what it is.
    let got = exec_lua(&rpc, r#"return btv.filetype.match("caf\233.rs")"#).await;
    assert_eq!(got.as_str(), Some("rust"));
    let bad = rpc
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("return btv.filetype.match(1)"),
                Value::Array(vec![]),
            ],
        )
        .await;
    assert!(bad.is_err(), "a path that isn't a string raises");
}

/// Point the data dir at `dir`. Hold `serial_lock`, with no server running.
fn use_data_dir(dir: &std::path::Path) {
    // SAFETY: the caller holds `serial_lock`, so no other test's thread reads the
    // environment while this writes it.
    unsafe { std::env::set_var("BEMTVI_DATA_DIR", dir) };
}

/// A data dir with a parser and a highlights query for each of `langs`.
fn grammar_fixture(tag: &str, langs: &[String]) -> std::path::PathBuf {
    let dir = temp_dir(tag);
    std::fs::create_dir_all(dir.join("parser")).unwrap();
    for lang in langs {
        std::fs::write(dir.join("parser").join(format!("{lang}.so")), "").unwrap();
        let q = dir.join("queries").join(lang);
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(q.join("highlights.scm"), "").unwrap();
    }
    dir
}

/// A scan on the tick stalls every request behind it for as long as the scan takes, so the
/// editor must keep answering well inside that. Timing, so a few tries: a loaded machine
/// can make a good run look stalled, but a scan on the tick can't ever look free.
#[tokio::test]
async fn installed_keeps_the_editor_responsive() {
    let _guard = serial_lock().lock().await;
    let langs: Vec<String> = (0..8000).map(|i| format!("lang{i}")).collect();
    use_data_dir(&grammar_fixture("ts_installed_big", &langs));
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;

    let started = std::time::Instant::now();
    let found = bemtvi_ts::highlightable_langs(&[], &Default::default()).unwrap();
    let scan = started.elapsed();
    assert_eq!(found.len(), langs.len());

    let mut slowest = Vec::new();
    for _ in 0..10 {
        let ask = std::time::Instant::now();
        exec_lua(
            &rpc,
            "_G.ts_done = false; \
             btv.treesitter.installed():next(function() _G.ts_done = true end)",
        )
        .await;
        let mut worst = ask.elapsed();
        loop {
            let ping = std::time::Instant::now();
            let done = lua_bool(&rpc, "return _G.ts_done").await == Some(true);
            worst = worst.max(ping.elapsed());
            if done {
                break;
            }
        }
        slowest.push(worst);
        if worst < scan / 2 {
            return;
        }
    }
    panic!("a scan takes {scan:?}, and requests stalled for {slowest:?}");
}

/// A `parser` that can't be listed isn't "nothing installed": a plugin would reinstall.
#[tokio::test]
async fn an_unreadable_grammar_dir_rejects_and_says_so() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("ts_installed_unreadable");
    std::fs::write(dir.join("parser"), "not a directory").unwrap();
    use_data_dir(&dir);
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;
    exec_lua(
        &rpc,
        "btv.treesitter.installed():next( \
           function() btv.g.ts_out = 'resolved' end, \
           function(e) btv.g.ts_out = tostring(e) end)",
    )
    .await;
    assert!(poll_true(&rpc, "return btv.g.ts_out ~= nil").await);
    let got = exec_lua(&rpc, "return btv.g.ts_out").await;
    let got = got.as_str().unwrap();
    assert!(got.contains("parser"), "names the dir: {got}");

    feed(&rpc, ":TSInstallInfo<CR>");
    let map = wait_redraw(&mut incoming, |m| message(m).contains("TSInstallInfo")).await;
    assert!(message(&map).contains("parser"), "{}", message(&map));
}

#[tokio::test]
async fn install_info_lists_the_scan() {
    let _guard = serial_lock().lock().await;
    use_data_dir(&grammar_fixture(
        "ts_installed_info",
        &["fixturelang".to_string()],
    ));
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    feed(&rpc, ":TSInstallInfo<CR>");
    assert!(
        poll_true(
            &rpc,
            "return table.concat(btv.buf.lines(0, 0, -1), '\\n'):find('fixturelang') ~= nil"
        )
        .await,
        "the listing opened"
    );
}

/// A daemon's language is installed on first sight only if this side can't paint it yet.
#[tokio::test]
async fn remote_autoinstall_skips_what_is_here() {
    let _guard = serial_lock().lock().await;
    use_data_dir(&grammar_fixture(
        "ts_installed_auto",
        &["fixturelang".to_string()],
    ));
    let (rpc, _incoming) = start_attached(
        ServerInit {
            ts_autoinstall: vec!["fixturelang".to_string(), "fixturemissing".to_string()],
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    exec_lua(
        &rpc,
        "_G.ts_cmds = {}; vim.cmd = function(c) table.insert(_G.ts_cmds, c) end",
    )
    .await;
    feed(&rpc, ":setf fixturelang<CR>:setf fixturemissing<CR>");
    assert!(
        poll_true(&rpc, "return #_G.ts_cmds > 0").await,
        "the missing one installs"
    );
    let cmds = exec_lua(&rpc, "return table.concat(_G.ts_cmds, ',')").await;
    assert_eq!(cmds.as_str(), Some("TSInstall fixturemissing"));
}

#[tokio::test]
async fn set_query_counts_as_highlightable_and_checks_its_args() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("ts_installed_set");
    std::fs::create_dir_all(dir.join("parser")).unwrap();
    std::fs::write(dir.join("parser/fixtureset.so"), "").unwrap();
    use_data_dir(&dir);
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    exec_lua(
        &rpc,
        "btv.treesitter.set_query('fixtureset', 'highlights', '(x) @y'); \
         btv.treesitter.installed():next(function(l) btv.g.ts_langs = l end)",
    )
    .await;
    assert!(poll_true(&rpc, "return btv.g.ts_langs ~= nil").await);
    let listed = exec_lua(
        &rpc,
        "return btv.tbl.contains(btv.g.ts_langs, 'fixtureset')",
    )
    .await;
    assert_eq!(listed.as_bool(), Some(true));

    // Clearing takes it back off.
    exec_lua(
        &rpc,
        "btv.g.ts_langs = nil; btv.treesitter.set_query('fixtureset', 'highlights', nil); \
         btv.treesitter.installed():next(function(l) btv.g.ts_langs = l end)",
    )
    .await;
    assert!(poll_true(&rpc, "return btv.g.ts_langs ~= nil").await);
    let listed = exec_lua(
        &rpc,
        "return btv.tbl.contains(btv.g.ts_langs, 'fixtureset')",
    )
    .await;
    assert_eq!(listed.as_bool(), Some(false));

    // A name the engine has no use for would otherwise be dropped without a word.
    let bad = rpc
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("btv.treesitter.set_query('x', 'nonesuch', '')"),
                Value::Array(vec![]),
            ],
        )
        .await;
    assert!(bad.is_err());
}

/// A promise of `btv.treesitter.highlight` goes on with nothing else going on. Its answer
/// comes off a grammar load, after the request that asked: no key or scroll follows to run
/// what it queued, and a plugin's colours used to wait for one. A chain of two, as what
/// comes of the first asks for the second.
#[tokio::test]
async fn highlight_goes_on_with_no_input() {
    let _guard = serial_lock().lock().await;
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;
    exec_lua(
        &rpc,
        r#"btv.buf.set_lines(0, 0, -1, true, { "painted" })
           btv.hl.define(0, "FromHighlight", { link = "Keyword" })
           local ns = btv.ns.create("no-input")
           btv.async(function()
             btv.await(btv.treesitter.highlight("sourcegitnone", "1"))
             btv.await(btv.treesitter.highlight("sourcegitnonetoo", "2"))
             btv.buf.set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 7, hl_group = "FromHighlight" })
           end)()"#,
    )
    .await;
    // From here on, only frames: a request would itself tick the editor.
    let map = wait_redraw(&mut incoming, |m| paints(m, "FromHighlight")).await;
    assert!(paints(&map, "FromHighlight"));
}

// ===== Real grammars: network + a C compiler, opt-in like the other ts e2e tests =====

/// Whether any painted span has capture `group`.
fn paints(map: &[(Value, Value)], group: &str) -> bool {
    window0_field(map, "highlights")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter().filter_map(Value::as_array).any(|cols| {
                cols.iter()
                    .filter_map(Value::as_array)
                    .any(|span| span.get(2).and_then(Value::as_str) == Some(group))
            })
        })
}

/// A data dir with the real json parser and no `highlights.scm` of its own.
fn json_without_highlights(tag: &str) -> std::path::PathBuf {
    let data = temp_dir(tag);
    bemtvi_ts::install::install(&data, "json")
        .expect("install json grammar (network + C compiler required)");
    std::fs::remove_file(data.join("queries/json/highlights.scm")).unwrap();
    data
}

#[tokio::test]
#[ignore = "needs network + a C compiler"]
async fn a_listed_runtimepath_query_really_paints() {
    let _guard = serial_lock().lock().await;
    use_data_dir(&json_without_highlights("ts_paint_rtp"));
    let rtp = temp_dir("ts_paint_rtp_dir");
    std::fs::create_dir_all(rtp.join("queries/json")).unwrap();
    std::fs::write(
        rtp.join("queries/json/highlights.scm"),
        "(number) @from.rtp\n",
    )
    .unwrap();
    let file = write_temp("ts_paint_rtp", "json", "{\"a\": 123}\n");
    let (rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    exec_lua(
        &rpc,
        "btv.treesitter.installed():next(function(l) btv.g.ts_langs = l end)",
    )
    .await;
    assert!(poll_true(&rpc, "return btv.g.ts_langs ~= nil").await);
    let listed = exec_lua(&rpc, "return btv.tbl.contains(btv.g.ts_langs, 'json')").await;
    assert_eq!(listed.as_bool(), Some(true));
    let map = wait_redraw(&mut incoming, |m| paints(m, "from.rtp")).await;
    assert!(paints(&map, "from.rtp"));
}

/// What a plugin paints from `btv.treesitter.highlight` as an install ends reaches the
/// screen with nothing else going on: no key or scroll comes after it to bring the frame.
/// Two parses in a row, as a diff's two versions need, the first waiting on the grammar.
#[tokio::test]
#[ignore = "needs network + a C compiler"]
async fn colours_from_an_install_paint_with_no_input() {
    let _guard = serial_lock().lock().await;
    // Installed once beforehand, for the compiler: the wait below is for the frame.
    let data = temp_dir("ts_paint_install");
    bemtvi_ts::install::install(&data, "json")
        .expect("install json grammar (network + C compiler required)");
    use_data_dir(&data);
    let file = write_temp("ts_paint_install", "txt", "{\"a\": 123}\n");
    let (rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    exec_lua(
        &rpc,
        r#"local ns = btv.ns.create("paint")
           btv.hl.define(0, "FromInstall", { link = "Keyword" })
           btv.on("TSInstalled", function()
             btv.async(function()
               btv.await(btv.treesitter.highlight("json", "1"))
               btv.await(btv.treesitter.highlight("json", "2"))
               btv.buf.set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 3, hl_group = "FromInstall" })
             end)()
           end)
           btv.cmd("TSInstall json")"#,
    )
    .await;
    // From here on, only frames: a request would itself tick the editor.
    let map = wait_redraw(&mut incoming, |m| paints(m, "FromInstall")).await;
    assert!(paints(&map, "FromInstall"));
}

/// An editor that looked for a grammar and found none remembers. Another editor installing
/// it can't tell this one, so `btv.treesitter.reload` has it look again.
#[tokio::test]
#[ignore = "needs network + a C compiler"]
async fn reload_finds_a_grammar_another_editor_installed() {
    let _guard = serial_lock().lock().await;
    let data = temp_dir("ts_reload");
    use_data_dir(&data);
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    let ask = r#"btv.g.spans = nil
        btv.treesitter.highlight("json", "[1]"):next(function(s) btv.g.spans = #s end)"#;
    exec_lua(&rpc, ask).await;
    assert!(poll_true(&rpc, "return btv.g.spans == 0").await, "none yet");

    bemtvi_ts::install::install(&data, "json")
        .expect("install json grammar (network + C compiler required)");
    exec_lua(&rpc, ask).await;
    assert!(
        poll_true(&rpc, "return btv.g.spans == 0").await,
        "it still remembers"
    );

    exec_lua(&rpc, "btv.treesitter.reload('json')").await;
    exec_lua(&rpc, ask).await;
    assert!(
        poll_true(&rpc, "return (btv.g.spans or 0) > 0").await,
        "it found the grammar"
    );
}

/// The grammar failed to load for want of a highlights query; handing it one is the cure,
/// so the verdict can't stay cached.
#[tokio::test]
#[ignore = "needs network + a C compiler"]
async fn set_query_revives_a_grammar_that_failed_to_load() {
    let _guard = serial_lock().lock().await;
    use_data_dir(&json_without_highlights("ts_paint_revive"));
    let file = write_temp("ts_paint_revive", "json", "{\"a\": 123}\n");
    let (rpc, mut incoming) = start_attached(
        ServerInit {
            file: Some(file),
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    wait_redraw(&mut incoming, |m| message(m).contains("highlights")).await;
    exec_lua(
        &rpc,
        "btv.treesitter.set_query('json', 'highlights', '(number) @from.set')",
    )
    .await;
    feed(&rpc, "j");
    let map = wait_redraw(&mut incoming, |m| paints(m, "from.set")).await;
    assert!(paints(&map, "from.set"));
}

/// An explicit `set_query` is the last word over a query file on the runtimepath, until
/// it's cleared.
#[tokio::test]
#[ignore = "needs network + a C compiler"]
async fn set_query_beats_the_runtimepath_until_cleared() {
    let _guard = serial_lock().lock().await;
    use_data_dir(&json_without_highlights("ts_paint_wins"));
    let rtp = temp_dir("ts_paint_wins_dir");
    std::fs::create_dir_all(rtp.join("queries/json")).unwrap();
    std::fs::write(
        rtp.join("queries/json/highlights.scm"),
        "(number) @from.rtp\n",
    )
    .unwrap();
    let (rpc, mut incoming) = start_attached(
        ServerInit {
            runtimepath: vec![rtp],
            ..Default::default()
        },
        80,
        24,
    )
    .await;
    exec_lua(
        &rpc,
        "btv.treesitter.set_query('json', 'highlights', '(number) @from.set')",
    )
    .await;
    let file = write_temp("ts_paint_wins", "json", "{\"a\": 123}\n");
    feed(&rpc, &format!(":e {file}<CR>"));
    let map = wait_redraw(&mut incoming, |m| paints(m, "from.set")).await;
    assert!(!paints(&map, "from.rtp"));

    exec_lua(&rpc, "btv.treesitter.set_query('json', 'highlights', nil)").await;
    feed(&rpc, "j");
    let map = wait_redraw(&mut incoming, |m| paints(m, "from.rtp")).await;
    assert!(paints(&map, "from.rtp"));
}

/// `TSInstalled` says how an install went, with its language as the pattern: what's
/// painted from a grammar off-buffer has no other way to hear of one. A mirror with
/// nothing in it fails the fetch, without the network.
#[tokio::test]
async fn an_install_says_how_it_went() {
    // SAFETY: nothing else in this binary installs.
    unsafe { std::env::set_var("BEMTVI_TS_MIRROR", temp_dir("ts_installed_mirror")) };
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    exec_lua(
        &rpc,
        r#"btv.on("TSInstalled", function(args) btv.g.ts_event = { args.match, args.data.lang, args.data.ok, args.data.error } end)"#,
    )
    .await;
    command(&rpc, "TSInstall sourcegitnonesuch").await;
    assert!(
        poll_true(&rpc, "return btv.g.ts_event ~= nil").await,
        "the event fired"
    );
    let event = exec_lua(&rpc, "return btv.g.ts_event").await;
    let event = event.as_array().expect("a list");
    assert_eq!(event[0].as_str(), Some("sourcegitnonesuch"), "the pattern");
    assert_eq!(event[1].as_str(), Some("sourcegitnonesuch"), "data.lang");
    assert_eq!(event[2].as_bool(), Some(false), "data.ok");
    assert!(!event[3].as_str().expect("data.error").is_empty());
}
