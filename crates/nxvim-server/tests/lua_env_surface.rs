//! Behavior tests for the small `nx.*` surfaces the nvim-lspconfig port needed and
//! nxvim did not have: `nx.cwd` / `nx.stdpath` / `nx.pid` / `nx.version` (editor and
//! host facts a config must be able to ask for without reaching into `vim.fn`, which
//! ADR 0002 keeps off the alias whitelist), and the two JSON values Lua cannot express
//! on its own — `nx.json.null` and `nx.json.empty_object()`.
//!
//! The JSON pair is the load-bearing one: an LSP `init_options` that means `{}` or
//! `null` and encodes as `[]` or a missing key is a message the server reads
//! differently, and the failure looks like "the server started but does nothing"
//! rather than an error. See docs/plans/2026-07-29-nvim-lspconfig-native-port.md.
//!
//! Black-box per the project conventions: a real server over RPC, driven with
//! `nvim_exec_lua`.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn lua_str(rpc: &Rpc, code: &str) -> String {
    exec_lua(rpc, code)
        .await
        .as_str()
        .map(str::to_owned)
        .unwrap_or_default()
}

#[tokio::test]
async fn cwd_is_the_editors_working_directory_and_follows_cd() {
    let (rpc, _incoming) = start().await;

    let cwd = lua_str(&rpc, "return nx.cwd()").await;
    assert!(
        cwd.starts_with('/'),
        "nx.cwd() must be absolute, got {cwd:?}"
    );
    assert!(
        !cwd.ends_with('/') || cwd == "/",
        "nx.cwd() must not carry a trailing separator, got {cwd:?}"
    );

    // It reads the editor's effective directory rather than a value frozen at startup:
    // a config resolving a fallback root after a `:cd` has to see the new one.
    exec_lua(&rpc, "nx.cmd('cd /')").await;
    let after = lua_str(&rpc, "return nx.cwd()").await;
    assert_eq!(after, "/", "nx.cwd() must follow `:cd`");
}

#[tokio::test]
async fn stdpath_names_the_xdg_directories() {
    let (rpc, _incoming) = start().await;

    for what in ["config", "data", "cache", "state"] {
        let dir = lua_str(&rpc, &format!("return nx.stdpath('{what}')")).await;
        assert!(
            dir.starts_with('/'),
            "nx.stdpath({what:?}) must be an absolute path, got {dir:?}"
        );
        assert!(
            !dir.ends_with('/'),
            "nx.stdpath({what:?}) must not carry a trailing separator, got {dir:?}"
        );
    }

    // The point of asking rather than composing $HOME by hand: the answers differ, so a
    // plugin's cache does not land in the user's config.
    let data = lua_str(&rpc, "return nx.stdpath('data')").await;
    let cache = lua_str(&rpc, "return nx.stdpath('cache')").await;
    assert_ne!(data, cache, "data and cache must be distinct directories");
}

#[tokio::test]
async fn pid_is_this_process_and_version_names_nxvim() {
    let (rpc, _incoming) = start().await;

    let pid = exec_lua(&rpc, "return nx.pid()").await.as_i64().unwrap();
    assert_eq!(
        pid,
        i64::from(std::process::id()),
        "nx.pid() must be the editor process a server would watch, not some other id"
    );

    // Servers carry this into `clientInfo` / vendor telemetry; it must say what the
    // editor actually is.
    let version = lua_str(&rpc, "return nx.version()").await;
    assert!(
        version.starts_with("nxvim "),
        "nx.version() must name nxvim, got {version:?}"
    );
}

