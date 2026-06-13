//! The wasm (emscripten) edit-host — Phase 5 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`.
//!
//! This drives the **real** synchronous [`EditHost`] tick (the same one
//! `nxvim-server`'s native [`run`](nxvim_server) loop drives — core + the PUC Lua 5.1
//! VM + the full server glue: autocmds, mirrors, lifecycle, the redraw projection)
//! behind a wasm [`HostEffects`] ([`WasmEffects`]); the keystroke path is the production
//! tick, not a hand-wired minimal tie-in.
//!
//! **Interop (emscripten, not wasm-bindgen):** JS→Rust via `ccall`/`cwrap` on the
//! `#[no_mangle] extern "C"` exports below; the redraw goes the other way as a return
//! value (JSON) the JS side reads, rather than a pushed `EM_JS` callback. Slice 5c runs
//! these exports **inside a Web Worker** (`web/worker.mjs`) and ferries the JSON redraw
//! UI-ward over `postMessage`; the UI (`web/index.html`) renders it and exposes the
//! `window.__nxvim` Playwright hook. Slice 5d drives the Worker's run loop off a
//! `SharedArrayBuffer` + `Atomics.wait` park — the same wait that blocks on input also
//! fires Worker-side timers (`vim.defer_fn` / `nx.timer`) via [`eh_set_clock`] /
//! [`eh_next_deadline`] / [`eh_tick_timers`], the wheel `evloop.rs` can't provide
//! in-Worker. **Phase 6 (serverless OPFS):** files live in the browser's Origin Private
//! File System. There is no daemon, but OPFS handle acquisition is *async* (only a
//! `FileSystemSyncAccessHandle`'s operations are sync), so a synchronous [`HostFs`] is
//! impossible without Asyncify — instead `:e` / `:w` route through the *same off-tick
//! seam* a daemon session uses ([`HostEffects::fs_fetch`] / [`HostEffects::fs_save`]),
//! and the Worker fulfills them against OPFS between ticks ([`eh_take_fs_requests`] →
//! [`eh_fs_read_complete`] / [`eh_fs_write_complete`]). LSP / native-treesitter / process
//! spawn remain unavailable and fail loud (a later daemon slice re-enables them).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::rc::Rc;

use nxvim_core::{BufferId, DirEntry, Editor, NumberedMark, PendingSave, PersistState};
use nxvim_lua::{BlockingSystem, LuaRuntime, SystemOutput, SystemSpec};
use nxvim_server::{EditHost, HostEffects};
use rmpv::Value;

/// The blocking shell-out seam (`nx._system`, behind `vim.fn.system` / a config's
/// `root_dir` probe) on the serverless browser build: there is no process to run — a
/// later Phase 6 daemon slice would carry one over the wire — so every call fails *loud*
/// with a named message. Without this, [`LuaRuntime`]'s default `StdBlockingSystem` would
/// reach `std::process::Command`, which on emscripten degrades to a cryptic
/// "failed to spawn" errno; per *no silent stubs / fail loud with the name of what's
/// missing*, the browser build says plainly that processes aren't available. The seam's
/// contract is to *return a degraded [`SystemOutput`]*, never raise (callers rely on a
/// value), so `code = -1` + the message on stderr is the loud form here.
struct WasmBlockingSystem;

impl BlockingSystem for WasmBlockingSystem {
    fn run(&self, _spec: SystemSpec) -> SystemOutput {
        SystemOutput::failed(
            "processes (vim.fn.system / vim.system) are not available in the browser build yet",
        )
    }
}

