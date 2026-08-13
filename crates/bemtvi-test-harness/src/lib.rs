//! Shared harness for bemtvi's black-box integration tests.
//!
//! Every test in the workspace drives the editor the same way a real UI client
//! does: it starts a real [`bemtvi_server`] on its own OS thread, connects over
//! the same msgpack-RPC a front end speaks (an in-process [`tokio::io::duplex`]
//! pipe), feeds vim key-notation through `btv_input`, and asserts on observable
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
//!   [`redraw_get`], [`window0`], [`message_of`].
//! - **map convention** — functions take the already-extracted map
//!   (`&[(Value, Value)]`): [`field`], [`map_get`], [`window0_field`], [`u64_at`],
//!   [`message`], plus the predicate-driven [`drain_to_latest_redraw`].
//!
//! Per-window fields (lines, cursor, highlights, diagnostics, scroll, …) live
//! under `windows[0]`; the `field`/`window0_*` helpers know to look there.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bemtvi_rpc::{connect, Incoming, Rpc};
use bemtvi_server::{run as run_server, ServerInit};
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
    // A named thread: when the server panics, the panic message lands on this
    // thread — `<unnamed>` would leave the test's eventual "rpc connection
    // closed" with no hint that the server died, let alone where.
    std::thread::Builder::new()
        .name("bemtvi-test-server".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("server runtime");
            // `run_server` returns `Ok(())` on a normal client disconnect (test
            // teardown), so an `Err` here is a genuine server failure. Surface it —
            // swallowing it leaves the test to die later with an opaque
            // "rpc connection closed" and no hint of the root cause.
            if let Err(e) = runtime.block_on(run_server(server_end, init)) {
                eprintln!("bemtvi-test-harness: server thread exited with error: {e:#}");
            }
        })
        .expect("spawn bemtvi-test-server thread");
    let (reader, writer) = tokio::io::split(client_end);
    connect(reader, writer)
}

/// Attach a UI of `cols` × `rows` declaring the given capability map entries —
/// the general form behind [`attach`] and its capability-declaring wrappers.
pub async fn attach_with_caps(rpc: &Rpc, cols: u16, rows: u16, caps: Vec<(Value, Value)>) {
    rpc.request(
        "btv_ui_attach",
        vec![
            Value::from(cols as u64),
            Value::from(rows as u64),
            Value::Map(caps),
        ],
    )
    .await
    .expect("ui attach");
}

/// Attach a UI of `cols` × `rows` with no declared capabilities (a legacy
/// terminal), so the server begins emitting `redraw`s.
pub async fn attach(rpc: &Rpc, cols: u16, rows: u16) {
    attach_with_caps(rpc, cols, rows, vec![]).await;
}

/// [`attach`] a UI that declares the **kitty keyboard protocol** active
/// (`keyboard_protocol = true` in the capabilities map), so the server parses
/// distinct `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>` instead of folding them onto their named
/// twins — the way a modern client attaches. Plain [`attach`] leaves it off (a
/// legacy terminal), which is what the default harness setup exercises.
pub async fn attach_keyboard_protocol(rpc: &Rpc, cols: u16, rows: u16) {
    attach_with_caps(
        rpc,
        cols,
        rows,
        vec![(Value::from("keyboard_protocol"), Value::Boolean(true))],
    )
    .await;
}

/// [`attach`] a UI that declares **truecolor** (24-bit color) support
/// (`truecolor = true` in the capabilities map), the way a rich terminal attaches.
/// The server defaults in the bundled `bemtvi` colorscheme on such an attach when
/// the config hasn't already chosen one. Plain [`attach`] leaves it off (a
/// 256-color / legacy terminal), so the registry stays empty by default.
pub async fn attach_truecolor(rpc: &Rpc, cols: u16, rows: u16) {
    attach_with_caps(
        rpc,
        cols,
        rows,
        vec![(Value::from("truecolor"), Value::Boolean(true))],
    )
    .await;
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

/// Start an 80×24-attached server editing a fresh temp file seeded with
/// `content` — the standard "open a buffer with known text" fixture.
pub async fn start_with_file(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("open", "txt", content);
    start_attached(
        ServerInit {
            file: Some(path),
            ..Default::default()
        },
        80,
        24,
    )
    .await
}

/// [`start_attached`] (80×24) with a fake mouse clock injected via
/// [`ServerInit::mouse_clock`], so a deterministic multi-click can be driven
/// (two presses placed inside `'mousetime'` with [`TestClock::set_ms`]).
pub async fn start_clocked_init(
    mut init: ServerInit,
) -> (Rpc, TestClock, UnboundedReceiver<Incoming>) {
    let clock = TestClock::new();
    init.mouse_clock = Some(clock.handle());
    let (rpc, incoming) = start_attached(init, 80, 24).await;
    (rpc, clock, incoming)
}

/// [`start_clocked_init`] with an otherwise-default init — the common case.
pub async fn start_clocked() -> (Rpc, TestClock, UnboundedReceiver<Incoming>) {
    start_clocked_init(ServerInit::default()).await
}

/// Write `init_lua` to `<dir>/init.lua` and return a [`ServerInit`] sourcing it
/// at startup (`dir` as both `config_dir` and the runtimepath).
pub fn config_init(dir: &std::path::Path, init_lua: &str) -> ServerInit {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    }
}

