//! The wasm (emscripten) edit-host — slice 5b of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`.
//!
//! This drives the **real** synchronous [`EditHost`] tick (the same one
//! `nxvim-server`'s native [`run`](nxvim_server) loop drives — core + the PUC Lua 5.1
//! VM + the full server glue: autocmds, mirrors, lifecycle, the redraw projection)
//! behind a wasm [`HostEffects`] ([`WasmEffects`]). It supersedes the throwaway
//! `nxvim-edithost-demo`, which proved only that core+Lua *compile and run* together in
//! wasm via a hand-wired minimal tie-in; here the keystroke path is the production tick.
//!
//! **Interop (emscripten, not wasm-bindgen):** JS→Rust via `ccall`/`cwrap` on the
//! `#[no_mangle] extern "C"` exports below; the redraw goes the other way as a return
//! value (JSON) the JS side reads, rather than a pushed `EM_JS` callback. Slice 5c runs
//! these exports **inside a Web Worker** (`web/worker.mjs`) and ferries the JSON redraw
//! UI-ward over `postMessage`; the UI (`web/index.html`) renders it and exposes the
//! `window.__nxvim` Playwright hook. v1 is **serverless**: there is no daemon, so the
//! off-tick fs / LSP / native-treesitter effects are unreachable and the [`WasmEffects`]
//! impls of them `unreachable!`-loud (never a silent no-op) — see each method.

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::rc::Rc;

use nxvim_core::{BufferId, Editor, PendingSave};
use nxvim_lua::LuaRuntime;
use nxvim_server::{EditHost, HostEffects};
use rmpv::Value;

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
}

/// The wasm [`HostEffects`]: the analogue of `nxvim-server`'s `NativeEffects`, but the
/// "client wire" is the [`Sink`] the JS UI drains instead of msgpack-RPC. The
/// serverless v1 has no daemon, so every off-tick effect is unreachable here (see each
/// method) — the editor tick only reaches them in a daemon session, which the browser
/// build never enters ([`has_remote_fs`](HostEffects::has_remote_fs) is `false`).
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

    fn fs_fetch(&mut self, _buffer: BufferId, _path: String) {
        // Off-tick reads only happen in a daemon session (`has_remote_fs` true); the
        // serverless browser build returns `false` there, so the editor tick never
        // enqueues an open through this seam (`drain_pending_opens` returns early).
        unreachable!("fs_fetch: serverless wasm edit-host has no daemon fs (Phase 6)")
    }

    fn fs_save(&mut self, _save: PendingSave) {
        // Off-tick saves are gated on a daemon fs too (`dispatch_save` checks
        // `has_remote_fs`); a serverless `:w` writes in-process, never crossing this.
        unreachable!("fs_save: serverless wasm edit-host has no daemon fs (Phase 6)")
    }

    fn fs_watch(&mut self, _path: String) {
        // `sync_buffer_watches` is native-only; the wasm build arms no file watches.
        unreachable!("fs_watch: serverless wasm edit-host has no daemon fs (Phase 6)")
    }

    fn fs_unwatch(&mut self, _path: String) {
        unreachable!("fs_unwatch: serverless wasm edit-host has no daemon fs (Phase 6)")
    }

    fn has_remote_fs(&self) -> bool {
        // No daemon in serverless v1 — so the editor tick takes its local branches and
        // never reaches the off-tick fs effects above.
        false
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
/// (slice 5c). 80×24 matches the demo / a conventional terminal.
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
    let fx = Box::new(WasmEffects { sink: sink.clone() });
    let mut host = EditHost::new(Editor::new(), lua, fx);
    // Seed the serverless startup (lifecycle events, mirrors, `v:vim_did_enter`)
    // *before* attaching the UI — the same order the native server uses (startup runs,
    // then a client attaches and triggers the first paint).
    host.boot();
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
