//! Shared harness for nxvim's black-box integration tests.
//!
//! Every test in the workspace drives the editor the same way a real UI client
//! does: it starts a real [`nxvim_server`] on its own OS thread, connects over
//! the same msgpack-RPC a front end speaks (an in-process [`tokio::io::duplex`]
//! pipe), feeds vim key-notation through `nvim_input`, and asserts on observable
//! results — buffer contents, the cursor, or the `redraw` notifications the
//! server projects for clients to paint. Nothing reaches into the editor's
//! internals.
//!
//! That harness used to be copy-pasted into every `tests/*.rs` file. This crate
//! is the single home for the parts that don't vary: spawning the server,
//! the request/notify conveniences ([`feed`], [`lines`], [`cursor`], …), and the
//! two families of `redraw` accessors.
//!
//! ## Two redraw conventions
//!
//! A `redraw` notification carries one argument: a msgpack map. Tests reach into
//! it two ways, and both live here so neither file has to re-derive them:
//!
//! - **params convention** — functions take the raw notification params
//!   (`&[Value]`) and index `params[0]` themselves: [`drain_latest_redraw`],
//!   [`redraw_get`], [`window0_get`], [`window0`], [`message_of`].
//! - **map convention** — functions take the already-extracted map
//!   (`&[(Value, Value)]`): [`field`], [`map_get`], [`window0_field`], [`u64_at`],
//!   [`message`], plus the predicate-driven [`drain_to_latest_redraw`].
//!
//! Per-window fields (lines, cursor, highlights, diagnostics, scroll, …) live
//! under `windows[0]`; the `field`/`window0_*` helpers know to look there.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

// ===== server lifecycle ======================================================

/// Spawn a server on its own OS thread, wired to a connected client over an
/// in-process duplex pipe. Does **not** attach a UI — call [`attach`] (or use
/// [`start_attached`]) when the test asserts on `redraw`s.
///
/// Must be called from within a tokio runtime (i.e. inside a `#[tokio::test]`):
/// [`connect`] spawns its reader/writer tasks on the caller's runtime. The
/// server's own thread builds a fresh current-thread runtime with IO and time
/// enabled, so it can host subprocess workers (LSP, grammar loads) and timers.
pub fn spawn(init: ServerInit) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    connect(reader, writer)
}