/// Start an 80×24-attached server sourcing `init_lua` from the throwaway config
/// dir `dir` (see [`config_init`]).
pub async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(config_init(dir, init_lua), 80, 24).await
}

/// Like [`start_with_config`] but also opens `file` in the initial buffer, so
/// the startup lifecycle seed (`BufReadPost`→`FileType`→`BufEnter`) fires for it.
pub async fn start_with_file_and_config(
    dir: &std::path::Path,
    file: &str,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let mut init = config_init(dir, init_lua);
    init.file = Some(file.to_string());
    start_attached(init, 80, 24).await
}

/// Drain `incoming` until the server closes the connection — await a `:qa`-style
/// exit before restarting against the same store.
pub async fn await_server_exit(mut incoming: UnboundedReceiver<Incoming>) {
    while incoming.recv().await.is_some() {}
}

// ===== driving the editor ====================================================

/// Type a string of vim key-notation (a fire-and-forget `btv_input` notify).
pub fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("btv_input", vec![Value::from(keys)]);
}

/// Feed `keys` as an awaited request, then a `nvim_get_mode` barrier — so the
/// input is fully processed server-side before the following read.
pub async fn feed_sync(rpc: &Rpc, keys: &str) {
    rpc.request("btv_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
}

/// A fake monotonic millisecond clock for mouse multi-click tests. Hand its
/// [`handle`](TestClock::handle) to [`ServerInit::mouse_clock`] before spawning,
/// keep the `TestClock`, and [`set_ms`](TestClock::set_ms) it between gestures to
/// place them inside or outside `'mousetime'` deterministically — the server
/// stamps each `btv_input_mouse` from this instead of the wall clock, so
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

/// Send a mouse gesture via `btv_input_mouse(button, action, "", 0, row, col)`
/// — `row`/`col` are global, 0-based screen cells (`grid 0`), the way a real
/// client reports them. Fire-and-forget; pair with a [`barrier`] / [`cursor`]
/// read to observe the effect. For modifiers or to assert the RPC *rejects* a
/// malformed call, drive `btv_input_mouse` directly.
pub fn feed_mouse(rpc: &Rpc, button: &str, action: &str, row: usize, col: usize) {
    feed_mouse_mod(rpc, button, action, "", row, col);
}

/// Like [`feed_mouse`], but with a `modifier` string (`btv_input_mouse`'s param 3 —
/// e.g. `"S"` for Shift, `"C-S"` for Ctrl+Shift) so tests can drive `<S-ScrollWheel>`
/// and other modified gestures.
pub fn feed_mouse_mod(
    rpc: &Rpc,
    button: &str,
    action: &str,
    modifier: &str,
    row: usize,
    col: usize,
) {
    rpc.notify(
        "btv_input_mouse",
        vec![
            Value::from(button),
            Value::from(action),
            Value::from(modifier),
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

/// Run an ex-command via `btv_command`.
pub async fn command(rpc: &Rpc, cmd: &str) {
    rpc.request("btv_command", vec![Value::from(cmd)])
        .await
        .expect("command");
}

/// Fetch all current-buffer lines. Doubles as a **barrier**: awaiting the
/// response guarantees the server has processed every message queued before it.
pub async fn lines(rpc: &Rpc) -> Vec<String> {
    buf_lines(rpc, 0).await
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
        // An empty reply is indistinguishable from an unexpected one — fail loud
        // rather than assert on a phantom empty buffer (no-silent-stubs).
        other => panic!("buf_lines: unexpected reply {other:?}"),
    }
}

/// Buffer `handle`'s name (`nvim_buf_get_name`; handle `0` = the current buffer).
pub async fn buf_name_of(rpc: &Rpc, handle: u64) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(handle)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The current buffer's name.
pub async fn buf_name(rpc: &Rpc) -> String {
    buf_name_of(rpc, 0).await
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
        // An unexpected reply would silently read as (0, 0) — fail loud (same
        // reason as `buf_lines`).
        other => panic!("cursor_u64: unexpected reply {other:?}"),
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

/// Whether a focus-locked bottom panel (`:messages` / `:ls` / `btv.panel.open`) is
/// currently open — the open/closed oracle for panel tests (`bemtvi_panel_is_open`).
pub async fn panel_is_open(rpc: &Rpc) -> bool {
    rpc.request("bemtvi_panel_is_open", vec![])
        .await
        .expect("bemtvi_panel_is_open")
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
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                latest = Some(params);
            }
            // A non-redraw notification or a server-initiated request (FS/proc/LSP
            // traffic, all of which interleave with `redraw`s on a busy channel):
            // skip it but keep draining, or a later redraw left behind it would make
            // this return a stale frame — the take-first flakiness CLAUDE.md warns of.
            Ok(_) => continue,
            Err(_) => return latest,
        }
    }
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

/// Wait for the next notification named `method` and return its params, up to a
/// generous timeout — for the one-shot server→client notifications that are not
/// `redraw` frames (`btv_ui_send`, …).
///
/// Take-*first*, deliberately: unlike a repaint, each of these carries a distinct
/// event, so "the latest" would silently drop earlier ones. Panics if none
/// arrives before the timeout.
pub async fn wait_notification(
    incoming: &mut UnboundedReceiver<Incoming>,
    method: &str,
) -> Vec<Value> {
    if let Some(params) = drain_notification(incoming, method) {
        return params;
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(params) = drain_notification(incoming, method) {
            return params;
        }
    }
    panic!("no {method:?} notification arrived");
}

/// The first queued notification named `method`, or `None` when none is buffered
/// — the non-blocking half of [`wait_notification`], for asserting that a
/// notification was *not* emitted (drain after a barrier that guarantees the
/// server has finished the tick).
pub fn drain_notification(
    incoming: &mut UnboundedReceiver<Incoming>,
    method: &str,
) -> Option<Vec<Value>> {
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method: m, params }) if m == method => {
                return Some(params);
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Wait for a `redraw` whose map satisfies `keep`, up to a generous timeout:
/// the most recent match already queued, else the first match to arrive.
/// *Map convention.* Because the await path returns the *first* arrival that
/// matches, pass a predicate specific enough to reject stale frames (a previous
/// action's trailing barrier repaint can land late under load) — a `|_| true`
/// predicate here has take-first semantics whenever the queue is empty.
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
    map_get(map, key)
}

/// The first window's sub-map (`windows[0]`) from the raw params, or `None`.
/// *Params convention.*
pub fn window0(params: &[Value]) -> Option<&Vec<(Value, Value)>> {
    match redraw_get(params, "windows")?.as_array()?.first()? {
        Value::Map(win) => Some(win),
        _ => None,
    }
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

// ===== input → redraw ========================================================

/// Feed `keys` and return the freshest resulting `redraw` map that satisfies
/// `keep` (*map convention*).
///
/// The server processes messages serially, writing each message's response then
/// its `redraw`; we send `btv_input` as a request then a `nvim_get_mode` barrier,
/// so once the barrier `.await` resolves the input's redraw is already queued.
/// We take the *most recent* qualifying redraw, not the first: a frame still in
/// flight from earlier in the test (the startup frame, or a previous call's
/// trailing barrier repaint) can land in `incoming` after the pre-drain below
/// when the reader task lags under load, and taking the first would then return
/// that stale frame — the source of intermittent, test-shuffling failures.
/// `keep` lets a caller pin the exact frame it means: the default
/// ([`redraw_after`]) takes the freshest state (the barrier's repaint is
/// state-identical to the input's), while scroll tests pass a predicate to
/// single out the input's own frame, the only one carrying the one-shot
/// `scroll` gesture (which the trailing barrier repaint lacks).
pub async fn redraw_after_matching(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
    keep: impl Fn(&[(Value, Value)]) -> bool,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // discard notifications buffered earlier in the test

    // request (not notify): the server responds *then* redraws, and the barrier
    // below relies on that ordering
    rpc.request("btv_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");

    if let Some(map) = drain_to_latest_redraw(incoming, &keep) {
        return map;
    }
    // The barrier guarantees the input's redraw is queued before its response, so
    // the drain above should have found it. Under heavy load the reader task can
    // still lag; poll a bounded while rather than failing on the first miss.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming, &keep) {
            return map;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Feed `keys` and return the freshest resulting `redraw` — the common case.
pub async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    redraw_after_matching(rpc, incoming, keys, |_| true).await
}

/// Feed `keys` and return the resulting message line ([`message`] over
/// [`redraw_after`]).
pub async fn message_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    message(&redraw_after(rpc, incoming, keys).await)
}

// ===== polling ===============================================================

/// Poll the Lua expression `code` (which must `return` a boolean) until it reads
/// `true` or the budget runs out — for state that settles asynchronously
/// (a plugin load, an off-tick chain).
pub async fn poll_true(rpc: &Rpc, code: &str) -> bool {
    for _ in 0..200 {
        if lua_bool(rpc, code).await == Some(true) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// Let the server run for `ms` of real time, then [`barrier`] so everything it did in
/// that window has been processed before the next assertion.
///
/// For a **negative** assertion — "give a spurious second event a chance to land, then
/// assert it didn't" — where no observable signals that the window has passed. Prefer an
/// event-driven barrier when one exists ([`poll_true`] on the state you expect, or
/// feeding a key to force the diff the spurious event would ride); a wall-clock wait is
/// the fallback, not the default.
///
/// **The wait has to be on this side.** `exec_lua(rpc, "return btv.promise.delay(80)")`
/// reads like a wait and is not: `nvim_exec_lua` answers with the chunk's *value* and
/// never awaits it, so the call returns in ~1ms carrying an unresolved promise table
/// (`{ _state = "pending" }`) and the window it was supposed to open is zero. A test
/// written that way passes for the wrong reason — it asserts on state sampled
/// immediately, not after the settle it claims to wait for.
pub async fn settle_ms(rpc: &Rpc, ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    barrier(rpc).await;
}

// ===== menu (picker / completion popup) ======================================

/// Poll for the latest redraw whose `menu` key is a map — the widget is open —
/// returning that frame (*map convention*), or `None` if none arrives within the
/// poll window. Each round sends a [`barrier`] to flush the server's queued
/// redraw onto the wire, then takes the latest matching frame.
pub async fn poll_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// Poll for the latest redraw whose `menu` key is *absent / nil* — no popup —
/// returning that frame so the caller can also assert on it, or `None` if every
/// frame in the window still carries a menu.
pub async fn poll_no_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            !matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// The `menu` sub-map of a redraw map (panics when absent) — pair with
/// [`poll_menu`], whose `Some` already guarantees it.
pub fn menu_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "menu") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a menu map, got {other:?}"),
    }
}