#[tokio::test]
async fn json_null_encodes_as_null_where_a_nil_would_drop_the_key() {
    let (rpc, _incoming) = start().await;

    // The distinction the sentinel exists for: `nil` removes the key entirely, which a
    // protocol peer reads as "unset, use your default" rather than "explicitly nothing"
    // — and here it leaves nothing behind at all, so the table encodes as an empty
    // array rather than as an object with a null in it.
    let dropped = lua_str(&rpc, "return nx.json.encode({ token = nil })").await;
    assert_eq!(dropped, "[]", "a nil value leaves no key to encode");

    let explicit = lua_str(&rpc, "return nx.json.encode({ token = nx.json.null })").await;
    assert_eq!(explicit, r#"{"token":null}"#);

    // One shared sentinel, so a caller can identify it on the way back out.
    let same = exec_lua(&rpc, "return nx.json.null == nx.json.null").await;
    assert_eq!(same.as_bool(), Some(true));

    // Nested, and alongside real values.
    let nested = lua_str(
        &rpc,
        "return nx.json.encode({ a = 1, b = { c = nx.json.null } })",
    )
    .await;
    assert!(
        nested.contains(r#""c":null"#),
        "nx.json.null must survive nesting, got {nested}"
    );
}

#[tokio::test]
async fn empty_object_encodes_as_an_object_where_a_bare_table_is_an_array() {
    let (rpc, _incoming) = start().await;

    // An empty Lua table is both an empty array and an empty object; the codec has to
    // pick one, and picks `[]`.
    let bare = lua_str(&rpc, "return nx.json.encode({ models = {} })").await;
    assert_eq!(bare, r#"{"models":[]}"#);

    let marked = lua_str(
        &rpc,
        "return nx.json.encode({ models = nx.json.empty_object() })",
    )
    .await;
    assert_eq!(marked, r#"{"models":{}}"#);

    // A fresh table each call, so it can be filled in afterwards — and a table with
    // entries encodes as an object regardless.
    let distinct = exec_lua(
        &rpc,
        "return nx.json.empty_object() ~= nx.json.empty_object()",
    )
    .await;
    assert_eq!(distinct.as_bool(), Some(true));

    let filled = lua_str(
        &rpc,
        r#"local t = nx.json.empty_object()
           t.a = 1
           return nx.json.encode({ m = t })"#,
    )
    .await;
    assert_eq!(filled, r#"{"m":{"a":1}}"#);
}

#[tokio::test]
async fn the_json_sentinels_reach_an_lsp_config_unchanged() {
    let (rpc, _incoming) = start().await;

    // The reason both exist: they are written into an LSP config's `init_options`, and
    // what matters is the JSON that crosses at `initialize`. `nx.lsp.get_config` is the
    // same resolved table the dispatcher hands the spawn, so encoding it is the honest
    // end-to-end check short of a live server.
    exec_lua(
        &rpc,
        r#"nx.lsp.config("sentinel_probe", {
             init_options = {
               token = nx.json.null,
               memory = { file_store = nx.json.empty_object() },
             },
           })"#,
    )
    .await;

    let encoded = lua_str(
        &rpc,
        "return nx.json.encode(nx.lsp.get_config('sentinel_probe').init_options)",
    )
    .await;
    assert!(
        encoded.contains(r#""token":null"#),
        "the null must survive the config merge, got {encoded}"
    );
    assert!(
        encoded.contains(r#""file_store":{}"#),
        "the empty object must survive the config merge, got {encoded}"
    );
}

#[tokio::test]
async fn a_sentinel_overrides_the_value_a_preset_already_put_there() {
    let (rpc, _incoming) = start().await;

    // The shape the sentinels actually ship in: an `lsp/<name>.lua` preset (or an
    // earlier `nx.lsp.config` call) already fills the key, and the user's config
    // overrides it with "explicitly nothing" / "explicitly an empty object". That runs
    // through `nx.tbl.deep_extend`, whose map-merge rule treats an EMPTY table as a
    // mergeable map — so a sentinel merged onto a populated table contributed no keys
    // and vanished, leaving the preset's value on the wire. Silently: the server starts
    // and answers with a configuration nobody asked for, which is the exact failure the
    // sentinels exist to prevent.
    exec_lua(
        &rpc,
        r#"nx.lsp.config("preset_probe", {
             init_options = {
               auth = { token = "from-the-preset" },
               memory = { file_store = { size = 10 } },
             },
           })
           nx.lsp.config("preset_probe", {
             init_options = {
               auth = nx.json.null,
               memory = { file_store = nx.json.empty_object() },
             },
           })"#,
    )
    .await;

    let encoded = lua_str(
        &rpc,
        "return nx.json.encode(nx.lsp.get_config('preset_probe').init_options)",
    )
    .await;
    assert!(
        encoded.contains(r#""auth":null"#),
        "the null must REPLACE the preset's table, not merge into it, got {encoded}"
    );
    assert!(
        encoded.contains(r#""file_store":{}"#),
        "the empty object must REPLACE the preset's map, got {encoded}"
    );

    // And the merge is not one-way: a real value written over a sentinel wins too, so a
    // preset that nulls a key stays overridable.
    exec_lua(
        &rpc,
        r#"nx.lsp.config("unnull_probe", { init_options = { auth = nx.json.null } })
           nx.lsp.config("unnull_probe", { init_options = { auth = { token = "mine" } } })"#,
    )
    .await;
    let back = lua_str(
        &rpc,
        "return nx.json.encode(nx.lsp.get_config('unnull_probe').init_options)",
    )
    .await;
    assert_eq!(back, r#"{"auth":{"token":"mine"}}"#);
}

#[tokio::test]
async fn is_null_identifies_a_sentinel_that_has_been_copied() {
    let (rpc, _incoming) = start().await;

    // `nx.json.null` is one shared table, but every path that *stores* it copies it —
    // `nx.tbl.deepcopy`, and so `nx.tbl.deep_extend`, which is what `nx.lsp.config`
    // merges with. The copy carries the mark but is a different table, so `==` against
    // the sentinel answers false exactly where a caller would ask: on the way back out
    // of a config. `nx.json.is_null` reads the mark instead.
    let copied_eq = exec_lua(
        &rpc,
        "return nx.tbl.deepcopy({ t = nx.json.null }).t == nx.json.null",
    )
    .await;
    assert_eq!(
        copied_eq.as_bool(),
        Some(false),
        "a copy is a different table — this is why identity is not the test"
    );

    for (expr, want) in [
        ("nx.json.null", true),
        ("nx.tbl.deepcopy({ t = nx.json.null }).t", true),
        ("nx.json.empty_object()", false),
        ("{}", false),
        ("nil", false),
        ("false", false),
        ("\"null\"", false),
    ] {
        let got = exec_lua(&rpc, &format!("return nx.json.is_null({expr})")).await;
        assert_eq!(
            got.as_bool(),
            Some(want),
            "nx.json.is_null({expr}) must be {want}"
        );
    }

    // The round trip a config does: write the sentinel, read it back off the resolved
    // config, recognize it.
    exec_lua(
        &rpc,
        r#"nx.lsp.config("is_null_probe", { init_options = { token = nx.json.null } })"#,
    )
    .await;
    let recognized = exec_lua(
        &rpc,
        "return nx.json.is_null(nx.lsp.get_config('is_null_probe').init_options.token)",
    )
    .await;
    assert_eq!(recognized.as_bool(), Some(true));
}