/// Attach a UI of `cols` × `rows`, so the server begins emitting `redraw`s.
pub async fn attach(rpc: &Rpc, cols: u16, rows: u16) {
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(cols as u64),
            Value::from(rows as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
}

/// [`spawn`] the server with `init` and [`attach`] a `cols` × `rows` UI.
pub async fn start_attached(
    init: ServerInit,
    cols: u16,
    rows: u16,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = spawn(init);
    attach(&rpc, cols, rows).await;
    (rpc, incoming)
}

// ===== driving the editor ====================================================

/// Type a string of vim key-notation (a fire-and-forget `nvim_input` notify).
pub fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// A fake monotonic millisecond clock for mouse multi-click tests. Hand its
/// [`handle`](TestClock::handle) to [`ServerInit::mouse_clock`] before spawning,
/// keep the `TestClock`, and [`set_ms`](TestClock::set_ms) it between gestures to
/// place them inside or outside `'mousetime'` deterministically — the server
/// stamps each `nvim_input_mouse` from this instead of the wall clock, so
/// "two clicks 100 ms apart" never depends on real timing.
#[derive(Clone, Default)]
pub struct TestClock(Arc<AtomicU64>);

impl TestClock {
    /// A fresh clock reading `0`.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set the time the server will stamp onto the *next* mouse gesture. Only
    /// takes effect for gestures the server processes after this store, so always
    /// await a barrier (e.g. [`cursor`] / [`mode`]) between two `set_ms` + feeds.
    pub fn set_ms(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    /// The shared handle to put in [`ServerInit::mouse_clock`].
    pub fn handle(&self) -> Arc<AtomicU64> {
        self.0.clone()
    }
}

/// Send a mouse gesture via `nvim_input_mouse(button, action, "", 0, row, col)`
/// — `row`/`col` are global, 0-based screen cells (`grid 0`), the way a real
/// client reports them. Fire-and-forget; pair with a [`barrier`] / [`cursor`]
/// read to observe the effect. For modifiers or to assert the RPC *rejects* a
/// malformed call, drive `nvim_input_mouse` directly.
pub fn feed_mouse(rpc: &Rpc, button: &str, action: &str, row: usize, col: usize) {
    rpc.notify(
        "nvim_input_mouse",
        vec![
            Value::from(button),
            Value::from(action),
            Value::from(""),
            Value::from(0u64),
            Value::from(row as u64),
            Value::from(col as u64),
        ],
    );
}

/// Like [`feed_mouse`], but first advance `clock` to `ms` so the server stamps
/// this gesture at that time — for driving `'mousetime'`-based multi-click
/// (double/triple-click) deterministically. Await a barrier (e.g. [`cursor`] /
/// [`mode`]) before the next `feed_mouse_at`, so the server reads this gesture's
/// stamp before the clock moves on.
pub fn feed_mouse_at(
    rpc: &Rpc,
    clock: &TestClock,
    ms: u64,
    button: &str,
    action: &str,
    row: usize,
    col: usize,
) {
    clock.set_ms(ms);
    feed_mouse(rpc, button, action, row, col);
}

/// Run an ex-command via `nvim_command`.
pub async fn command(rpc: &Rpc, cmd: &str) {
    rpc.request("nvim_command", vec![Value::from(cmd)])
        .await
        .expect("command");
}

/// Fetch all current-buffer lines. Doubles as a **barrier**: awaiting the
/// response guarantees the server has processed every message queued before it.
pub async fn lines(rpc: &Rpc) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Lines of an explicit buffer `handle` (0 = current).
pub async fn buf_lines(rpc: &Rpc, handle: u64) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(handle),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("buf_get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Cursor position as `(1-based line, 0-based column)`.
///
/// CONVENTION: the line is **1-based** — this relays `nvim_win_get_cursor`
/// verbatim, which is 1-based like neovim, so a cursor on the first line reads as
/// line `1`. Beware: the multi-cursor tests' `secondary_cursors` helper reports a
/// **0-based screen row** instead, so the two are off by one — a secondary on the
/// same line as the primary is `secondary_cursors` row `N` vs `cursor` line
/// `N + 1`. Don't cross-compare the two raw.
pub async fn cursor(rpc: &Rpc) -> (usize, usize) {
    let (line, col) = cursor_u64(rpc).await;
    (line as usize, col as usize)
}

/// Cursor position as `(1-based line, 0-based column)`, as raw `u64`s.
pub async fn cursor_u64(rpc: &Rpc) -> (u64, u64) {
    let result = rpc
        .request("nvim_win_get_cursor", vec![Value::from(0u64)])
        .await
        .expect("get_cursor");
    match result {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0),
            a.get(1).and_then(Value::as_u64).unwrap_or(0),
        ),
        _ => (0, 0),
    }
}

/// The current mode (the `mode` field of `nvim_get_mode`).
pub async fn mode(rpc: &Rpc) -> String {
    let result = rpc
        .request("nvim_get_mode", vec![])
        .await
        .expect("get_mode");
    match result {
        Value::Map(map) => field(&map, "mode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Whether a focus-locked bottom panel (`:messages` / `:ls` / `nx.panel.open`) is
/// currently open — the open/closed oracle for panel tests (`nxvim_panel_is_open`).
pub async fn panel_is_open(rpc: &Rpc) -> bool {
    rpc.request("nxvim_panel_is_open", vec![])
        .await
        .expect("nxvim_panel_is_open")
        .as_bool()
        .unwrap_or(false)
}

/// A round-trip request whose only purpose is to act as a barrier: once it
/// resolves, every message sent before it (input, the redraw it triggered) has
/// been processed and queued. Uses `nvim_get_mode` (cheap, side-effect-free).
pub async fn barrier(rpc: &Rpc) {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
}

// ===== Lua ===================================================================

/// Run a Lua chunk via `nvim_exec_lua` and return its result value.
pub async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
}

/// `return`-style Lua chunk evaluated for a boolean (`None` if not a boolean).
pub async fn lua_bool(rpc: &Rpc, code: &str) -> Option<bool> {
    exec_lua(rpc, code).await.as_bool()
}

/// `return`-style Lua chunk evaluated for a `u64` (`None` if not an integer).
pub async fn lua_u64(rpc: &Rpc, code: &str) -> Option<u64> {
    exec_lua(rpc, code).await.as_u64()
}

// ===== redraw: draining ======================================================

/// Drain every queued notification and return the most recent `redraw`'s params
/// (the raw `Vec<Value>`), or `None` when none is buffered. *Params convention.*
pub fn drain_latest_redraw(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<Value>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = Some(params);
        }
    }
    latest
}

/// Drain every queued notification and return *all* `redraw` params in arrival
/// order. *Params convention.*
pub fn drain_all_redraws(incoming: &mut UnboundedReceiver<Incoming>) -> Vec<Vec<Value>> {
    let mut all = Vec::new();
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            all.push(params);
        }
    }
    all
}