/// The menu's visible row labels, in order. Takes the `menu` **sub-map** (see
/// [`menu_of`]); a row is `[label, ...]` or a bare label string.
pub fn menu_items(menu: &[(Value, Value)]) -> Vec<String> {
    match map_get(menu, "items") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|row| match row {
                Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
                Value::String(s) => s.as_str().unwrap_or("").to_string(),
                other => panic!("unexpected menu row {other:?}"),
            })
            .collect(),
        other => panic!("expected menu items array, got {other:?}"),
    }
}

// ===== temp filesystem =======================================================

/// Prefix of a **run root** — the one directory a single test process puts all
/// of its temp paths in. Distinct from the `bemtvi-test-` prefix `btv.test.tempdir()`
/// uses, so the sweep below can never mistake one for the other.
const RUN_ROOT_PREFIX: &str = "bemtvi-testrun-";

/// The per-process run root: `$TMPDIR/bemtvi-testrun-<pid>`, created on first use.
///
/// Every path the helpers below hand out lives inside it, which is what makes
/// the temp footprint of a test binary a *single* directory that can be removed
/// wholesale when the process exits. Before this existed each helper dropped its
/// path straight into the shared system temp dir and nothing ever collected it,
/// so one `cargo test --workspace` left ~2000 entries (~130 MB) behind in `/tmp`.
///
/// First call also [sweeps](sweep_stale_temp_roots) the roots of runs that are
/// gone, and arms the exit hook that removes this one.
pub fn temp_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        sweep_stale_temp_roots();
        let root = std::env::temp_dir().join(format!("{RUN_ROOT_PREFIX}{}", std::process::id()));
        // `create_dir_all`, not `create_dir`: the sweep just removed any root
        // left by an earlier run that happened to hold this pid, but a *live*
        // sibling process cannot legitimately own our pid, so tolerating an
        // existing directory here costs nothing and keeps a recycled pid from
        // failing every test in the binary. The per-path `create_dir` /
        // `create_new` below still fail loud on a hostile pre-creation, which is
        // where the symlink/TOCTOU exposure actually is.
        //
        // Owner-only mode (unix): every fixture the helpers hand out lives under
        // this root, so `0700` here closes the read side of the hostile-temp-dir
        // model — another local user can neither list the root nor open any
        // fixture under it, whatever the per-file mode. The editor and any
        // subprocess a test spawns run as the same user, so nothing they do is
        // affected.
        let create_root = || -> std::io::Result<()> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder.recursive(true);
                builder.create(&root)
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(&root)
            }
        };
        create_root().unwrap_or_else(|e| panic!("create temp run root {}: {e}", root.display()));
        // Removes the root on a normal exit — including the `process::exit` the
        // libtest harness makes after reporting results, which runs no
        // destructors. A run that dies without unwinding leaves its root for the
        // sweep above to reclaim.
        unsafe { libc::atexit(remove_run_root) };
        root
    })
    .clone()
}