/// The outbound effects the wasm edit-host captures for the UI to drain. The
/// [`WasmEffects`] writes here; the FFI layer reads it back out (the redraw via
/// [`eh_redraw_json`]). Shared by [`Rc`]`<`[`RefCell`]`<…>>` so the trait object the
/// [`EditHost`] owns and the [`WasmEditHost`] handle the FFI holds see the same buffer.
#[derive(Default)]
struct Sink {
    /// The latest `redraw` notification's params — the editor view the UI renders.
    /// Overwritten each frame (the UI only ever wants the most recent); serialized by
    /// [`eh_redraw_json`].
    last_redraw: Option<Vec<Value>>,
    /// Non-`redraw` notifications (`nxvim_exit`, scripted `nxvim_panel_select`, …) in
    /// arrival order — queued, not dropped. The Worker transport (slice 5c) drains
    /// these UI-ward; v1's FFI doesn't surface them yet, but they are captured rather
    /// than silently discarded.
    notifications: Vec<(String, Vec<Value>)>,
    /// Off-tick OPFS **reads** the editor tick deferred this convergence (Phase 6): one
    /// `(buffer, path)` per `:edit` / startup open, recorded by
    /// [`fs_fetch`](HostEffects::fs_fetch) and drained UI-ward by [`eh_take_fs_requests`]
    /// for the Worker to fulfill against OPFS, then landed via [`eh_fs_read_complete`].
    fs_reads: Vec<(BufferId, String)>,
    /// `seq`s of off-tick OPFS **writes** newly enqueued this convergence (one per `:w`),
    /// recorded by [`fs_save`](HostEffects::fs_save). Drained by [`eh_take_fs_requests`]
    /// so each write is dispatched to the Worker exactly once; the [`PendingSave`] itself
    /// stays in [`fs_writes`](Sink::fs_writes) until its ack lands.
    fs_write_queue: Vec<u64>,
    /// In-flight off-tick OPFS writes, keyed by [`PendingSave::seq`]. Holds the whole
    /// snapshot — its `bytes` (handed to JS via [`eh_save_bytes`]) and the metadata
    /// [`EditHost::complete_fs_write`] needs to finalize the buffer's saved-state — until
    /// the Worker reports the OPFS write done ([`eh_fs_write_complete`]), which removes it.
    fs_writes: HashMap<u64, PendingSave>,
}

/// The wasm [`HostEffects`]: the analogue of `nxvim-server`'s `NativeEffects`, but the
/// "client wire" is the [`Sink`] the JS UI drains instead of msgpack-RPC. The editor
/// runs in **off-tick fs** mode ([`has_remote_fs`](HostEffects::has_remote_fs) is `true`)
/// because OPFS is async to open: `:e` / `:w` record their read/write into the [`Sink`]
/// for the Worker to fulfill against OPFS between ticks. The remaining off-tick effects
/// (LSP, native treesitter, watch) stay unreachable on this build (see each method).
struct WasmEffects {
    sink: Rc<RefCell<Sink>>,
}

impl HostEffects for WasmEffects {
    fn notify(&mut self, method: &str, params: Vec<Value>) {
        let mut sink = self.sink.borrow_mut();
        if method == "redraw" {
            sink.last_redraw = Some(params);
        } else {
            sink.notifications.push((method.to_string(), params));
        }
    }

    fn respond(&mut self, _id: u64, _result: Result<Value, Value>) {
        // The wasm build feeds input through the FFI exports, not the msgpack-RPC
        // dispatch router (which is gated off the wasm build — slice 5a), so nothing
        // ever issues a client request that awaits a response. Reaching here means an
        // ungated caller started routing RPC into the wasm tick — fail loud so it's
        // caught, rather than silently swallow a reply no one reads.
        unreachable!("respond: the wasm edit-host takes input via FFI, not RPC requests")
    }

    fn fs_fetch(&mut self, buffer: BufferId, path: String) {
        // An off-tick `:edit` / startup open (Phase 6 — OPFS): record the request for the
        // Worker to fulfill against OPFS between ticks (`eh_take_fs_requests` →
        // `eh_fs_read_complete`). OPFS handle acquisition is async, so the read can't run
        // synchronously here on the editor thread — it crosses to the Worker's async leg,
        // exactly as a daemon read crosses the wire.
        self.sink.borrow_mut().fs_reads.push((buffer, path));
    }

    fn fs_save(&mut self, save: PendingSave) {
        // An off-tick `:w` (Phase 6 — OPFS): stash the whole snapshot keyed by its seq (so
        // `eh_save_bytes` can hand the bytes to JS and `complete_fs_write` can finalize on
        // the ack) and queue the seq for `eh_take_fs_requests` to dispatch to the Worker.
        let seq = save.seq;
        let mut sink = self.sink.borrow_mut();
        sink.fs_writes.insert(seq, save);
        sink.fs_write_queue.push(seq);
    }