/// Drain every queued `redraw` and return the most recent map for which `keep`
/// holds (skipping non-redraw notifications and rejected redraws), or `None`.
/// *Map convention.*
///
/// Panics if a `redraw` arrives without a map payload (a protocol violation).
pub fn drain_to_latest_redraw(
    incoming: &mut UnboundedReceiver<Incoming>,
    keep: impl Fn(&[(Value, Value)]) -> bool,
) -> Option<Vec<(Value, Value)>> {
    let mut latest = None;
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => {
                        if keep(&map) {
                            latest = Some(map);
                        }
                    }
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue, // a non-redraw notification — ignore
            Err(_) => return latest,
        }
    }
}

/// Wait for the most recent `redraw` whose map satisfies `keep`, up to a generous
/// timeout. *Map convention.*
///
/// Unlike [`drain_to_latest_redraw`] — which only inspects frames already queued
/// in `incoming` — this also *awaits* frames the client reader task has not
/// delivered yet. That makes it the robust choice after an action whose redraw is
/// emitted on a later server tick (e.g. an `exec_lua` that mutates layout), where
/// a plain drain can momentarily see only the stale prior frame under load (the
/// take-latest race described in the crate docs / CLAUDE.md). Panics if no
/// matching frame arrives before the timeout.
pub async fn wait_redraw(
    incoming: &mut UnboundedReceiver<Incoming>,
    keep: impl Fn(&[(Value, Value)]) -> bool,
) -> Vec<(Value, Value)> {
    // A matching frame may already be queued — take the most recent one.
    if let Some(map) = drain_to_latest_redraw(incoming, &keep) {
        return map;
    }
    // Otherwise await further notifications until one matches.
    let wait = async {
        loop {
            match incoming.recv().await {
                Some(Incoming::Notification { method, params }) if method == "redraw" => {
                    match params.into_iter().next() {
                        Some(Value::Map(map)) if keep(&map) => return map,
                        Some(Value::Map(_)) => continue,
                        _ => panic!("redraw without a map"),
                    }
                }
                Some(_) => continue, // a non-redraw notification — ignore
                None => panic!("notification channel closed before a matching redraw"),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a matching redraw")
}

// ===== redraw: accessors (params convention) =================================

/// A top-level value in the redraw map, addressed through the raw params
/// (`params[0]` is the map). *Params convention.*
pub fn redraw_get<'a>(params: &'a [Value], key: &str) -> Option<&'a Value> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The first window's sub-map (`windows[0]`) from the raw params, or `None`.
/// *Params convention.*
pub fn window0(params: &[Value]) -> Option<&Vec<(Value, Value)>> {
    match redraw_get(params, "windows")?.as_array()?.first()? {
        Value::Map(win) => Some(win),
        _ => None,
    }
}

/// A per-window value (`windows[0][key]`) addressed through the raw params.
/// *Params convention.*
pub fn window0_get<'a>(params: &'a [Value], key: &str) -> Option<&'a Value> {
    window0(params)?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The redraw's message-line text, addressed through the raw params.
/// *Params convention.*
pub fn message_of(params: &[Value]) -> String {
    redraw_get(params, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

// ===== redraw / map accessors (map convention) ===============================

/// Look up `key` in a redraw (or any) map: a top-level key, falling back to the
/// first window's sub-map (`windows[0]`). The two namespaces don't collide, so
/// the fallback keeps single-window helpers working across the per-window
/// protocol move. *Map convention.*
pub fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map_get(map, key).or_else(|| window0_field(map, key))
}

/// A strict top-level lookup (no window fallback) — also the right tool for any
/// plain msgpack map (tabline, window-tree, …). *Map convention.*
pub fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A per-window value from the first window's sub-map (`windows[0]`).
/// *Map convention.*
pub fn window0_field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    let Value::Map(win) = map_get(map, "windows")?.as_array()?.first()? else {
        return None;
    };
    map_get(win, key)
}

/// A `u64`-valued top-level key, defaulting to `0` when absent or non-integer.
pub fn u64_at(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

/// A string-valued field (top-level or `windows[0]`), or `""` when absent.
pub fn field_str(map: &[(Value, Value)], key: &str) -> String {
    field(map, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The redraw's message-line text. *Map convention.*
pub fn message(map: &[(Value, Value)]) -> String {
    field_str(map, "message")
}

// ===== temp filesystem =======================================================

/// A unique suffix (`<pid>_<n>`) for temp paths, stable within a test process.
fn unique() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{n}", std::process::id())
}

/// A fresh, uniquely-named `.txt` temp file path (not created).
pub fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nxvim_test_{tag}_{}.txt", unique()))
}

/// Create and return a fresh, uniquely-named temp directory.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}", unique()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write `content` to a fresh temp file with extension `ext`; return its path.
pub fn write_temp(tag: &str, ext: &str, content: &str) -> String {
    let path = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}.{ext}", unique()));
    std::fs::write(&path, content).expect("write temp file");
    path.to_string_lossy().into_owned()
}

/// Write `n` lines (`line1`..`lineN`) to a fresh temp file; return its path.
pub fn write_n_lines(tag: &str, n: usize) -> String {
    let path = temp_path(tag);
    let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&path, body).expect("write temp file");
    path.to_string_lossy().into_owned()
}