/// `atexit` handler: remove this process's run root. Best-effort — a failure at
/// exit must not turn a passing run into a nonzero exit status, and whatever it
/// leaves behind the next run's sweep reclaims.
///
/// Must not panic: unwinding out of an `extern "C"` handler is undefined
/// behavior, so every step here swallows its error.
extern "C" fn remove_run_root() {
    let root = std::env::temp_dir().join(format!("{RUN_ROOT_PREFIX}{}", std::process::id()));
    remove_tree(&root);
}

/// `remove_dir_all`, retried after restoring write+search permission on every
/// directory underneath.
///
/// A test that deliberately makes a directory unwritable — proving a save into
/// one fails safely, say — leaves a subtree the plain removal cannot enter if it
/// panics before restoring the mode. Without the retry that root is not merely
/// leaked once: the [sweep](sweep_stale_temp_roots) hits the same wall on every
/// subsequent run, so it would sit in the temp dir forever.
fn remove_tree(root: &std::path::Path) -> bool {
    if std::fs::remove_dir_all(root).is_ok() {
        return true;
    }
    if !root.exists() {
        return true;
    }
    chmod_dirs_writable(root);
    std::fs::remove_dir_all(root).is_ok()
}

/// Give the owner `rwx` on `dir` and every directory below it. Symlinks are not
/// followed (`symlink_metadata`), so a link planted inside the tree cannot steer
/// the chmod at a directory outside it.
fn chmod_dirs_writable(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        let _ = std::fs::set_permissions(dir, perms);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            chmod_dirs_writable(&entry.path());
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Remove every run root in the system temp dir whose process is gone, and
/// return how many were removed.
///
/// This is the half of the cleanup that survives runs which never unwind — a
/// SIGKILL, an abort under `panic=abort`, a killed CI job — and so never ran
/// their exit hook. Roots belonging to *live* processes (concurrent test
/// binaries in the same `cargo test` invocation, most of all) are left alone.
pub fn sweep_stale_temp_roots() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(RUN_ROOT_PREFIX)) else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 || process_is_alive(pid) {
            continue;
        }
        // Racy by nature: another run may be sweeping the same stale root. Ignore
        // the failure — either way it ends up gone.
        if remove_tree(&entry.path()) {
            swept += 1;
        }
    }
    swept
}