    fn fs_watch(&mut self, _path: String) {
        // `sync_buffer_watches` is native-only; the wasm build arms no file watches (OPFS
        // has no change-notification, and the serverless editor is the sole writer).
        unreachable!("fs_watch: the wasm edit-host arms no file watches (native-only)")
    }

    fn fs_unwatch(&mut self, _path: String) {
        unreachable!("fs_unwatch: the wasm edit-host arms no file watches (native-only)")
    }

    fn has_remote_fs(&self) -> bool {
        // OPFS is an *off-tick* fs (its handle acquisition is async), so the editor tick
        // takes the off-tick `:e`/`:w` branches — `fs_fetch` / `fs_save` above — exactly
        // as a daemon session does, only the transport is OPFS instead of the wire.
        true
    }

    fn ts_install(&mut self, _lang: String) {
        // `:TSInstall` echoes a loud "not available in the browser build yet" at the
        // ex-command layer (excmd.rs) on this build instead of reaching the effect, so
        // this is unreachable; guard it loudly in case that gating ever regresses.
        unreachable!("ts_install: native treesitter is unavailable in the browser build")
    }
}

/// The FFI handle: the real [`EditHost`] plus a clone of the [`Sink`] its
/// [`WasmEffects`] writes to, so the exports can read the captured redraw back out.
pub struct WasmEditHost {
    host: EditHost,
    sink: Rc<RefCell<Sink>>,
}

/// Default UI size the wasm host attaches at; the JS side resizes via a re-attach
/// (slice 5c). 80×24 matches a conventional terminal.
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Borrow a C string as `&str` (empty on null / bad UTF-8).
///
/// # Safety
/// `p` must be a valid, NUL-terminated C string pointer for the call's duration.
unsafe fn as_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    CStr::from_ptr(p).to_str().unwrap_or("")
}

/// Move a `String` out to JS as an owned `char*`; the caller frees it via
/// [`eh_free_string`] (the harness's `readStr` does this).
fn into_owned_cstr(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).unwrap().into_raw()
}

/// Convert a msgpack [`Value`] (an rmpv redraw frame) into a [`serde_json::Value`] for
/// the JS side. Map keys are stringified (the redraw map's are all strings already);
/// binary/ext — which the redraw projection never emits — degrade to a byte array /
/// null rather than failing.
fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(n) => n
            .as_i64()
            .map(Into::into)
            .or_else(|| n.as_u64().map(Into::into))
            .unwrap_or(J::Null),
        Value::F32(f) => serde_json::Number::from_f64(*f as f64)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::F64(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::String(s) => J::String(s.as_str().unwrap_or("").to_string()),
        Value::Binary(b) => J::Array(b.iter().map(|&byte| J::from(byte)).collect()),
        Value::Array(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Map(pairs) => J::Object(
            pairs
                .iter()
                .map(|(k, val)| (json_key(k), value_to_json(val)))
                .collect(),
        ),
        Value::Ext(..) => J::Null,
    }
}

/// Stringify a msgpack map key for JSON (which requires string keys). Redraw maps key
/// on strings; anything else falls back to its `Display`/`Debug` form rather than
/// dropping the entry.
fn json_key(k: &Value) -> String {
    match k {
        Value::String(s) => s.as_str().unwrap_or("").to_string(),
        Value::Integer(n) => n.to_string(),
        other => format!("{other}"),
    }
}