// ===== serialization =========================================================

/// A process-wide lock for tests that share mutable global state (env vars,
/// spawned subprocesses). Each test binary links its own instance, so the lock
/// serializes within a binary — which is exactly the scope that shares a process.
///
/// It's a [`tokio::sync::Mutex`], not a `std` one, so the guard can be held
/// across `.await` points — these tests `let _g = serial_lock().lock().await;`
/// at the top of an `async fn` and keep it for the whole body.
pub fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ===== clipboard =============================================================

/// An in-memory stand-in for the host clipboard, injected via
/// [`nxvim_server::ClipboardProvider::Custom`] so `"+` / `"*` round-trips are
/// deterministic and inspectable instead of environment-dependent.
#[derive(Clone, Default)]
pub struct FakeClipboard(std::sync::Arc<Mutex<Option<(String, bool)>>>);

impl nxvim_core::Clipboard for FakeClipboard {
    fn get(&self) -> Option<(String, bool)> {
        self.0.lock().unwrap().clone()
    }
    fn set(&self, text: &str, linewise: bool) {
        *self.0.lock().unwrap() = Some((text.to_string(), linewise));
    }
}

impl FakeClipboard {
    /// Seed the clipboard as if an external app put `text` on it.
    pub fn seed(&self, text: &str, linewise: bool) {
        *self.0.lock().unwrap() = Some((text.to_string(), linewise));
    }
    /// Read what the editor last wrote (or the seeded value).
    pub fn peek(&self) -> Option<(String, bool)> {
        self.0.lock().unwrap().clone()
    }
}