/// Whether `pid` names a live process. `kill(pid, 0)` performs the existence and
/// permission checks without delivering a signal; `EPERM` means the process
/// exists but belongs to someone else, which still counts as alive (and means
/// its root is not ours to remove).
fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A unique suffix (`<pid>_<n>_<rand>`) for temp paths, stable within a test
/// process. The `<rand>` component is an OS-seeded per-call random value, so a
/// path is not predictable from pid+counter alone: that narrows the symlink /
/// TOCTOU window in the world-writable system temp dir (defence in depth on top
/// of the `create_new` O_EXCL creates below) and avoids spurious collisions when
/// a pid is reused after an earlier run left files behind.
fn unique() -> String {
    use std::hash::BuildHasher;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rand = std::hash::RandomState::new().hash_one(n);
    format!("{}_{n}_{rand:016x}", std::process::id())
}

/// Create `path` exclusively (O_CREAT|O_EXCL) and write `content` to it. Failing
/// when the path already exists — including when it is a pre-planted symlink —
/// defeats the classic temp-file symlink attack in a shared temp dir (a plain
/// `fs::write` would follow the link and truncate the target). The unique names
/// mean an existing path is never expected, so this only ever fails loud on an
/// actual collision / hostile pre-creation, never in normal test flow.
fn write_new(path: &std::path::Path, content: &[u8]) {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // Owner-only on unix (no dependence on the caller's umask): a fixture that
    // may carry a test's credentials or plugin config is never left world-
    // readable in the shared temp dir even if the run root above is somehow
    // bypassed. The editor reads these files as the same user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options
        .open(path)
        .unwrap_or_else(|e| panic!("create temp file {}: {e}", path.display()));
    f.write_all(content).expect("write temp file");
}