/// Construct the wasm edit-host: a fresh editor + a fresh Lua VM (empty runtimepath —
/// no plugins/config in v1), wired to a [`WasmEffects`], then booted (the serverless
/// startup seed) and attached at the default UI size so the first frame is ready.
/// Returns null if the Lua VM fails to initialize.
///
/// # Safety
/// The returned pointer must be freed exactly once via [`eh_free`].
#[no_mangle]
pub extern "C" fn eh_new() -> *mut WasmEditHost {
    let sink = Rc::new(RefCell::new(Sink::default()));
    let lua = match LuaRuntime::new(Vec::new()) {
        Ok(lua) => lua,
        Err(_) => return std::ptr::null_mut(),
    };
    // No processes in the serverless browser build — make `nx._system` fail loud with a
    // named message rather than emscripten's cryptic spawn errno (StdBlockingSystem).
    lua.set_blocking_system(Rc::new(WasmBlockingSystem));
    let fx = Box::new(WasmEffects { sink: sink.clone() });
    let mut host = EditHost::new(Editor::new(), lua, fx);
    // OPFS is async to open, so `:e` / `:w` defer to the off-tick seam the Worker
    // fulfills (Phase 6); turn it on before boot so the very first open routes there.
    host.enable_offtick_fs();
    // Seed the serverless startup (buffer snapshot + mirrors + baselines) *before*
    // attaching the UI. This is only the FIRST half of boot — the Worker sources an
    // optional `init.lua` from OPFS next, then calls `eh_boot_finish` to fire the
    // startup lifecycle events + `v:vim_did_enter` (native ordering: config first).
    host.boot_begin();
    host.attach_ui(DEFAULT_COLS, DEFAULT_ROWS);
    Box::into_raw(Box::new(WasmEditHost { host, sink }))
}

/// Feed vim key-notation (e.g. `"ihello<Esc>"`) through the real tick and project the
/// resulting frame into the [`Sink`] (read back via [`eh_redraw_json`]).
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `notation` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_input(h: *mut WasmEditHost, notation: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    handle.host.feed(as_str(notation));
}

/// Apply a mouse gesture through the real tick and project the resulting frame — the
/// `nvim_input_mouse` counterpart of [`eh_input`]. `button`/`action`/`modifier` are the
/// `nvim_input_mouse` strings (`"left"`/`"wheel"`, `"press"`/`"drag"`/`"release"`/`"up"`/
/// `"down"`, `"CS"`…) and `row`/`col` the 0-based global screen cell; core owns the
/// hit-test (single-grid). The Worker sets the clock ([`eh_set_clock`]) before this call
/// so multi-click timing is right.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `button`/`action`/`modifier` valid
/// C strings.
#[no_mangle]
pub unsafe extern "C" fn eh_input_mouse(
    h: *mut WasmEditHost,
    button: *const c_char,
    action: *const c_char,
    modifier: *const c_char,
    row: usize,
    col: usize,
) {
    let Some(handle) = h.as_mut() else { return };
    handle
        .host
        .mouse(as_str(button), as_str(action), as_str(modifier), row, col);
}

/// Source a single-file `init.lua` (read from OPFS by the Worker) during startup —
/// run after [`eh_new`]'s boot-begin and before [`eh_boot_finish`], so a config's
/// startup-buffer autocmds (`BufEnter` …) fire. Returns an owned C string: empty on success,
/// else the Lua error message (the Worker surfaces it). The editor still finishes
/// booting on error. `require` of further modules won't resolve (empty runtimepath) —
/// this is one self-contained file.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `code` a valid C string. Free the
/// returned pointer with [`eh_free_string`].
#[no_mangle]
pub unsafe extern "C" fn eh_source_lua(h: *mut WasmEditHost, code: *const c_char) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(String::new());
    };
    match handle.host.source_config(as_str(code)) {
        Ok(()) => into_owned_cstr(String::new()),
        Err(e) => into_owned_cstr(e),
    }
}

/// Finish serverless startup: fire the lifecycle events and mark `v:vim_did_enter`.
/// Run by the Worker after [`eh_new`] and the optional [`eh_source_lua`] config sourcing
/// — the second half of the boot [`eh_new`] began.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_boot_finish(h: *mut WasmEditHost) {
    if let Some(handle) = h.as_mut() {
        handle.host.boot_finish();
    }
}

/// Re-attach the UI at a new `cols` × `rows` size (the resize path) and repaint — the
/// browser fires this on window resize so the redraw projects into the new grid. A
/// no-op-shaped wrapper over [`EditHost::attach_ui`], which both sizes the grid and
/// pushes a fresh frame into the [`Sink`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_attach(h: *mut WasmEditHost, cols: usize, rows: usize) {
    let Some(handle) = h.as_mut() else { return };
    handle.host.attach_ui(cols.max(1), rows.max(1));
}