/// Escape `path` for interpolation into a double-quoted Lua string literal.
pub fn q(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// A fresh, uniquely-named `.txt` temp file path (not created), inside this
/// run's [`temp_root`].
pub fn temp_path(tag: &str) -> PathBuf {
    temp_root().join(format!("bemtvi_test_{tag}_{}.txt", unique()))
}

/// Create and return a fresh, uniquely-named temp directory inside this run's
/// [`temp_root`]. Uses `create_dir` (not `create_dir_all`) so it fails loud if
/// the path already exists — an idempotent `create_dir_all` would silently
/// accept an attacker-planted directory or symlink at the (otherwise unique)
/// path.
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = temp_root().join(format!("bemtvi_test_{tag}_{}", unique()));
    // Owner-only on unix, like the run root it lives under (defence in depth
    // for fixtures a test writes into it with plain `fs::write`).
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&dir).expect("create temp dir");
    }
    #[cfg(not(unix))]
    std::fs::create_dir(&dir).expect("create temp dir");
    dir
}

/// Write `content` to a fresh temp file with extension `ext` inside this run's
/// [`temp_root`]; return its path.
pub fn write_temp(tag: &str, ext: &str, content: &str) -> String {
    let path = temp_root().join(format!("bemtvi_test_{tag}_{}.{ext}", unique()));
    write_new(&path, content.as_bytes());
    path.to_string_lossy().into_owned()
}

/// Write `n` lines (`line1`..`lineN`) to a fresh temp file; return its path.
pub fn write_n_lines(tag: &str, n: usize) -> String {
    let path = temp_path(tag);
    let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
    write_new(&path, body.as_bytes());
    path.to_string_lossy().into_owned()
}

// ===== daemon wire ===========================================================

/// An in-memory [`HostFs`](bemtvi_core::HostFs) for the **daemon** side of the
/// daemon-wire suites: path → bytes, plus optional directories. Paths under
/// `/virtual/...` prove content crossed the wire — the edit-host's local disk
/// cannot read them. Without registered directories,
/// [`read_dir`](bemtvi_core::HostFs::read_dir) errors on every path, so the
/// daemon's file/dir/new classification resolves a stored path to a file and an
/// absent one to a new-file — never mistaking a file for a directory.
#[derive(Clone, Default)]
pub struct DaemonFs {
    inner: Arc<Mutex<DaemonFsTree>>,
}

#[derive(Default)]
struct DaemonFsTree {
    files: std::collections::HashMap<PathBuf, Vec<u8>>,
    dirs: std::collections::HashMap<PathBuf, Vec<(bool, String)>>,
    fail_writes: bool,
    hold: Option<Arc<WriteGate>>,
}

/// The shared state behind [`DaemonFs::hold_writes`]: `released` gates the daemon's
/// `write_atomic`, `parked` counts the writers currently waiting on it.
#[derive(Default)]
struct WriteGate {
    state: Mutex<(bool, usize)>,
    cv: std::sync::Condvar,
}

/// A parked-writes latch from [`DaemonFs::hold_writes`] — every daemon-side write
/// blocks until it is [`release`](WriteHold::release)d.
///
/// This is what makes "the buffer was edited *while the write was in flight*"
/// deterministic instead of a race: park the write, edit, then let the ack land. The
/// wait is a real blocking wait on the daemon's task, so a test using this **must** run
/// on a multi-thread runtime (`#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`)
/// or the parked daemon stalls the editor too.
pub struct WriteHold {
    gate: Arc<WriteGate>,
    tree: Arc<Mutex<DaemonFsTree>>,
}

impl WriteHold {
    /// Poll until at least one daemon write is parked on this latch, so the test knows
    /// the snapshot has been taken and the bytes are on the wire. Panics if none parks
    /// within the budget — a silent "nothing was in flight" would make the test that
    /// edits mid-write prove nothing.
    pub async fn await_parked(&self) {
        for _ in 0..200 {
            if self.gate.state.lock().unwrap().1 > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("no daemon write parked on the hold within 2s");
    }

    /// Let every parked (and future) write through.
    pub fn release(self) {
        self.clear();
    }
}

impl Drop for WriteHold {
    fn drop(&mut self) {
        // Also covers a hold that is dropped without `release()`: without this the
        // tree's `hold` would stay armed, silently parking every later write in the
        // process. Clearing on drop makes a forgotten hold benign instead of a
        // wedge.
        self.clear();
    }
}

impl WriteHold {
    fn clear(&self) {
        self.tree.lock().unwrap().hold = None;
        self.gate.state.lock().unwrap().0 = true;
        self.gate.cv.notify_all();
    }
}

impl DaemonFs {
    /// A fake pre-seeded with one file.
    pub fn with(path: &str, contents: &str) -> Self {
        let me = DaemonFs::default();
        me.set(path, contents);
        me
    }

    /// A fake pre-seeded with several `(path, contents)` files — for multi-buffer
    /// tests (`:wall` / `:wqa`, cross-file LSP).
    pub fn with_files(entries: &[(&str, &str)]) -> Self {
        let me = DaemonFs::default();
        for (path, contents) in entries {
            me.set(path, contents);
        }
        me
    }

    /// Store (or overwrite) `path`'s contents — both a test's initial seeding and a
    /// mid-test mutation (an external writer changing the remote file) a reload or
    /// the file watch should then see across the wire. Chainable.
    pub fn set(&self, path: &str, contents: &str) -> &Self {
        self.set_bytes(path, contents.as_bytes())
    }

    /// Store raw `bytes` (not necessarily valid UTF-8) — for the encoding-seam
    /// tests. Chainable.
    pub fn set_bytes(&self, path: &str, bytes: &[u8]) -> &Self {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(PathBuf::from(path), bytes.to_vec());
        self
    }

    /// Register a directory at `path` whose entries are `(is_dir, name)` pairs.
    /// Chainable.
    pub fn dir(&self, path: &str, entries: &[(bool, &str)]) -> &Self {
        let entries = entries
            .iter()
            .map(|(is_dir, name)| (*is_dir, name.to_string()))
            .collect();
        self.inner
            .lock()
            .unwrap()
            .dirs
            .insert(PathBuf::from(path), entries);
        self
    }

    /// Park every subsequent [`write_atomic`](bemtvi_core::HostFs::write_atomic) until
    /// the returned [`WriteHold`] is released — the seam for asserting on a buffer that
    /// is edited *while its write is in flight*. See [`WriteHold`] for the
    /// multi-thread-runtime requirement.
    pub fn hold_writes(&self) -> WriteHold {
        // A second hold would drop the first gate while its test still relies on it,
        // silently unparking writes it meant to park — fail loud instead.
        assert!(
            self.inner.lock().unwrap().hold.is_none(),
            "hold_writes: a hold is already armed (the previous WriteHold was \
             released out of order?)"
        );
        let gate = Arc::new(WriteGate::default());
        self.inner.lock().unwrap().hold = Some(gate.clone());
        WriteHold {
            gate,
            tree: self.inner.clone(),
        }
    }

    /// Make every subsequent [`write_atomic`](bemtvi_core::HostFs::write_atomic)
    /// fail loud (`PermissionDenied`) — the edit-host must surface it, never
    /// report a silent success. Chainable.
    pub fn fail_writes(&self, fail: bool) -> &Self {
        self.inner.lock().unwrap().fail_writes = fail;
        self
    }

    /// The bytes currently stored at `path`, as a string — the daemon's view of
    /// what the editor wrote across the wire. `None` if nothing is stored there.
    pub fn content(&self, path: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .files
            .get(std::path::Path::new(path))
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

impl bemtvi_core::HostFs for DaemonFs {
    fn exists(&self, path: &std::path::Path) -> bool {
        let t = self.inner.lock().unwrap();
        t.files.contains_key(path) || t.dirs.contains_key(path)
    }

    fn open_read(&self, path: &std::path::Path) -> std::io::Result<Box<dyn std::io::Read>> {
        match self.inner.lock().unwrap().files.get(path) {
            Some(bytes) => Ok(Box::new(std::io::Cursor::new(bytes.clone()))),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            )),
        }
    }

    fn stat(&self, path: &std::path::Path) -> Option<bemtvi_core::FileStat> {
        self.inner
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|b| bemtvi_core::FileStat {
                mtime: None,
                size: b.len() as u64,
            })
    }