/// Set the Worker's current JS clock (ms) so a timer armed during the next input tick
/// computes its deadline relative to now. The Worker calls this before [`eh_input`].
/// `now_ms` is a `double` (`performance.now()` / `Date.now()`), floored to whole ms.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_set_clock(h: *mut WasmEditHost, now_ms: f64) {
    if let Some(handle) = h.as_mut() {
        handle.host.set_clock(now_ms.max(0.0) as u64);
    }
}

/// The soonest pending timer deadline (ms on the JS clock), or `-1` when no timer is
/// armed — the Worker uses it as its `Atomics.wait` timeout so the one park that wakes on
/// a keystroke also wakes to fire the next `vim.defer_fn` / `nx.timer` (slice 5d).
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_next_deadline(h: *mut WasmEditHost) -> f64 {
    match h
        .as_ref()
        .and_then(|handle| handle.host.next_timer_deadline())
    {
        Some(due_ms) => due_ms as f64,
        None => -1.0,
    }
}

/// Fire every timer due at `now_ms` (the Worker calls this on each wake), running each
/// Lua callback through the real effects path and projecting a frame if any fired.
/// Returns `1` when at least one timer fired (so the Worker posts a fresh redraw), else
/// `0`.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_tick_timers(h: *mut WasmEditHost, now_ms: f64) -> i32 {
    match h.as_mut() {
        Some(handle) => i32::from(handle.host.fire_due_timers(now_ms.max(0.0) as u64)),
        None => 0,
    }
}

// ============================================================================
// Off-tick OPFS fs (Phase 6). The editor enqueues reads/writes off the keystroke
// tick (`has_remote_fs() == true`); the Worker drains them here, runs the async OPFS
// op between ticks, and reports the result back — the OPFS analogue of the daemon's
// `select!` arms (`apply_open` / `apply_save_done`), only the transport is OPFS.
// ============================================================================

/// Drain the off-tick fs requests the editor enqueued since the last call, as JSON the
/// Worker fulfills against OPFS:
/// `{"reads":[{"buffer":N,"path":"…"}],"writes":[{"seq":N,"path":"…","lines":N}]}`. The
/// reads are removed (the Worker lands each via [`eh_fs_read_complete`]); each write entry
/// names a queued `seq` whose bytes the Worker fetches with [`eh_save_bytes`] /
/// [`eh_save_len`] and whose result it reports with [`eh_fs_write_complete`]. Caller frees
/// with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_fs_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"reads":[],"writes":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    let reads: Vec<serde_json::Value> = sink
        .fs_reads
        .drain(..)
        .map(|(buffer, path)| serde_json::json!({ "buffer": buffer.0, "path": path }))
        .collect();
    let queued: Vec<u64> = std::mem::take(&mut sink.fs_write_queue);
    let writes: Vec<serde_json::Value> = queued
        .into_iter()
        .filter_map(|seq| {
            sink.fs_writes.get(&seq).map(|save| {
                serde_json::json!({
                    "seq": seq,
                    "path": save.path.display().to_string(),
                    "lines": save.lines,
                })
            })
        })
        .collect();
    into_owned_cstr(serde_json::json!({ "reads": reads, "writes": writes }).to_string())
}

/// Pointer to the snapshotted bytes of the in-flight off-tick write `seq` (a `double` so
/// the small counter crosses the JS boundary without 64-bit-int marshalling), for the
/// Worker to copy out (`HEAPU8.subarray(ptr, ptr + len)`) and write to OPFS. Valid until
/// [`eh_fs_write_complete`] removes that save; null if `seq` is unknown. With [`eh_save_len`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; the returned pointer is read-only
/// and must be consumed before the next FFI call that could mutate the save map.
#[no_mangle]
pub unsafe extern "C" fn eh_save_bytes(h: *mut WasmEditHost, seq: f64) -> *const u8 {
    h.as_ref()
        .and_then(|handle| {
            let sink = handle.sink.borrow();
            sink.fs_writes
                .get(&(seq.max(0.0) as u64))
                .map(|s| s.bytes.as_ptr())
        })
        .unwrap_or(std::ptr::null())
}