    fn write_atomic(&self, path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
        // Park here if a `hold_writes` latch is armed — taken *without* the tree lock
        // held, so the test can still read the fake while a write waits.
        let hold = self.inner.lock().unwrap().hold.clone();
        if let Some(gate) = hold {
            let mut state = gate.state.lock().unwrap();
            state.1 += 1;
            gate.cv.notify_all();
            while !state.0 {
                state = gate.cv.wait(state).unwrap();
            }
            state.1 -= 1;
        }
        let mut t = self.inner.lock().unwrap();
        if t.fail_writes {
            // A loud failure the edit-host must surface — never a silent success.
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "daemon refuses the write",
            ));
        }
        t.files.insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn read_dir(&self, dir: &std::path::Path) -> std::io::Result<Vec<bemtvi_core::DirEntry>> {
        match self.inner.lock().unwrap().dirs.get(dir) {
            Some(entries) => Ok(entries
                .iter()
                .map(|(is_dir, name)| bemtvi_core::DirEntry {
                    is_dir: *is_dir,
                    name: name.clone(),
                })
                .collect()),
            // No directory registered: a file path must not classify as one.
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a directory",
            )),
        }
    }

    fn canonicalize(&self, path: &std::path::Path) -> std::io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// Start a server whose async fs is a [`bemtvi_server::RemoteHostFs`] talking to a
/// [`bemtvi_server::serve_fs_daemon`] (backed by `fake`) over an in-process duplex,
/// with `init`'s other fields as given (`host_fs_async` is filled in here).
/// UI-attached. The daemon task and the remote fs's RPC tasks live on the test
/// runtime; the server runs on its own thread and reaches the daemon only through
/// the injected async fs. The client's notification receiver is returned (not
/// dropped: dropping it would tear the client connection down and stop the server).
pub async fn spawn_with_daemon_fs_init(
    fake: DaemonFs,
    mut init: ServerInit,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_fs_daemon(daemon_reader, daemon_writer, Box::new(fake)).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    init.host_fs_async = Some(Box::new(bemtvi_server::RemoteHostFs::connect(
        host_reader,
        host_writer,
    )));
    let (rpc, incoming) = spawn(init);
    // `attach` returning proves startup did not block on the (deferred) file fetch.
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Start a server whose **language servers run on a daemon**: `init.lsp_transport`
/// is a [`bemtvi_server::RemoteLspTransport`] talking to a
/// [`bemtvi_server::serve_lsp_daemon`] over an in-process duplex, so every server's
/// stdio is tunneled over the wire exactly as a `--connect-daemon` session tunnels
/// it. The fs stays local, which isolates the LSP leg as the thing under test.
///
/// The daemon side spawns real children, so a test still points `$BEMTVI_LSP_CMD` /
/// `$BEMTVI_LSP_CMD_<NAME>` at the scripted mock — the override is resolved
/// edit-host-side into the [`ServerSpawn`](bemtvi_lsp::ServerSpawn) that crosses the
/// wire, so it reaches the daemon's spawn unchanged.
pub async fn spawn_with_daemon_lsp(mut init: ServerInit) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_lsp_daemon(daemon_reader, daemon_writer).await;
    });
    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    init.lsp_transport = Some(Box::new(bemtvi_server::RemoteLspTransport::connect(
        host_reader,
        host_writer,
    )));
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// [`spawn_with_daemon_fs_init`] opening `file`, with an otherwise-default init —
/// the common case.
pub async fn spawn_with_daemon_fs(
    fake: DaemonFs,
    file: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    spawn_with_daemon_fs_init(
        fake,
        ServerInit {
            file: Some(file.to_string()),
            ..Default::default()
        },
    )
    .await
}

/// Poll the current buffer's lines until `pred` accepts them or the budget runs
/// out — an off-tick fill (initial daemon fetch, `:e` reload, async listing)
/// lands a moment after the triggering action, so a bounded retry beats a fixed
/// sleep. Returns the final lines either way, so a failed assert shows what
/// *did* arrive.
pub async fn await_lines_where(rpc: &Rpc, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..200 {
        let lines = buf_lines(rpc, 0).await;
        if pred(&lines) {
            return lines;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// [`await_lines_where`] for the common case: poll until the lines equal `want`.
pub async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    await_lines_where(rpc, |lines| lines == want).await
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
/// [`bemtvi_server::ClipboardProvider::Custom`] so `"+` / `"*` round-trips are
/// deterministic and inspectable instead of environment-dependent.
#[derive(Clone, Default)]
pub struct FakeClipboard(std::sync::Arc<Mutex<Option<(String, bool)>>>);

impl bemtvi_core::Clipboard for FakeClipboard {
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