/// Byte length of the in-flight off-tick write `seq`'s snapshot (`0` if unknown). The
/// length companion to [`eh_save_bytes`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_save_len(h: *mut WasmEditHost, seq: f64) -> usize {
    h.as_ref()
        .map(|handle| {
            handle
                .sink
                .borrow()
                .fs_writes
                .get(&(seq.max(0.0) as u64))
                .map_or(0, |s| s.bytes.len())
        })
        .unwrap_or(0)
}

/// Land a finished off-tick OPFS **read** into `buffer`: `kind` is `0` an existing file
/// (`contents` is its UTF-8 text), `1` a not-yet-existing path (new-file buffer), `2` a
/// **directory** (`path` is the canonical dir, `contents` is a JSON array of its entries
/// `[{ "is_dir": bool, "name": str }, …]` → the file-explorer listing), any other a read
/// error (`contents` is the message). Drives the real lifecycle and repaints — see
/// [`EditHost::complete_fs_read`] / [`EditHost::complete_fs_read_dir`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `path` / `contents` valid C strings.
#[no_mangle]
pub unsafe extern "C" fn eh_fs_read_complete(
    h: *mut WasmEditHost,
    buffer: f64,
    path: *const c_char,
    kind: u8,
    contents: *const c_char,
) {
    let Some(handle) = h.as_mut() else { return };
    let buffer = BufferId(buffer.max(0.0) as u64);
    let path = as_str(path).to_string();
    if kind == 2 {
        handle
            .host
            .complete_fs_read_dir(buffer, path, parse_dir_entries(as_str(contents)));
    } else {
        handle
            .host
            .complete_fs_read(buffer, path, kind, as_str(contents));
    }
}

/// Parse a directory listing's entries — a JSON array `[{ "is_dir": bool, "name": str }, …]`
/// the Worker built from the OPFS enumeration — into [`DirEntry`]s for the explorer. A
/// malformed array / entry is skipped rather than failing the whole listing (the explorer
/// degrades to the entries it could read, never a panic across the FFI boundary).
fn parse_dir_entries(json: &str) -> Vec<DirEntry> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|e| {
            let name = e.get("name")?.as_str()?.to_string();
            let is_dir = e.get("is_dir").and_then(|b| b.as_bool()).unwrap_or(false);
            Some(DirEntry { is_dir, name })
        })
        .collect()
}

/// Report a finished off-tick OPFS **write** of `seq`: `ok != 0` finalizes the buffer's
/// saved-state with a [`FileStat`](nxvim_core::FileStat) of `size` bytes + `mtime_ms`
/// (negative = unknown); otherwise it fails loud with `err` and cancels any deferred
/// quit. Removes the in-flight save and repaints — see [`EditHost::complete_fs_write`].
/// `seq` / `size` / `mtime_ms` are `double`s (small values; no 64-bit-int marshalling).
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `err` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_fs_write_complete(
    h: *mut WasmEditHost,
    seq: f64,
    ok: i32,
    size: f64,
    mtime_ms: f64,
    err: *const c_char,
) {
    let Some(handle) = h.as_mut() else { return };
    let Some(save) = handle
        .sink
        .borrow_mut()
        .fs_writes
        .remove(&(seq.max(0.0) as u64))
    else {
        return;
    };
    let mtime = if mtime_ms < 0.0 {
        None
    } else {
        Some(mtime_ms as u64)
    };
    handle
        .host
        .complete_fs_write(save, ok != 0, size.max(0.0) as u64, mtime, as_str(err));
}

/// Execute a Lua chunk through the real effects path (queued `vim.cmd`s and deferred
/// work apply exactly as from the keystroke tick), then project a frame. Returns the
/// result rendered as a string, prefixed `ok:` / `err:`. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `code` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_exec_lua(h: *mut WasmEditHost, code: *const c_char) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("err:null host".into());
    };
    let rendered = match handle.host.exec_lua(as_str(code)) {
        Ok(shown) => format!("ok:{shown}"),
        Err(e) => format!("err:{e}"),
    };
    into_owned_cstr(rendered)
}

/// Return the latest captured `redraw` frame as JSON (`"null"` if none yet) — the real
/// server view projection through the real tick, the 5b proof. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_redraw_json(h: *mut WasmEditHost) -> *mut c_char {
    let json = match h.as_ref() {
        Some(handle) => match &handle.sink.borrow().last_redraw {
            Some(params) => value_to_json(&Value::Array(params.clone())).to_string(),
            None => "null".to_string(),
        },
        None => "null".to_string(),
    };
    into_owned_cstr(json)
}

/// Return the current buffer's lines joined by `\n`. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_lines(h: *mut WasmEditHost) -> *mut c_char {
    match h.as_ref() {
        Some(handle) => into_owned_cstr(handle.host.lines().join("\n")),
        None => into_owned_cstr(String::new()),
    }
}

// ============================================================================
// Persistence (shada) — serverless OPFS. The editor's cross-session state (registers,
// marks, history, jumplist, …) is the pure [`PersistState`] core hands out; the Worker
// serializes it to a single JSON blob in OPFS and restores it at boot. This is the
// browser analogue of `nxvim-server`'s redb store, minus the multi-instance merge a tab
// doesn't need — same snapshot, simpler bytes.
// ============================================================================

/// Apply vim's numbered-mark shift to a freshly-loaded snapshot — the wasm store's
/// load-time step (native does the equivalent in `shada.rs`). A consumed clean-exit
/// cursor becomes `'0`, the prior `'0`–`'8` slide down one, and `'9` drops; with no exit
/// cursor (the tab was hidden/closed without a flush) the set passes through unchanged.
/// Clears `exit_cursor` — it's consumed into `'0`, exactly as the native merged snapshot.
fn shift_numbered_on_load(state: &mut PersistState) {
    let Some(exit) = state.exit_cursor.take() else {
        return;
    };
    let mut by_digit: HashMap<char, NumberedMark> = state
        .numbered_marks
        .drain(..)
        .map(|m| (m.digit, m))
        .collect();
    let mut out = vec![NumberedMark {
        digit: '0',
        path: exit.path,
        line: exit.line,
        col: exit.col,
    }];
    for n in 1u8..=9 {
        let from = (b'0' + n - 1) as char;
        if let Some(m) = by_digit.remove(&from) {
            out.push(NumberedMark {
                digit: (b'0' + n) as char,
                path: m.path,
                line: m.line,
                col: m.col,
            });
        }
    }
    state.numbered_marks = out;
}

/// Serialize the cross-session (shada) snapshot as a JSON blob for the Worker to persist
/// to OPFS. `include_exit != 0` keeps the clean-exit cursor (the flush-on-hide path, so it
/// seeds `'0` next launch); the debounced live checkpoint passes `0` (matching native,
/// where `'0` tracks *exits* only). Empty string on the (practically impossible)
/// serialization failure, so the Worker writes nothing rather than a corrupt/empty blob.
/// Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_export_shada(h: *mut WasmEditHost, include_exit: i32) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(String::new());
    };
    let mut state = handle.host.export_persist();
    if include_exit == 0 {
        state.exit_cursor = None;
    }
    into_owned_cstr(serde_json::to_string(&state).unwrap_or_default())
}

/// Seed the editor from a shada JSON blob the Worker read from OPFS, applying the
/// numbered-mark shift (load is when `'0` ← last-exit cursor). Run between config sourcing
/// and [`eh_boot_finish`], so restored marks / registers / history are live for the first
/// frame. Returns an owned C string: empty on success, else the parse error (the Worker
/// surfaces it, like a bad `init.lua`) — a corrupt blob doesn't brick the session. Caller
/// frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `json` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_load_shada(h: *mut WasmEditHost, json: *const c_char) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(String::new());
    };
    let json = as_str(json);
    if json.is_empty() {
        return into_owned_cstr(String::new());
    }
    match serde_json::from_str::<PersistState>(json) {
        Ok(mut state) => {
            shift_numbered_on_load(&mut state);
            handle.host.import_persist(state);
            into_owned_cstr(String::new())
        }
        Err(e) => into_owned_cstr(format!("shada parse: {e}")),
    }
}

/// Free a string returned by [`eh_exec_lua`] / [`eh_redraw_json`] / [`eh_lines`].
///
/// # Safety
/// `p` must be a pointer previously returned by one of those, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn eh_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Destroy a host from [`eh_new`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn eh_free(h: *mut WasmEditHost) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}
