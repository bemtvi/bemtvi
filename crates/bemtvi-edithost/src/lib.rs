//! The wasm (emscripten) edit-host — Phase 5 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`.
//!
//! This drives the **real** synchronous [`EditHost`] tick (the same one
//! `bemtvi-server`'s native [`run`](bemtvi_server) loop drives — core + the PUC Lua 5.4
//! VM + the full server glue: autocmds, mirrors, lifecycle, the redraw projection)
//! behind a wasm [`HostEffects`] ([`WasmEffects`]); the keystroke path is the production
//! tick, not a hand-wired minimal tie-in.
//!
//! **Interop (emscripten, not wasm-bindgen):** JS→Rust via `ccall`/`cwrap` on the
//! `#[no_mangle] extern "C"` exports below; the redraw goes the other way as a return
//! value (JSON) the JS side reads, rather than a pushed `EM_JS` callback. Slice 5c runs
//! these exports **inside a Web Worker** (`web/worker.mjs`) and ferries the JSON redraw
//! UI-ward over `postMessage`; the UI (`web/index.html`) renders it and exposes the
//! `window.__bemtvi` Playwright hook. Slice 5d drives the Worker's run loop off a
//! `SharedArrayBuffer` + `Atomics.wait` park — the same wait that blocks on input also
//! fires Worker-side timers (`vim.defer_fn` / `btv.timer`) via [`eh_set_clock`] /
//! [`eh_next_deadline`] / [`eh_tick_timers`], the wheel `evloop.rs` can't provide
//! in-Worker. **Phase 6 (serverless OPFS):** files live in the browser's Origin Private
//! File System. There is no daemon, but OPFS handle acquisition is *async* (only a
//! `FileSystemSyncAccessHandle`'s operations are sync), so a synchronous [`HostFs`] is
//! impossible without Asyncify — instead `:e` / `:w` route through the *same off-tick
//! seam* a daemon session uses ([`HostEffects::fs_fetch`] / [`HostEffects::fs_save`]),
//! and the Worker fulfills them against OPFS between ticks ([`eh_take_fs_requests`] →
//! [`eh_fs_read_complete`] / [`eh_fs_write_complete`]). **Tree-sitter indentation** is wired
//! through the [`WasmSyntax`] engine, which calls the worker's web-tree-sitter indenter
//! synchronously over the `eh_js_ts_*` FFI bridge (highlighting stays a UI-thread overlay).
//! LSP / process spawn remain unavailable and fail loud (a later daemon slice re-enables them).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::rc::Rc;

use bemtvi_core::{
    BufferEdit, BufferId, Clipboard, DirEntry, Editor, FoldRange, IndentParams, NumberedMark,
    OpenOutcome, PendingSave, PersistState, Span, SyntaxEngine,
};
use bemtvi_lsp::{
    ApplyEditOutcome, LspEvent, LspNotify, LspRequest, ReqToken, ServerKey, ServerSpawn,
    SyncLspClient, WireOp,
};
use bemtvi_lua::LuaRuntime;
use bemtvi_server::{decode_config_bundle_bytes, EditHost, HostEffects};
use rmpv::Value;

// The synchronous Rust→JS treesitter-indent bridge (web/eh-lib.js, linked by build.sh's
// `--js-library`). These are the one place the editor tick calls *into* JS synchronously
// — indentation is decided mid-keystroke, so it can't ride the off-tick `Sink` seam every
// other effect uses. The worker installs the backing functions (its web-tree-sitter
// indenter, web/ts-indent.js) on `globalThis`; with none installed (the Node harness),
// they degrade to "no ts indent" (-1 / 0) and the core falls back. See [`WasmSyntax`].
extern "C" {
    /// Target indent width in columns for 0-indexed `line` of `text` (NUL-terminated
    /// UTF-8) in `lang`, given the resolved `sw`/`ts`; `-1` to fall back.
    fn eh_js_ts_indent(
        lang: *const c_char,
        text: *const c_char,
        line: i32,
        sw: i32,
        ts: i32,
    ) -> i32;
    /// `1` if a grammar with an `indents.scm` is loaded for `lang`, else `0`.
    fn eh_js_ts_available(lang: *const c_char) -> i32;
    /// Drop `lang`'s cached grammar after a `:TSInstall`, so the next query reloads it.
    fn eh_js_ts_reload(lang: *const c_char);
    /// Foldable line ranges for `text` in `lang`, written into the `cap`-int `out` buffer
    /// as flat `[start, end, …]` 0-based inclusive pairs. Returns the total number of i32s
    /// needed (may exceed `cap` — the caller grows + retries); `-1` if no tree-sitter folds
    /// are available (no runner / grammar loading / no `folds.scm`).
    fn eh_js_ts_folds(lang: *const c_char, text: *const c_char, out: *mut i32, cap: i32) -> i32;
    /// `1` if a grammar with a `folds.scm` is loaded for `lang`, else `0`.
    fn eh_js_ts_folds_available(lang: *const c_char) -> i32;
    /// Byte ranges of `text`'s `textobjects.scm` nodes captured as `capture` (e.g.
    /// `function.inner`) that contain byte offset `byte`, written into the `cap`-int
    /// `out` buffer as flat `[start, end, …]` **byte** pairs, innermost (smallest
    /// span) first. Returns the total number of i32s needed (may exceed `cap` — the
    /// caller grows + retries); `-1` if no tree-sitter text objects are available (no
    /// runner / grammar loading / no `textobjects.scm`). The JS side converts between
    /// the core's UTF-8 byte offsets and web-tree-sitter's UTF-16 units.
    fn eh_js_ts_textobjects(
        lang: *const c_char,
        text: *const c_char,
        capture: *const c_char,
        byte: i32,
        out: *mut i32,
        cap: i32,
    ) -> i32;
    /// `1` if a grammar with a `textobjects.scm` is loaded for `lang`, else `0`.
    fn eh_js_ts_textobjects_available(lang: *const c_char) -> i32;
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
    /// Non-`redraw` notifications (`bemtvi_exit`, scripted `bemtvi_panel_select`, …) in
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
    /// File paths the editor newly asked to **watch** this convergence (the remote watch
    /// leg — Phase 6 watch slice), recorded by [`fs_watch`](HostEffects::fs_watch). Each carries
    /// the editor's disk baseline (`Some` once the file was read) so a re-dialed daemon can
    /// detect a change made *during* an outage on re-arm (the reconnect re-stat — Phase 7). Drained
    /// by [`eh_take_watch_requests`] for the Worker to arm on the daemon (`fs_watch [path, known?]`
    /// over WebTransport); a serverless OPFS session has no change source, so the Worker
    /// drops them. A `fs_changed` push the arm yields lands back via [`eh_remote_file_changed`].
    watch_arms: Vec<(String, Option<bemtvi_core::FileStat>)>,
    /// File paths the editor newly asked to **stop watching** (a buffer closed / lost its
    /// file), recorded by [`fs_unwatch`](HostEffects::fs_unwatch); the disarm twin of
    /// [`watch_arms`](Sink::watch_arms), drained by [`eh_take_watch_requests`].
    watch_disarms: Vec<String>,
    /// Async process spawns the editor enqueued this convergence (the proc leg — Phase 6d):
    /// `(id, argv, cwd, env, stdin)` per `vim.system` / `jobstart` with an `on_exit`,
    /// recorded by [`proc_spawn`](HostEffects::proc_spawn). Drained by
    /// [`eh_take_proc_requests`] for the Worker to forward over WebTransport
    /// (`proc_spawn [id, argv, cwd?, env, stdin]`); the child's pid/exit return via
    /// [`eh_proc_spawned`] / [`eh_proc_exited`]. Only ever enqueued when a process host is
    /// present (the tick gates on [`proc_host`](Sink::proc_host)).
    #[allow(clippy::type_complexity)]
    proc_spawns: Vec<(
        u64,
        Vec<String>,
        Option<String>,
        Vec<(String, String)>,
        Vec<u8>,
        bool,
    )>,
    /// Ids the editor asked to **kill** (`handle:kill()`), recorded by
    /// [`proc_kill`](HostEffects::proc_kill); the disarm twin of
    /// [`proc_spawns`](Sink::proc_spawns), drained by [`eh_take_proc_requests`] for the
    /// Worker to forward (`proc_kill [id]`).
    proc_kills: Vec<u64>,
    /// Off-tick `btv.fs` ops the editor enqueued this convergence (the `luafs_op` leg —
    /// Phase 2): `(cb_id, job)` per `btv.fs.*` call, recorded by [`fs_op`](HostEffects::fs_op).
    /// Drained by [`eh_take_fs_op_requests`]; the Worker routes each to the daemon `luafs_op`
    /// leg over WebTransport when connected, else to OPFS (Phase 3a, serverless), and lands the
    /// typed result via [`eh_fs_op_result`]. Always enqueued on wasm (there is always *some* fs —
    /// OPFS is the serverless fallback), unlike the daemon-only proc leg.
    fs_ops: Vec<(u64, bemtvi_lua::FsJob, bool)>,
    /// Off-tick `btv.git.*` ops the editor enqueued this convergence (the `git_op` leg):
    /// `(cb_id, job, local)` per `btv.git.*` call, recorded by [`git_op`](HostEffects::git_op).
    /// Drained by [`eh_take_git_op_requests`]; the Worker routes each to the daemon `git_op`
    /// leg over WebTransport and lands the typed result via [`eh_git_op_result`]. Only enqueued
    /// when a daemon is connected — there is no in-browser git engine, so a serverless session
    /// rejects the op loud in the tick (the editor never records it here).
    git_ops: Vec<(u64, bemtvi_lua::GitJob, bool)>,
    /// Off-tick `btv.http.fetch` requests the editor enqueued this convergence (the `http_op`
    /// leg): `(cb_id, request)` per `btv.http.fetch`, recorded by [`http_op`](HostEffects::http_op).
    /// Drained by [`eh_take_http_requests`]; the Worker routes each to the daemon `http_op` leg
    /// over WebTransport when connected, else runs the browser's own `fetch()` (serverless — the
    /// browser always has one, so this is always enqueable, no host gate), and lands the typed
    /// result via [`eh_http_result`].
    /// Each is `(id, request, local)` — `local` (`btv.http.fetch_local`) forces the browser's
    /// own `fetch()` even when a daemon is connected (the Worker routes on it).
    http_ops: Vec<(u64, bemtvi_lua::HttpRequest, bool)>,
    /// `btv.http.mount` publications the editor enqueued this convergence: `(cb_id, name)` per
    /// mount, recorded by [`http_mount`](HostEffects::http_mount). Drained by
    /// [`eh_take_http_mounts`]; the Worker registers the Service Worker that intercepts
    /// `/plugin/*` on the page's origin and lands the bound origin (or the reason there is
    /// none) via [`eh_http_mount_result`].
    http_mounts: Vec<(u64, String)>,
    /// `mount:close()` retirements enqueued this convergence: the mount's `cb_id`. Drained by
    /// [`eh_take_http_unmounts`]. The route itself is already gone from the editor's table (so
    /// a later request 404s); this only lets the Worker forget its own per-mount bookkeeping.
    http_unmounts: Vec<u64>,
    /// Mount-handler replies enqueued this convergence: `(req_id, reply)` per `respond(res)`,
    /// recorded by [`http_respond`](HostEffects::http_respond). Drained by
    /// [`eh_take_http_server_replies`]; the Worker relays each to the window, which posts it
    /// down the Service Worker's `MessageChannel` port and completes the browser's request.
    http_server_replies: Vec<(u64, bemtvi_lua::HttpServerReply)>,
    /// Streaming `btv.fs.watch` arms the editor enqueued this convergence (the `luafs_watch` leg —
    /// Phase 3b): `(stream_id, path, recursive)` per `btv.fs.watch`, recorded by
    /// [`fs_watch_stream`](HostEffects::fs_watch_stream). Drained by [`eh_take_fs_watch_requests`]
    /// for the Worker to forward over WebTransport (`luafs_watch [id, path, recursive]`); change
    /// batches return via [`eh_fs_watch_change`] / errors via [`eh_fs_watch_err`]. Daemon-only
    /// (the tick gates the watch on a connected daemon; serverless fails it loud — no change source).
    fs_watch_arms: Vec<(u64, String, bool)>,
    /// Streaming-watch `:stop()`s the editor enqueued (the disarm twin of
    /// [`fs_watch_arms`](Sink::fs_watch_arms)), recorded by
    /// [`fs_unwatch_stream`](HostEffects::fs_unwatch_stream); drained by
    /// [`eh_take_fs_watch_requests`] (`luafs_unwatch [id]`).
    fs_watch_disarms: Vec<u64>,
    /// Terminal PTYs the editor newly asked to **open** this convergence (the web `:terminal`
    /// — Phase 7): `(buf, argv, cwd, rows, cols)` per `:terminal`, recorded by
    /// [`term_open`](HostEffects::term_open). Drained by [`eh_take_terminal_requests`] for the
    /// Worker to forward over WebTransport (`term_open [buf, argv, cwd?, rows, cols]`); the
    /// child's output/exit return as `term_data`/`term_exit` pushes (`eh_terminal_data` /
    /// `eh_terminal_exit`). Only enqueued when a process host is present (the dispatch gates
    /// on [`proc_host`](Sink::proc_host); a session with no host has no PTY host).
    #[allow(clippy::type_complexity)]
    term_opens: Vec<(u64, Vec<String>, Option<String>, u16, u16)>,
    /// Input bytes the editor asked to **write** to a terminal PTY (a forwarded keystroke /
    /// paste / vt100 query reply), recorded by [`term_write`](HostEffects::term_write);
    /// `(buf, bytes)`, drained by [`eh_take_terminal_requests`] (`term_write [buf, bytes]`).
    term_writes: Vec<(u64, Vec<u8>)>,
    /// Terminal **resizes** the editor enqueued (the window's text area changed), recorded by
    /// [`term_resize`](HostEffects::term_resize); `(buf, rows, cols)`, drained as
    /// `term_resize [buf, rows, cols]` so the daemon PTY reflows.
    term_resizes: Vec<(u64, u16, u16)>,
    /// Terminal PTYs the editor asked to **kill** (the terminal closed), recorded by
    /// [`term_kill`](HostEffects::term_kill); the close twin of [`term_opens`](Sink::term_opens),
    /// drained as `term_kill [buf]`.
    term_kills: Vec<u64>,
    /// Terminals a `^C` just trimmed as a flood-cancel, recorded by
    /// [`term_interrupted`](HostEffects::term_interrupted); drained in the terminal-requests
    /// JSON (`interrupt`) so the Worker discards the child's in-flight backlog.
    term_interrupts: Vec<u64>,
    /// Whether a **process host** is currently available — either a daemon over WebTransport
    /// *or* a local in-browser Worker host (Pyodide interpreter / basedpyright LSP). Flipped
    /// by the Worker via [`eh_set_proc_host`] on a `?daemon=` boot / runtime `:connect` /
    /// local-host bring-up / disconnect. Read by
    /// [`has_remote_proc`](HostEffects::has_remote_proc) /
    /// [`has_remote_lsp`](HostEffects::has_remote_lsp) to gate the editor's async-spawn /
    /// terminal / LSP branches: a session with no process host (serverless OPFS, no local
    /// host) must fail those loud in the tick, never silently enqueue a request no host can
    /// fulfil. ("remote" in the gate names means *out-of-core / off-tick*, which a local
    /// Worker host equally is — not specifically a daemon.)
    proc_host: bool,
    /// Treesitter grammars the editor newly asked to **install** this convergence (one
    /// per `:TSInstall <lang>`), recorded by [`ts_install`](HostEffects::ts_install).
    /// Drained by [`eh_take_ts_requests`] for the Worker to forward to the UI thread,
    /// which fetches the prebuilt `.wasm` + queries (offline bundle / OPFS / jsDelivr),
    /// caches + registers them with the JS highlighter, and lands the outcome back via
    /// [`eh_ts_install_complete`]. Browser-side fetch, not a native compile.
    ts_requests: Vec<String>,
    /// The browser clipboard mirror backing the `"+` / `"*` registers. `clipboard_get` is
    /// the value the UI last pushed in from `navigator.clipboard` ([`eh_clipboard_push`],
    /// refreshed on focus / paste); a `"+p` reads it. `clipboard_writes` queues each `"+` /
    /// `"*` yank or delete for the Worker to drain ([`eh_take_clipboard_writes`]) and write
    /// out to `navigator.clipboard`. Text is stored *verbatim* — linewise-ness rides the
    /// trailing `\n`, exactly as `bemtvi-server`'s native `SystemClipboard` (pbcopy/pbpaste)
    /// treats it — so a value agrees whether read back in-editor or after a round trip
    /// through the OS clipboard. Written by [`WasmClipboard`] (the editor owns it).
    clipboard_get: Option<String>,
    clipboard_writes: Vec<String>,
    /// Raw LSP [`WireOp`]s the [`SyncLspClient`] produced this convergence (the LSP leg —
    /// Phase 6e): `Spawn` / `Stdin` / `Kill`, moved here by [`WasmEffects::flush_lsp_wire`]
    /// after every client interaction (an outbound `ensure`/`notify`/`request` *or* an
    /// inbound `feed_stdout` — a handshake completion / config pull can emit ops at any
    /// point). Drained by [`eh_take_lsp_requests`] for the Worker to forward over
    /// WebTransport (`lsp_spawn` / `lsp_stdin` / `lsp_kill`); the daemon's `lsp_stdout` /
    /// `lsp_stderr` / `lsp_exited` pushes land back via [`eh_lsp_stdout`] / [`eh_lsp_stderr`]
    /// / [`eh_lsp_exited`]. Daemon-only (the editor gates `vim.lsp.start` on a connected
    /// daemon via [`has_remote_lsp`](HostEffects::has_remote_lsp); serverless fails it loud).
    lsp_ops: Vec<WireOp>,
    /// Duplex `btv.process` children the editor newly asked to **open** this convergence
    /// (the `dproc_*` leg — the DAP / framed-protocol transport): `(id, argv, cwd, env)`,
    /// recorded by [`dproc_open`](HostEffects::dproc_open). Drained by
    /// [`eh_take_dproc_requests`] for the Worker to forward (`dproc_open [id, argv, cwd?,
    /// env]`); raw stdout/stderr return via [`eh_dproc_out`], the exit via [`eh_dproc_exit`].
    /// Daemon-only (a duplex child has no serverless fallback).
    #[allow(clippy::type_complexity)]
    dproc_opens: Vec<(u64, Vec<String>, Option<String>, Vec<(String, String)>)>,
    /// Stdin bytes the editor asked to write to a duplex child (`handle:write`), recorded
    /// by [`dproc_write`](HostEffects::dproc_write); `(id, bytes)`, drained as `dproc_write`.
    dproc_writes: Vec<(u64, Vec<u8>)>,
    /// Duplex children the editor asked to **kill** (`handle:kill`), recorded by
    /// [`dproc_kill`](HostEffects::dproc_kill); drained as `dproc_kill [id]`.
    dproc_kills: Vec<u64>,
    /// `btv.socket` connections the editor newly asked to **open** this convergence (the
    /// `sock_*` leg — a DAP `type="server"` adapter transport): `(id, host, port)`,
    /// recorded by [`sock_connect`](HostEffects::sock_connect). Drained by
    /// [`eh_take_sock_requests`]; `connected`/data/`closed` return via [`eh_sock_connected`]
    /// / [`eh_sock_data`] / [`eh_sock_closed`]. Daemon-only.
    sock_connects: Vec<(u64, String, u16)>,
    /// Bytes the editor asked to send over a connection (`handle:write`), recorded by
    /// [`sock_write`](HostEffects::sock_write); `(id, bytes)`, drained as `sock_write`.
    sock_writes: Vec<(u64, Vec<u8>)>,
    /// Connections the editor asked to **close** (`handle:close`), recorded by
    /// [`sock_close`](HostEffects::sock_close); drained as `sock_close [id]`.
    sock_closes: Vec<u64>,
}

/// The `"+` / `"*` clipboard provider for the browser build — the wasm twin of
/// `bemtvi-server`'s `SystemClipboard` (which shells out to pbcopy/pbpaste). The synchronous
/// [`Clipboard`] seam can't await `navigator.clipboard` (it's async, and unreachable off the
/// UI thread anyway), so it bridges through the [`Sink`]: [`get`](Clipboard::get) returns the
/// value the UI last pushed in ([`eh_clipboard_push`]), and [`set`](Clipboard::set) updates
/// that mirror *and* queues the text for the UI to write out ([`eh_take_clipboard_writes`]).
struct WasmClipboard {
    sink: Rc<RefCell<Sink>>,
}

// SAFETY: the entire wasm edit-host runs on the single Web Worker thread, so the `Rc` is
// never sent across threads. The `Send` bound on `Clipboard` exists for `bemtvi-server`'s
// native worker thread and is never exercised on this build — the same single-thread
// justification the `Rc`-holding `WasmEffects` relies on (`HostEffects` is itself `!Send`).
unsafe impl Send for WasmClipboard {}

impl Clipboard for WasmClipboard {
    fn get(&self) -> Option<(String, bool)> {
        // Linewise iff the text ends in `\n`, mirroring the native `SystemClipboard` — a
        // linewise yank kept its trailing newline, so reading it back re-derives the flag.
        self.sink
            .borrow()
            .clipboard_get
            .as_ref()
            .map(|text| (text.clone(), text.ends_with('\n')))
    }

    fn set(&self, text: &str, _linewise: bool) {
        // Store verbatim (a linewise yank already carries its `\n`); the UI hands the same
        // bytes to `navigator.clipboard`, and `get` re-derives linewise from the newline.
        let mut sink = self.sink.borrow_mut();
        sink.clipboard_get = Some(text.to_string());
        sink.clipboard_writes.push(text.to_string());
    }
}

/// One opened buffer's state the [`WasmSyntax`] engine keeps: the language it parses as
/// and a **shadow copy of the text**, patched by the editor's edit deltas. The shadow is
/// what gets handed to the JS indenter on a query — the trait's contract is that the
/// engine owns its own text (so its methods never borrow the editor's buffers).
struct WasmBufState {
    language: String,
    /// Full buffer text (the rope keeps a trailing `\n`, so this does too).
    shadow: String,
}

/// The wasm [`SyntaxEngine`]: the browser twin of `bemtvi-ts`'s native `Engine`, but it
/// owns **no parser** — web-tree-sitter (the wasm tree-sitter runtime) lives in JS, not in
/// this Rust wasm module. So highlighting is handled entirely UI-side (`web/highlight.js`,
/// a paint overlay, never routed through this engine), and this engine exists purely for
/// **indentation**, which — unlike highlighting — the core must decide *synchronously
/// inside the tick*. On [`indent`](SyntaxEngine::indent) it hands the shadow text to the
/// worker's web-tree-sitter indenter through the [`eh_js_ts_indent`] FFI bridge and returns
/// its verdict; the worker keeps the grammars + `indents.scm` and runs the ported
/// nvim-treesitter algorithm (`web/ts-indent.js`).
///
/// SAFETY of the `Send` bound `SyntaxEngine` does not require: this holds only owned data
/// (no `Rc`), and the whole edit-host runs on the single Worker thread anyway.
#[derive(Default)]
struct WasmSyntax {
    buffers: HashMap<BufferId, WasmBufState>,
}

impl WasmSyntax {
    /// Borrow a buffer's language as a C string for the FFI bridge (empty on a buffer the
    /// engine never opened — the JS side maps that to "no grammar" → fall back).
    fn lang_cstr(language: &str) -> Option<CString> {
        CString::new(language).ok()
    }

    /// The C-string language for an opened `buffer` (the FFI-bridge query key), or `None`
    /// when the engine never opened that buffer / its language won't form a C string —
    /// every grammar-availability check and indent/fold query starts here.
    fn lang_for(&self, buffer: BufferId) -> Option<CString> {
        Self::lang_cstr(&self.buffers.get(&buffer)?.language)
    }
}

impl SyntaxEngine for WasmSyntax {
    fn open(&mut self, buffer: BufferId, language: &str, text: &str) -> OpenOutcome {
        // Just snapshot the text + language; the parser is JS-side. A grammar that isn't
        // available there is the JS indenter's silent-fallback case, never a load failure
        // surfaced here — so always `Ok` (matches the wasm build's best-effort highlighting).
        self.buffers.insert(
            buffer,
            WasmBufState {
                language: language.to_string(),
                shadow: text.to_string(),
            },
        );
        OpenOutcome::Ok
    }

    fn edit(&mut self, buffer: BufferId, edits: &[BufferEdit]) {
        let Some(state) = self.buffers.get_mut(&buffer) else {
            return;
        };
        // Patch the shadow with each byte-range replacement, defending against a malformed
        // delta (out-of-range / mid-codepoint) so a bad edit degrades to a stale shadow
        // rather than a panic across the FFI boundary.
        for e in edits {
            let len = state.shadow.len();
            if e.start_byte > e.old_end_byte
                || e.old_end_byte > len
                || !state.shadow.is_char_boundary(e.start_byte)
                || !state.shadow.is_char_boundary(e.old_end_byte)
            {
                continue;
            }
            state
                .shadow
                .replace_range(e.start_byte..e.old_end_byte, &e.text);
        }
    }

    fn close(&mut self, buffer: BufferId) {
        self.buffers.remove(&buffer);
    }

    fn reload_grammar(&mut self, lang: &str) {
        // A `:TSInstall <lang>` just landed: tell the JS indenter to evict its cached
        // grammar so the next query reloads the freshly installed parser + indents.scm.
        if let Some(c) = Self::lang_cstr(lang) {
            unsafe { eh_js_ts_reload(c.as_ptr()) };
        }
    }

    fn highlights(&mut self, _buffer: BufferId, _first: usize, _last: usize) -> Vec<Span> {
        // Highlighting on the browser build is a UI-thread paint overlay (web/highlight.js),
        // never routed through the core engine — so this is always empty (the redraw path
        // doesn't even call it on wasm; `refresh_highlights` is native-only).
        Vec::new()
    }

    fn indent(&mut self, buffer: BufferId, line: usize, p: &IndentParams) -> Option<usize> {
        let state = self.buffers.get(&buffer)?;
        let lang = Self::lang_cstr(&state.language)?;
        let text = CString::new(state.shadow.as_str()).ok()?;
        let width = unsafe {
            eh_js_ts_indent(
                lang.as_ptr(),
                text.as_ptr(),
                line as i32,
                p.shiftwidth as i32,
                p.tabstop as i32,
            )
        };
        // `-1` is the JS side's fallback signal (grammar still loading, no grammar / no
        // indents.scm, or an inconclusive `@indent.auto` query). A real verdict is `>= 0`.
        (width >= 0).then_some(width as usize)
    }

    fn indents_available(&self, buffer: BufferId) -> bool {
        self.lang_for(buffer)
            .is_some_and(|lang| unsafe { eh_js_ts_available(lang.as_ptr()) != 0 })
    }

    fn folds(&mut self, buffer: BufferId) -> Vec<FoldRange> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let (Some(lang), Ok(text)) = (
            Self::lang_cstr(&state.language),
            CString::new(state.shadow.as_str()),
        ) else {
            return Vec::new();
        };
        // The worker writes flat `[start, end, …]` i32 pairs into `buf`; it returns the
        // total count needed, which may exceed `buf.len()` (a fold-dense file) — grow and
        // retry once in that case. `1024` ints = 512 folds covers the common case.
        //
        // Defence-in-depth: `needed` is a count the JS folds runner returns (driven by the
        // buffer text, which can be an untrusted file). Cap the retry allocation so a
        // pathologically fold-dense buffer — or a buggy/hostile runner returning an inflated
        // count — can't size a multi-GB `Vec` and abort the whole wasm module (the 32-bit
        // address space tops out at 4 GB). Folds are cosmetic, so we degrade to the ranges
        // that fit rather than OOM, mirroring this method's existing "return Vec::new() when
        // unavailable" graceful-fallback shape. `1 << 20` ints = 512K fold ranges (~4 MB) is
        // far beyond any real file.
        const MAX_FOLD_INTS: usize = 1 << 20;
        let mut buf = vec![0i32; 1024];
        loop {
            let needed = unsafe {
                eh_js_ts_folds(
                    lang.as_ptr(),
                    text.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                )
            };
            if needed < 0 {
                return Vec::new(); // no tree-sitter folds available
            }
            let needed = needed as usize;
            if needed <= buf.len() {
                buf.truncate(needed);
                break;
            }
            if buf.len() >= MAX_FOLD_INTS {
                // Already at the cap; keep the ranges that fit (JS wrote the first `cap`).
                break;
            }
            buf = vec![0i32; needed.min(MAX_FOLD_INTS)];
        }
        buf.chunks_exact(2)
            .map(|c| FoldRange {
                start: c[0].max(0) as usize,
                end: c[1].max(0) as usize,
            })
            .collect()
    }

    fn folds_available(&self, buffer: BufferId) -> bool {
        self.lang_for(buffer)
            .is_some_and(|lang| unsafe { eh_js_ts_folds_available(lang.as_ptr()) != 0 })
    }

    fn text_objects_at(
        &mut self,
        buffer: BufferId,
        capture: &str,
        byte: usize,
    ) -> Vec<(usize, usize)> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let (Some(lang), Ok(text), Ok(capture)) = (
            Self::lang_cstr(&state.language),
            CString::new(state.shadow.as_str()),
            CString::new(capture),
        ) else {
            return Vec::new();
        };
        // Same grow-and-retry protocol as `folds`: the worker writes flat `[start, end,
        // …]` byte pairs and returns the count needed; grow once past the initial 512
        // ranges if a query is unusually rich. The `MAX` cap defends against a hostile
        // runner returning an inflated count sizing a multi-GB `Vec` (32-bit wasm).
        const MAX_INTS: usize = 1 << 20;
        let mut buf = vec![0i32; 1024];
        loop {
            let needed = unsafe {
                eh_js_ts_textobjects(
                    lang.as_ptr(),
                    text.as_ptr(),
                    capture.as_ptr(),
                    byte as i32,
                    buf.as_mut_ptr(),
                    buf.len() as i32,
                )
            };
            if needed < 0 {
                return Vec::new(); // no tree-sitter text objects available
            }
            let needed = needed as usize;
            if needed <= buf.len() {
                buf.truncate(needed);
                break;
            }
            if buf.len() >= MAX_INTS {
                break;
            }
            buf = vec![0i32; needed.min(MAX_INTS)];
        }
        buf.chunks_exact(2)
            .map(|c| (c[0].max(0) as usize, c[1].max(0) as usize))
            .collect()
    }

    fn text_objects_available(&self, buffer: BufferId) -> bool {
        self.lang_for(buffer)
            .is_some_and(|lang| unsafe { eh_js_ts_textobjects_available(lang.as_ptr()) != 0 })
    }

    fn set_query(&mut self, _lang: &str, _name: &str, _text: Option<String>) -> Result<(), String> {
        // The browser indenter sources its `indents.scm` from fixed assets (the offline
        // bundle / the OPFS `:TSInstall` cache), not from an engine-held query store, so a
        // runtime `query.set` override has nothing to install here — the same way the wasm
        // highlighter (web/highlight.js) uses its own sanitized queries and ignores
        // overrides. Reported as a no-op success rather than a failure so buffer-open's
        // query resolution doesn't echo a spurious error every time.
        Ok(())
    }

    fn set_query_overlay(
        &mut self,
        _lang: &str,
        _name: &str,
        _text: Option<String>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// The wasm [`HostEffects`]: the analogue of `bemtvi-server`'s `NativeEffects`, but the
/// "client wire" is the [`Sink`] the JS UI drains instead of msgpack-RPC. The editor
/// runs in **off-tick fs** mode ([`has_remote_fs`](HostEffects::has_remote_fs) is `true`)
/// because OPFS is async to open: `:e` / `:w` record their read/write into the [`Sink`]
/// for the Worker to fulfill against OPFS between ticks. The remaining off-tick effects
/// (LSP, native treesitter, watch) stay unreachable on this build (see each method).
struct WasmEffects {
    sink: Rc<RefCell<Sink>>,
    /// The synchronous LSP client (Phase 6e) — the wasm twin of `bemtvi-server`'s async
    /// `LspManager`. Drives N language servers over the daemon's raw `lsp_*` wire: the
    /// editor's [`lsp_ensure`](HostEffects::lsp_ensure) / [`lsp_notify`](HostEffects::lsp_notify)
    /// / [`lsp_request`](HostEffects::lsp_request) feed it, its outbound [`WireOp`]s drain to
    /// the [`Sink`] for the Worker to forward, and the daemon's `lsp_stdout` / `lsp_exited`
    /// pushes feed back in via [`lsp_stdout`](HostEffects::lsp_stdout) /
    /// [`lsp_exited`](HostEffects::lsp_exited). Lives here (not the `Sink`) because only the
    /// editor thread touches it — the FFI reaches it through the [`EditHost`] → effects path,
    /// never directly.
    lsp: SyncLspClient,
}

impl WasmEffects {
    /// Move the LSP client's freshly-produced wire ops into the [`Sink`] for the Worker to
    /// forward to the daemon. Called after *every* client interaction — an outbound
    /// `ensure`/`notify`/`request` enqueues a `Spawn`/`Stdin`, and an inbound `feed_stdout`
    /// can too (the `initialize` reply flushes the queued `initialized` + `didOpen`, a
    /// `workspace/configuration` pull replies inline) — so the drain can't wait for the tick.
    fn flush_lsp_wire(&mut self) {
        let ops = self.lsp.take_wire_ops();
        if !ops.is_empty() {
            self.sink.borrow_mut().lsp_ops.extend(ops);
        }
    }
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

    fn fs_watch(&mut self, path: String, known: Option<bemtvi_core::FileStat>) {
        // The remote watch leg (Phase 6): record the arm for the Worker to forward to the
        // daemon (`fs_watch [path, known?]` over WebTransport) — the wasm twin of the native
        // `sync_buffer_watches` arming a watch. A serverless OPFS session has no external
        // writer, so the Worker drops it; either way the editor arms uniformly. `known` is the
        // buffer's disk baseline: a re-dialed daemon (which lost its own baselines) compares it
        // to the live file at arm time and pushes a change made *during* the outage (the
        // reconnect re-stat — Phase 7), so an outage-window edit isn't silently re-baselined.
        self.sink.borrow_mut().watch_arms.push((path, known));
    }

    fn fs_unwatch(&mut self, path: String) {
        // The disarm twin of `fs_watch` (a buffer closed / lost its file): record it for the
        // Worker to drop the daemon watch (`fs_unwatch [path]`).
        self.sink.borrow_mut().watch_disarms.push(path);
    }

    fn has_remote_fs(&self) -> bool {
        // OPFS is an *off-tick* fs (its handle acquisition is async), so the editor tick
        // takes the off-tick `:e`/`:w` branches — `fs_fetch` / `fs_save` above — exactly
        // as a daemon session does, only the transport is OPFS instead of the wire.
        true
    }

    fn proc_spawn(
        &mut self,
        id: u64,
        cmd: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdin: Vec<u8>,
        stream: bool,
    ) {
        // An async `vim.system` / `jobstart` against the daemon (Phase 6d): record the
        // spawn for the Worker to forward over WebTransport (`eh_take_proc_requests` →
        // `proc_spawn`). The child's pid/exit return via `eh_proc_spawned` / `eh_proc_exited`
        // — the wasm twin of the daemon's `proc_spawned`/`proc_exited` pushes. A streaming
        // spawn (`btv.run_stream`'s streamed stdout, e.g. a picker source) also streams stdout back
        // inbound via `eh_proc_stdout`. Only reached when a daemon is connected (the tick
        // gates on `has_remote_proc`).
        self.sink
            .borrow_mut()
            .proc_spawns
            .push((id, cmd, cwd, env, stdin, stream));
    }

    fn proc_kill(&mut self, id: u64) {
        // `handle:kill()` on a daemon child: record the kill for the Worker to forward
        // (`proc_kill [id]`); the resulting exit returns inbound on `eh_proc_exited`.
        self.sink.borrow_mut().proc_kills.push(id);
    }

    fn has_remote_proc(&self) -> bool {
        // A `vim.system` / `:terminal` needs a process host — a connected daemon OR a local
        // in-browser Worker host (Pyodide). A session with neither has nowhere to run a
        // process. The Worker flips this via `eh_set_proc_host` on `:connect` / `?daemon=` /
        // local-host bring-up; when false the tick fails the spawn loud instead of enqueuing it.
        self.sink.borrow().proc_host
    }

    fn dproc_open(
        &mut self,
        id: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) {
        // A duplex `btv.process` child against the daemon (the DAP transport): record the
        // open for the Worker to forward (`dproc_open`); its raw stdout/stderr return via
        // `eh_dproc_out`, the exit via `eh_dproc_exit`.
        self.sink
            .borrow_mut()
            .dproc_opens
            .push((id, argv, cwd, env));
    }

    fn dproc_write(&mut self, id: u64, bytes: Vec<u8>) {
        self.sink.borrow_mut().dproc_writes.push((id, bytes));
    }

    fn dproc_kill(&mut self, id: u64) {
        self.sink.borrow_mut().dproc_kills.push(id);
    }

    fn sock_connect(&mut self, id: u64, host: String, port: u16) {
        // An `btv.socket` connection against the daemon (a DAP `type="server"` transport):
        // record the connect for the Worker to forward (`sock_connect`); `connected`/data/
        // `closed` return via `eh_sock_connected` / `eh_sock_data` / `eh_sock_closed`.
        self.sink.borrow_mut().sock_connects.push((id, host, port));
    }

    fn sock_write(&mut self, id: u64, bytes: Vec<u8>) {
        self.sink.borrow_mut().sock_writes.push((id, bytes));
    }

    fn sock_close(&mut self, id: u64) {
        self.sink.borrow_mut().sock_closes.push(id);
    }

    fn fs_op(&mut self, id: u64, job: bemtvi_lua::FsJob, local: bool) {
        // An off-tick `btv.fs` op (Phase 2): record the (cb_id, job, local) for the Worker to
        // fulfill (`eh_take_fs_op_requests`). A normal op routes to the daemon `luafs_op` leg
        // (connected) else OPFS (serverless); a `local`-flagged op (the plugin manager's
        // discover / source) ALWAYS routes to the local OPFS store, never the daemon —
        // plugin management is local (see the plan doc). The typed result returns via
        // `eh_fs_op_result`.
        self.sink.borrow_mut().fs_ops.push((id, job, local));
    }

    fn git_op(&mut self, id: u64, job: bemtvi_lua::GitJob, local: bool) {
        // An off-tick `btv.git` op: record the (cb_id, job, local) for the Worker to route to
        // the daemon `git_op` leg (`eh_take_git_op_requests`). Only reached when a daemon is
        // connected — the editor tick rejects a serverless git op loud before it gets here
        // (there is no in-browser git engine). The typed result returns via `eh_git_op_result`.
        self.sink.borrow_mut().git_ops.push((id, job, local));
    }

    fn http_op(&mut self, id: u64, request: bemtvi_lua::HttpRequest, local: bool) {
        // An off-tick `btv.http.fetch`: record the (cb_id, request, local) for the Worker to
        // fulfill (`eh_take_http_requests`). Routes to the daemon `http_op` leg when connected
        // (unless `local` — `btv.http.fetch_local` — which forces the browser `fetch()`), else
        // the browser's own `fetch()`. The typed result returns via `eh_http_result`.
        self.sink.borrow_mut().http_ops.push((id, request, local));
    }

    fn http_mount(&mut self, id: u64, name: String) {
        // `btv.http.mount`: record it for the Worker, which ensures the Service Worker is
        // registered and active and reports the page's origin back via `eh_http_mount_result`.
        // A tab has no port to bind, so there is nothing to do here but ask.
        self.sink.borrow_mut().http_mounts.push((id, name));
    }

    fn http_respond(&mut self, req_id: u64, reply: bemtvi_lua::HttpServerReply) {
        // A mount handler's `respond(res)`: record it for the Worker to relay back to the
        // Service Worker's parked `fetch` handler.
        self.sink
            .borrow_mut()
            .http_server_replies
            .push((req_id, reply));
    }

    fn http_unmount(&mut self, id: u64) {
        self.sink.borrow_mut().http_unmounts.push(id);
    }

    fn fs_watch_stream(&mut self, id: u64, path: String, recursive: bool) {
        // A streaming `btv.fs.watch` against the daemon (Phase 3b): record the arm for the Worker
        // to forward (`eh_take_fs_watch_requests` → `luafs_watch [id, path, recursive]`). Change
        // batches return via `eh_fs_watch_change`, a terminal error via `eh_fs_watch_err`. Only
        // reached with a daemon connected (the tick gates the watch on `has_remote_proc`).
        self.sink
            .borrow_mut()
            .fs_watch_arms
            .push((id, path, recursive));
    }

    fn fs_unwatch_stream(&mut self, id: u64) {
        // `:stop()` on a daemon watch: record the disarm for the Worker to forward (`luafs_unwatch`).
        self.sink.borrow_mut().fs_watch_disarms.push(id);
    }

    fn term_open(
        &mut self,
        buf: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        rows: u16,
        cols: u16,
    ) {
        // The web `:terminal` (Phase 7): the browser built the vt100 emulator; record the
        // open for the Worker to forward to the daemon (`eh_take_terminal_requests` →
        // `term_open`), which spawns the real PTY and streams its output back via `term_data`
        // pushes (`eh_terminal_data`). Only reached with a daemon connected (the dispatch
        // gates on `has_remote_proc`).
        self.sink
            .borrow_mut()
            .term_opens
            .push((buf, argv, cwd, rows, cols));
    }

    fn term_write(&mut self, buf: u64, bytes: Vec<u8>) {
        // A forwarded keystroke / paste / query-reply for `buf`'s daemon PTY.
        self.sink.borrow_mut().term_writes.push((buf, bytes));
    }

    fn term_interrupted(&mut self, buf: u64) {
        // A `^C` trimmed this flooding terminal; tell the Worker to drop its in-flight backlog.
        self.sink.borrow_mut().term_interrupts.push(buf);
    }

    fn term_resize(&mut self, buf: u64, rows: u16, cols: u16) {
        // The terminal window changed size; the daemon PTY must reflow too.
        self.sink.borrow_mut().term_resizes.push((buf, rows, cols));
    }

    fn term_kill(&mut self, buf: u64) {
        // The terminal closed; terminate the daemon PTY child (its exit returns on `term_exit`).
        self.sink.borrow_mut().term_kills.push(buf);
    }

    fn ts_load_grammar(&mut self, request: bemtvi_core::syntax::GrammarRequest) {
        // Unreachable by construction: the browser build highlights JS-side
        // (web-tree-sitter) and its `SyntaxEngine` owns no grammars, so nothing ever
        // asks for one to be loaded. If that changes, this must grow a real leg rather
        // than dropping the request — a grammar wedged "loading" forever would leave
        // the language silently unpainted.
        unreachable!(
            "the browser edit-host has no grammar to load ('{}')",
            request.language
        );
    }

    fn ts_install(&mut self, lang: String) {
        // `:TSInstall <lang>` on the browser build: record the request for the Worker to
        // forward to the UI thread (`eh_take_ts_requests` → `ts_install` postMessage),
        // where web-tree-sitter lives. The UI fetches the prebuilt grammar (offline
        // bundle / OPFS cache / jsDelivr), registers it, and lands the outcome back via
        // `eh_ts_install_complete`. Fire-and-forget — the editor tick doesn't block on it.
        self.sink.borrow_mut().ts_requests.push(lang);
    }

    fn lsp_ensure(&mut self, key: ServerKey, spawn: ServerSpawn) {
        // `vim.lsp.start` on a FileType: start the server in the sync client (idempotent;
        // mints a wire id, enqueues the `Spawn` + the `initialize` request). The resulting
        // wire ops drain to the Sink for the Worker to forward (`lsp_spawn` + `lsp_stdin`).
        // Only reached with a daemon connected (the editor gates on `has_remote_lsp`).
        self.lsp.ensure_server(key, spawn);
        self.flush_lsp_wire();
    }

    fn lsp_shutdown(&mut self, key: ServerKey) {
        // Cleanly stop the server in the sync client (the framed `exit` + the `Kill` drain to
        // the Sink as `lsp_stdin`/`lsp_kill` for the Worker). Driven on web by the reconnect
        // resync (Phase 7: `eh_daemon_status` → `resync_lsp_after_reconnect`), which retires
        // the pre-drop wire id before `lsp_ensure` re-spawns against the fresh link — the
        // old id's ops land on the new daemon as harmless unknown-id no-ops.
        self.lsp.shutdown(key);
        self.flush_lsp_wire();
    }

    fn lsp_notify(&mut self, key: ServerKey, note: LspNotify) {
        // A document-sync notification (`didOpen`/`didChange`/`didSave`/`didClose`): the
        // client serializes it (buffering until the server is `Ready`) and the framed bytes
        // drain to the Sink as an `lsp_stdin` for the Worker to forward.
        self.lsp.notify(key, note);
        self.flush_lsp_wire();
    }

    fn lsp_request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest) {
        // A language-feature request (hover/definition/completion/…): the client correlates
        // it by `token`; its reply returns later inbound as an `LspEvent::Reply` once the
        // daemon's `lsp_stdout` lands. The framed request bytes drain to the Sink.
        self.lsp.request(key, token, req);
        self.flush_lsp_wire();
    }

    fn lsp_apply_edit_response(&mut self, _key: ServerKey, id: u64, outcome: ApplyEditOutcome) {
        // The editor's answer to a server→client `workspace/applyEdit`, which the
        // server has been blocked on since it asked. The client frames the response
        // (it holds the JSON-RPC request id behind `id`); the bytes drain to the Sink
        // as an `lsp_stdin` for the Worker to forward, so flush.
        self.lsp.apply_edit_response(id, outcome);
        self.flush_lsp_wire();
    }

    fn lsp_stdout(&mut self, id: u64, bytes: Vec<u8>) {
        // A daemon `lsp_stdout` push: feed the server (wire `id`)'s byte buffer and process
        // every complete JSON-RPC frame. This can complete a handshake or answer a pull,
        // emitting fresh wire ops — so flush after (the Worker drains again post-call).
        self.lsp.feed_stdout(id, &bytes);
        self.flush_lsp_wire();
    }

    fn lsp_stderr(&mut self, id: u64, bytes: Vec<u8>) {
        // Diagnostic only — the browser has no LSP log file, so the client drops it (no
        // wire ops, no flush).
        self.lsp.feed_stderr(id, &bytes);
    }

    fn lsp_exited(&mut self, id: u64, code: Option<i32>, signal: Option<i32>) {
        // The server (wire `id`) exited / its pipe closed: the client surfaces an
        // `LspEvent::ServerExited` (drained via `lsp_take_events`) and forgets it; the editor
        // re-`ensure`s on the next FileType. No new wire ops normally, but flush for symmetry.
        self.lsp.exited(id, code, signal);
        self.flush_lsp_wire();
    }

    fn lsp_take_events(&mut self) -> Vec<LspEvent> {
        // Drain the distilled events the client produced (replies, diagnostics, refreshes,
        // exits) for `EditHost::drain_lsp_events` to fan into `on_lsp_event`.
        self.lsp.take_events()
    }

    fn has_remote_lsp(&self) -> bool {
        // A language server needs a process host — the daemon, or a local in-browser Worker
        // host (basedpyright's JS langserver). Same gate the proc leg uses. The Worker flips
        // `proc_host` via `eh_set_proc_host` on `:connect` / `?daemon=` / local-host bring-up;
        // when false the editor fails `vim.lsp.start` loud rather than enqueuing a spawn no
        // host can fulfil.
        self.sink.borrow().proc_host
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

/// Borrow a JS-supplied `(ptr, len)` byte buffer as a `&[u8]` (empty on null / zero
/// length). The common marshalling for every FFI export that takes raw bytes
/// (PTY / process / socket / LSP output, fs payloads) — arbitrary bytes that can't cross
/// as a C string (NULs / invalid UTF-8).
///
/// # Safety
/// `data` must point at `len` readable bytes for the call's duration (or be null with
/// `len` 0).
unsafe fn as_bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(data, len)
    }
}

/// Owned-`Vec` twin of [`as_bytes`] for the callers that keep the bytes past the call
/// (handing them to the core, which stores them).
///
/// # Safety
/// Same contract as [`as_bytes`].
unsafe fn as_byte_vec(data: *const u8, len: usize) -> Vec<u8> {
    as_bytes(data, len).to_vec()
}

/// Move a `String` out to JS as an owned `char*`; the caller frees it via
/// [`eh_free_string`] (the harness's `readStr` does this).
fn into_owned_cstr(s: String) -> *mut c_char {
    // Strip interior NULs (a C string can't carry them), but only pay the copying
    // `replace` when one is actually present — the redraw JSON crosses here every
    // keystroke and almost never holds a NUL, so the common path reuses `s`'s buffer
    // (`CString::new(String)` appends the terminator in place rather than re-copying).
    let s = if s.as_bytes().contains(&0) {
        s.replace('\0', "")
    } else {
        s
    };
    CString::new(s).unwrap().into_raw()
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
    let fx = Box::new(WasmEffects {
        sink: sink.clone(),
        lsp: SyncLspClient::new(),
    });
    let mut editor = Editor::new();
    // Wire the `"+` / `"*` registers to the browser clipboard via the Sink (the wasm twin of
    // the native `SystemClipboard`). Without a provider the editor errors loud on `"+`.
    editor.set_clipboard(Box::new(WasmClipboard { sink: sink.clone() }));
    // Install the treesitter-indent engine: it owns no parser (web-tree-sitter is JS-side)
    // and answers indentation synchronously through the `eh_js_ts_*` bridge to the worker's
    // indenter. Highlighting stays a UI-thread overlay; this is purely for `o`/`O`/`<CR>`/`=`
    // indent. Without it the core has no ts-indent and every line opens at column 0.
    editor.set_syntax_engine(Box::new(WasmSyntax::default()));
    let mut host = EditHost::new(editor, lua, fx);
    // OPFS is async to open, so `:e` / `:w` defer to the off-tick seam the Worker
    // fulfills (Phase 6); turn it on before boot so the very first open routes there.
    host.enable_offtick_fs();
    // Seed the serverless startup (buffer snapshot + mirrors + baselines) *before*
    // attaching the UI. This is only the FIRST half of boot — the Worker sources an
    // optional `init.lua` from OPFS next, then calls `eh_boot_finish` to fire the
    // startup lifecycle events + `v:vim_did_enter` (native ordering: config first).
    host.boot_begin();
    // Enable command-line completion (`:`+<Tab>) by default — the serverless analogue of
    // the native binary's `cmdline_complete_default` opt-in. Queued here, BEFORE the
    // Worker sources `init.lua` (`eh_source_lua`), so a config's own
    // `btv.cmdline_complete.setup{ ... }` still wins (last config drains last).
    host.enable_cmdline_complete();
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
/// `btv_input_mouse` counterpart of [`eh_input`]. `button`/`action`/`modifier` are the
/// `btv_input_mouse` strings (`"left"`/`"wheel"`, `"press"`/`"drag"`/`"release"`/`"up"`/
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

/// Apply a fetched remote-config bundle in a daemon session — the browser twin of the
/// native edit-host's fetch→materialize→source path. The Worker dials the daemon, fetches
/// `config_bundle` over WebTransport, re-encodes the reply to msgpack, and hands the bytes
/// here (`data`/`len`); this decodes them and stages the daemon's config + plugins into
/// the in-memory FS, points the runtimepath at the copy, and sources `init.lua` + plugins
/// — all synchronously, so `require` resolves against the staged tree. Run after [`eh_new`]
/// and *instead of* [`eh_source_lua`] (in daemon mode the editor is born remote), before
/// [`eh_boot_finish`]. Returns an owned C string: empty on success, else the error message
/// (the Worker surfaces it non-fatally — the editor still finishes booting).
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point at `len` valid
/// bytes (or be null with `len` 0). Free the returned pointer with [`eh_free_string`].
#[no_mangle]
pub unsafe extern "C" fn eh_apply_remote_config(
    h: *mut WasmEditHost,
    data: *const u8,
    len: usize,
) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(String::new());
    };
    let bytes = as_bytes(data, len);
    let result = decode_config_bundle_bytes(bytes)
        .and_then(|bundle| handle.host.apply_remote_config(bundle));
    match result {
        Ok(()) => into_owned_cstr(String::new()),
        Err(e) => into_owned_cstr(e),
    }
}

/// Seed the session cwd from a **runtime** `:connect bemtvi://…` (the browser twin of the
/// boot-time `eh_apply_remote_config` cwd seed). A runtime connect re-points the fs seam at a
/// new daemon but does NOT re-fetch `config_bundle`, so the daemon's cwd never reaches
/// `DirState` and a relative `btv.fs` path stays unrebased (resolving against the stale
/// serverless/previous dir). The Worker fetches the new daemon's cwd (a `realpath(".")` over
/// the fresh `luafs` leg) and hands it here; [`EditHost::seed_remote_cwd`] installs it as the
/// effective dir, refreshes `btv._cwd`, and marks the session daemon-fs so relative `btv.fs` /
/// spawn paths rebase against it. A no-op-safe null / empty `cwd` is ignored.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `cwd` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_seed_remote_cwd(h: *mut WasmEditHost, cwd: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    let cwd = as_str(cwd);
    if !cwd.is_empty() {
        handle.host.seed_remote_cwd(std::path::PathBuf::from(cwd));
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
/// a keystroke also wakes to fire the next `vim.defer_fn` / `btv.timer` (slice 5d).
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
    // Idle fast path (the common per-tick case): nothing enqueued → skip building the
    // serde_json maps + the `to_string`, return the empty shape directly.
    if sink.fs_reads.is_empty() && sink.fs_write_queue.is_empty() {
        return into_owned_cstr(r#"{"reads":[],"writes":[]}"#.into());
    }
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

/// Drain the remote-watch arm/disarm requests the editor enqueued since the last call, as
/// JSON `{"arm":[{"path":"…","stat":[secs,nanos,size]|null},…],"disarm":["…"]}` — the watch
/// leg's outbound half. In a daemon session the Worker forwards each arm as an
/// `fs_watch [path, stat?]` (and each disarm as `fs_unwatch [path]`) notification over
/// WebTransport; `stat` is the editor's disk baseline (the wire `[secs,nanos,size]` shape the
/// daemon's `decode_stat` reads, `secs=null` when the mtime is unknown), so a re-dialed daemon
/// catches an outage-window change on re-arm (Phase 7). Serverless OPFS has no change source, so
/// the Worker drops them. A `fs_changed` push the daemon sends in response lands back through
/// [`eh_remote_file_changed`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_watch_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"arm":[],"disarm":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.watch_arms.is_empty() && sink.watch_disarms.is_empty() {
        return into_owned_cstr(r#"{"arm":[],"disarm":[]}"#.into());
    }
    let arm: Vec<serde_json::Value> = std::mem::take(&mut sink.watch_arms)
        .into_iter()
        .map(|(path, stat)| serde_json::json!({ "path": path, "stat": stat.map(stat_to_wire_json) }))
        .collect();
    let disarm: Vec<String> = std::mem::take(&mut sink.watch_disarms);
    into_owned_cstr(serde_json::json!({ "arm": arm, "disarm": disarm }).to_string())
}

/// Encode a [`FileStat`](bemtvi_core::FileStat) as the wire `[secs, nanos, size]` array the
/// daemon's `decode_stat` reads (mirroring the native `encode_stat`): `secs` is `null` when the
/// mtime is unknown. Used by [`eh_take_watch_requests`] to carry the disk baseline as the
/// reconnect re-stat's `known` stat.
fn stat_to_wire_json(stat: bemtvi_core::FileStat) -> serde_json::Value {
    let (secs, nanos) = match stat
        .mtime
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    {
        Some(d) => (serde_json::json!(d.as_secs()), serde_json::json!(d.subsec_nanos())),
        None => (serde_json::Value::Null, serde_json::json!(0)),
    };
    serde_json::json!([secs, nanos, stat.size])
}

/// Drain the treesitter grammars the editor asked to install this convergence (one per
/// `:TSInstall <lang>`) as a JSON `["lang", …]` array, emptying the queue so each is
/// forwarded to the UI thread exactly once. The Worker posts a `ts_install` message per
/// language; the UI fetches/caches/registers the grammar (web-tree-sitter is UI-side) and
/// lands the outcome via [`eh_ts_install_complete`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_ts_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        let sink = handle.sink.borrow();
        if sink.ts_requests.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let reqs: Vec<String> = std::mem::take(&mut handle.sink.borrow_mut().ts_requests);
    into_owned_cstr(serde_json::to_string(&reqs).unwrap_or_else(|_| "[]".into()))
}

/// Drain the `"+` / `"*` clipboard writes the editor enqueued this convergence (one per
/// `"+` / `"*` yank or delete) as a JSON `["text", …]` array, emptying the queue so each is
/// forwarded to the UI thread exactly once. The Worker posts a `clipboard_write` message per
/// entry; the UI thread writes the text to `navigator.clipboard` (a Worker has no clipboard
/// access). Fire-and-forget — the editor tick never blocks on the OS clipboard. Caller frees
/// the result with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_clipboard_writes(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        let sink = handle.sink.borrow();
        if sink.clipboard_writes.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let writes: Vec<String> = std::mem::take(&mut handle.sink.borrow_mut().clipboard_writes);
    into_owned_cstr(serde_json::to_string(&writes).unwrap_or_else(|_| "[]".into()))
}

/// Push the host clipboard's current contents into the mirror a `"+` / `"*` paste reads.
/// The UI thread reads `navigator.clipboard` where it has permission (on focus / paste /
/// click) and calls this, so a subsequent `"+p` sees a copy made in another app. Stored
/// verbatim; linewise-ness is re-derived from a trailing `\n` on read (the native
/// `SystemClipboard` convention), so JS hands the text untouched.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `text` is a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_clipboard_push(h: *mut WasmEditHost, text: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    handle.sink.borrow_mut().clipboard_get = Some(as_str(text).to_string());
}

/// Land a finished browser `:TSInstall`: the UI thread fetched + registered the grammar
/// (`ok != 0`) or failed (`ok == 0`, `msg` the loud reason). Records the language for
/// `:TSInstallInfo` and echoes the outcome. Highlighting itself repaints JS-side when the
/// grammar registers, independent of this echo. See [`EditHost::complete_ts_install`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `lang` / `msg` are valid C strings.
#[no_mangle]
pub unsafe extern "C" fn eh_ts_install_complete(
    h: *mut WasmEditHost,
    lang: *const c_char,
    ok: i32,
    msg: *const c_char,
) {
    let Some(handle) = h.as_mut() else { return };
    handle
        .host
        .complete_ts_install(as_str(lang).to_string(), ok != 0, as_str(msg).to_string());
}

/// Seed the grammars available to the JS highlighter at boot, from a JSON `["lang", …]`
/// array the Worker assembles by reading the offline bundle's manifest and the OPFS
/// install cache. Backs `:TSInstallInfo`. See [`EditHost::seed_ts_installed`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `json` is a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_ts_seed_installed(h: *mut WasmEditHost, json: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    let langs: Vec<String> = serde_json::from_str(as_str(json)).unwrap_or_default();
    handle.host.seed_ts_installed(langs);
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
/// (`data`/`len` are its **raw bytes**), `1` a not-yet-existing path (new-file buffer), `2`
/// a **directory** (`path` is the canonical dir, `contents` is a JSON array of its entries
/// `[{ "is_dir": bool, "name": str }, …]` → the file-explorer listing), any other a read
/// error (`contents` is the message). A file's bytes cross as a pointer+length (not a C
/// string) so non-UTF-8 / invalid-UTF-8 content reaches Rust intact and is decoded through
/// the shared encoding seam ([`bemtvi_core::encoding::decode_to_rope`]) exactly like the native
/// and daemon read paths — the browser no longer `TextDecoder`s (and thus mangles) the
/// bytes in JS first. `contents` carries only the dir JSON (`kind == 2`) and the error
/// message; it is empty for a file/new read. Drives the real lifecycle and repaints — see
/// [`EditHost::complete_fs_read`] / [`EditHost::complete_fs_read_dir`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `path` / `contents` valid C strings;
/// `data` must point to `len` readable bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_fs_read_complete(
    h: *mut WasmEditHost,
    buffer: f64,
    path: *const c_char,
    kind: u8,
    contents: *const c_char,
    data: *const u8,
    len: usize,
    has_stat: i32,
    size: f64,
    mtime_ms: f64,
) {
    let Some(handle) = h.as_mut() else { return };
    let buffer = BufferId(buffer.max(0.0) as u64);
    let path = as_str(path).to_string();
    if kind == 2 {
        handle
            .host
            .complete_fs_read_dir(buffer, path, parse_dir_entries(as_str(contents)));
    } else {
        let bytes = as_bytes(data, len);
        // The daemon's `fs_read` reply carries the remote file's stat; thread it as the
        // read-from-disk baseline so the reconnect re-stat can compare against it. An OPFS
        // (serverless) read has none (`has_stat == 0`). `mtime_ms < 0` ⇒ unknown mtime.
        let stat = (has_stat != 0).then(|| {
            let mtime = if mtime_ms < 0.0 {
                None
            } else {
                Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms as u64))
            };
            bemtvi_core::FileStat {
                size: size.max(0.0) as u64,
                mtime,
            }
        });
        handle
            .host
            .complete_fs_read(buffer, path, kind, bytes, as_str(contents), stat);
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
/// saved-state with a [`FileStat`](bemtvi_core::FileStat) of `size` bytes + `mtime_ms`
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

/// Land a daemon-pushed file change (the `HostWatch` leg's `fs_changed`) — the watch leg's
/// inbound half, the daemon→edit-host push direction the fs leg never used. The Worker calls
/// this from `RpcClient.onNotify` when the daemon reports `path` changed under a watch armed
/// via [`eh_take_watch_requests`]. `has_stat == 0` means the file vanished (a `"deleted"`
/// reconcile); otherwise `size` + `mtime_ms` (negative = unknown) carry its new stat. Drives
/// the real `FileChangedShell` round-trip and, on an autoread / `"reload"` choice, enqueues
/// an off-tick re-fetch the Worker then fulfils (via [`eh_take_fs_requests`] →
/// [`eh_fs_read_complete`]); repaints. See [`EditHost::remote_file_changed`]. `size` /
/// `mtime_ms` are `double`s (small values; no 64-bit-int marshalling).
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `path` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_remote_file_changed(
    h: *mut WasmEditHost,
    path: *const c_char,
    has_stat: i32,
    size: f64,
    mtime_ms: f64,
) {
    let Some(handle) = h.as_mut() else { return };
    let mtime = if mtime_ms < 0.0 { -1 } else { mtime_ms as i64 };
    handle.host.remote_file_changed(
        as_str(path).to_string(),
        has_stat != 0,
        size.max(0.0) as u64,
        mtime,
    );
}

/// Land a daemon link **status phase** on the web edit-host — the browser twin of the native
/// run loop's `DaemonStatus` arm (the daemon-reconnect plan's Phase 7). The Worker's reconnect
/// supervisor calls this on every transition: `phase` is `"connected"` / `"reconnecting"` /
/// `"disconnected"`, and `reconnected != 0` marks a genuine reconnect (a down link came back, not
/// the initial connect). It mirrors the phase into `btv.daemon.status()`, fires the
/// `User DaemonStatusChanged` autocmd, and on a reconnect re-syncs the seams (re-arm watches,
/// re-open LSP, freeze lost terminals) so the fresh daemon picks up where the old one left off.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `phase` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_daemon_status(
    h: *mut WasmEditHost,
    phase: *const c_char,
    reconnected: i32,
) {
    let Some(handle) = h.as_mut() else { return };
    handle
        .host
        .apply_daemon_phase(as_str(phase), reconnected != 0);
}

// ============================================================================
// Off-tick daemon process leg (Phase 6d). The async `vim.system` / `jobstart` path: the
// editor enqueues a spawn off the keystroke tick (it can't run a process in the browser),
// the Worker forwards it over WebTransport to a connected daemon, and the child's pid/exit
// return as daemon→edit-host pushes the Worker lands back here. The browser twin of the
// native event-loop actor's proc routing (the daemon side is unchanged — Phase 3c/3q).
// ============================================================================

/// Tell the core whether a **process host** is available — a daemon over WebTransport *or* a
/// local in-browser Worker host (the Pyodide interpreter / basedpyright LSP) — flipping the
/// editor tick's async-spawn / terminal / LSP branches: `on != 0` lets them enqueue requests
/// for the Worker to fulfil; `0` (no host) fails them loud in the tick. The Worker calls this
/// on a `?daemon=` boot / runtime `:connect` / local-host bring-up (1) and on disconnect (0).
/// Unlike the off-tick fs (always on — OPFS is the serverless fallback), a process has no
/// serverless analogue, so this gate is real.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_set_proc_host(h: *mut WasmEditHost, on: i32) {
    if let Some(handle) = h.as_mut() {
        handle.sink.borrow_mut().proc_host = on != 0;
    }
}

/// Drain the async process spawns/kills the editor enqueued since the last call, as JSON the
/// Worker forwards to the daemon:
/// `{"spawn":[{"id":N,"argv":["…"],"cwd":"…"|null,"env":[["k","v"],…],"stdin":[byte,…]}],"kill":[N]}`.
/// Each spawn is dispatched exactly once (the queue is emptied); the daemon answers with
/// `proc_spawned`/`proc_exited` pushes the Worker lands via [`eh_proc_spawned`] /
/// [`eh_proc_exited`]. `stdin` is a byte array (empty for `vim.system`, which takes none).
/// Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_proc_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"spawn":[],"kill":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.proc_spawns.is_empty() && sink.proc_kills.is_empty() {
        return into_owned_cstr(r#"{"spawn":[],"kill":[]}"#.into());
    }
    let spawn: Vec<serde_json::Value> = std::mem::take(&mut sink.proc_spawns)
        .into_iter()
        .map(|(id, argv, cwd, env, stdin, stream)| {
            serde_json::json!({
                "id": id,
                "argv": argv,
                "cwd": cwd,
                "env": env.into_iter().map(|(k, v)| vec![k, v]).collect::<Vec<_>>(),
                "stdin": stdin,
                "stream": stream,
            })
        })
        .collect();
    let kill: Vec<u64> = std::mem::take(&mut sink.proc_kills);
    into_owned_cstr(serde_json::json!({ "spawn": spawn, "kill": kill }).to_string())
}

/// Land a daemon `proc_spawned` push: record the child's OS `pid` (`pid < 0` = failed to
/// spawn / unknown) under the spawn `id` so a `vim.system` handle's `.pid` resolves it.
/// `id` / `pid` are `double`s (small values; no 64-bit-int marshalling). See
/// [`EditHost::proc_spawned`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_proc_spawned(h: *mut WasmEditHost, id: f64, pid: f64) {
    if let Some(handle) = h.as_mut() {
        handle.host.proc_spawned(id.max(0.0) as u64, pid as i64);
    }
}

/// Land a daemon `proc_stdout` push: a streaming child (`btv.run_stream`'s streamed stdout, e.g. a
/// picker source) emitted a batch of stdout lines. `lines_json` is a JSON array of strings
/// (the lines, newline-stripped); fires the persistent `on_stdout` under `id`, then settles
/// + repaints so streamed rows appear as they arrive. See [`EditHost::proc_stdout`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `lines_json` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_proc_stdout(h: *mut WasmEditHost, id: f64, lines_json: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    let lines: Vec<String> = serde_json::from_str(as_str(lines_json)).unwrap_or_default();
    handle.host.proc_stdout(id.max(0.0) as u64, lines);
}

/// Land a daemon `proc_exited` push: run the spawn `id`'s `vim.system` `on_exit` with the
/// child's exit `code` and raw `stdout`/`stderr` bytes, then settle + repaint. The output is
/// passed as pointer+length (not a C string) because process output is arbitrary bytes —
/// it may contain NULs / invalid UTF-8, which a C string would truncate or mangle (Lua
/// strings are byte strings, so the callback sees them faithfully). `id` / `code` are
/// `double`s. A killed child arrives as `code == -1`. See [`EditHost::proc_exited`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `out`/`err` must point to `out_len`/
/// `err_len` readable bytes (or be null when the length is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_proc_exited(
    h: *mut WasmEditHost,
    id: f64,
    code: f64,
    out: *const u8,
    out_len: usize,
    err: *const u8,
    err_len: usize,
) {
    let Some(handle) = h.as_mut() else { return };
    let stdout = as_byte_vec(out, out_len);
    let stderr = as_byte_vec(err, err_len);
    handle
        .host
        .proc_exited(id.max(0.0) as u64, code as i32, stdout, stderr);
}

// ============================================================================
// Off-tick daemon DUPLEX-process leg (`dproc_*`) and SOCKET leg (`sock_*`). The DAP /
// framed-protocol transports: the editor enqueues a long-lived child / TCP connection off
// the tick, the Worker forwards it over WebTransport to a connected daemon, and the daemon
// streams raw bytes both ways. The browser twin of the native event-loop actor's duplex
// process / socket routing. Daemon-only (no serverless fallback). See the LSP leg, which
// has the identical bidirectional shape.
// ============================================================================

/// Drain the duplex-process opens/writes/kills the editor enqueued, as JSON the Worker
/// forwards: `{"open":[{"id":N,"argv":[…],"cwd":…,"env":[[k,v],…]}],"write":[{"id":N,
/// "bytes":[…]}],"kill":[N]}`. Raw stdout/stderr return via [`eh_dproc_out`], the exit via
/// [`eh_dproc_exit`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_dproc_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"open":[],"write":[],"kill":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.dproc_opens.is_empty() && sink.dproc_writes.is_empty() && sink.dproc_kills.is_empty() {
        return into_owned_cstr(r#"{"open":[],"write":[],"kill":[]}"#.into());
    }
    let open: Vec<serde_json::Value> = std::mem::take(&mut sink.dproc_opens)
        .into_iter()
        .map(|(id, argv, cwd, env)| {
            serde_json::json!({
                "id": id,
                "argv": argv,
                "cwd": cwd,
                "env": env.into_iter().map(|(k, v)| vec![k, v]).collect::<Vec<_>>(),
            })
        })
        .collect();
    let write: Vec<serde_json::Value> = std::mem::take(&mut sink.dproc_writes)
        .into_iter()
        .map(|(id, bytes)| serde_json::json!({ "id": id, "bytes": bytes }))
        .collect();
    let kill: Vec<u64> = std::mem::take(&mut sink.dproc_kills);
    into_owned_cstr(serde_json::json!({ "open": open, "write": write, "kill": kill }).to_string())
}

/// Land a daemon `dproc_out` push: a raw output chunk from a duplex child. `stderr != 0`
/// selects the error stream. `data` is pointer+length (arbitrary bytes — a framed protocol).
/// See [`EditHost::dproc_out`].
///
/// # Safety
/// `h` from [`eh_new`], not freed; `data` points to `len` readable bytes (or null when 0).
#[no_mangle]
pub unsafe extern "C" fn eh_dproc_out(
    h: *mut WasmEditHost,
    id: f64,
    data: *const u8,
    len: usize,
    stderr: i32,
) {
    let Some(handle) = h.as_mut() else { return };
    let bytes = as_byte_vec(data, len);
    handle
        .host
        .dproc_out(id.max(0.0) as u64, bytes, stderr != 0);
}

/// Land a daemon `dproc_exit` push: the duplex child exited (`code == -1` on a kill / spawn
/// failure). See [`EditHost::dproc_exit`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_dproc_exit(h: *mut WasmEditHost, id: f64, code: f64) {
    if let Some(handle) = h.as_mut() {
        handle.host.dproc_exit(id.max(0.0) as u64, code as i32);
    }
}

/// Drain the socket connects/writes/closes the editor enqueued, as JSON the Worker forwards:
/// `{"connect":[{"id":N,"host":"…","port":N}],"write":[{"id":N,"bytes":[…]}],"close":[N]}`.
/// `connected`/data/`closed` return via [`eh_sock_connected`] / [`eh_sock_data`] /
/// [`eh_sock_closed`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_sock_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"connect":[],"write":[],"close":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.sock_connects.is_empty() && sink.sock_writes.is_empty() && sink.sock_closes.is_empty() {
        return into_owned_cstr(r#"{"connect":[],"write":[],"close":[]}"#.into());
    }
    let connect: Vec<serde_json::Value> = std::mem::take(&mut sink.sock_connects)
        .into_iter()
        .map(|(id, host, port)| serde_json::json!({ "id": id, "host": host, "port": port }))
        .collect();
    let write: Vec<serde_json::Value> = std::mem::take(&mut sink.sock_writes)
        .into_iter()
        .map(|(id, bytes)| serde_json::json!({ "id": id, "bytes": bytes }))
        .collect();
    let close: Vec<u64> = std::mem::take(&mut sink.sock_closes);
    into_owned_cstr(
        serde_json::json!({ "connect": connect, "write": write, "close": close }).to_string(),
    )
}

/// Land a daemon `sock_connected` push: the TCP connection is established. See
/// [`EditHost::sock_connected`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_sock_connected(h: *mut WasmEditHost, id: f64) {
    if let Some(handle) = h.as_mut() {
        handle.host.sock_connected(id.max(0.0) as u64);
    }
}

/// Land a daemon `sock_data` push: a raw inbound chunk. `data` is pointer+length. See
/// [`EditHost::sock_data`].
///
/// # Safety
/// `h` from [`eh_new`], not freed; `data` points to `len` readable bytes (or null when 0).
#[no_mangle]
pub unsafe extern "C" fn eh_sock_data(h: *mut WasmEditHost, id: f64, data: *const u8, len: usize) {
    let Some(handle) = h.as_mut() else { return };
    let bytes = as_byte_vec(data, len);
    handle.host.sock_data(id.max(0.0) as u64, bytes);
}

/// Land a daemon `sock_closed` push: the connection closed. `err` is a C string with the
/// failure cause, or null on a clean close. See [`EditHost::sock_closed`].
///
/// # Safety
/// `h` from [`eh_new`], not freed; `err` a valid C string or null.
#[no_mangle]
pub unsafe extern "C" fn eh_sock_closed(h: *mut WasmEditHost, id: f64, err: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    let error = if err.is_null() {
        None
    } else {
        Some(as_str(err).to_string())
    };
    handle.host.sock_closed(id.max(0.0) as u64, error);
}

// ============================================================================
// Off-tick daemon `btv.fs` leg (Phase 2 of the off-tick plan). The async `btv.fs.*` path: the
// editor enqueues a high-level op off the keystroke tick (it can't run a synchronous fs in the
// browser), the Worker forwards it as one `luafs_op` request over WebTransport to a connected
// daemon, which runs `run_fs_job` and replies; the typed result lands back here and resolves the
// op's promise. The browser twin of the native event-loop actor's `btv.fs` routing. Only reached
// with a daemon connected (serverless `btv.fs` fails loud in the core — see effects.rs).
// ============================================================================

/// Lower an [`FsJob`](bemtvi_lua::FsJob) into the JSON object the Worker forwards as a
/// `luafs_op` request map: `{ id, op, … }`. The op name + field names match the daemon's
/// `bemtvi_lua::fs_job_from_value` decoder. `data` (write/append) rides as a JSON byte array
/// (the Worker converts it to a byte buffer so it crosses as msgpack `bin`), exactly as the
/// proc leg's `stdin` does.
fn fs_job_to_json(id: u64, job: &bemtvi_lua::FsJob, local: bool) -> serde_json::Value {
    use bemtvi_lua::FsJob;
    let mut o = serde_json::Map::new();
    o.insert("id".into(), serde_json::json!(id));
    // A `local`-flagged op (the plugin manager's discover / source) must hit the local
    // store (OPFS) even in a daemon session — the Worker routes on this. See
    // `docs/plans/2026-07-03-remote-aware-plugin-manager.md`.
    o.insert("local".into(), serde_json::json!(local));
    let mut put = |k: &str, v: serde_json::Value| {
        o.insert(k.into(), v);
    };
    match job {
        FsJob::Stat { path } => {
            put("op", "stat".into());
            put("path", path.clone().into());
        }
        FsJob::Lstat { path } => {
            put("op", "lstat".into());
            put("path", path.clone().into());
        }
        FsJob::Exists { path } => {
            put("op", "exists".into());
            put("path", path.clone().into());
        }
        FsJob::Readdir { path } => {
            put("op", "readdir".into());
            put("path", path.clone().into());
        }
        FsJob::Read { path } => {
            put("op", "read".into());
            put("path", path.clone().into());
        }
        FsJob::ReadText { path, encoding } => {
            put("op", "read_text".into());
            put("path", path.clone().into());
            put("encoding", encoding.clone().into());
        }
        FsJob::Write { path, data } => {
            put("op", "write".into());
            put("path", path.clone().into());
            put("data", serde_json::json!(data));
        }
        FsJob::Append { path, data } => {
            put("op", "append".into());
            put("path", path.clone().into());
            put("data", serde_json::json!(data));
        }
        FsJob::Mkdir {
            path,
            recursive,
            mode,
        } => {
            put("op", "mkdir".into());
            put("path", path.clone().into());
            put("recursive", (*recursive).into());
            put("mode", (*mode).into());
        }
        FsJob::Rename { from, to } => {
            put("op", "rename".into());
            put("from", from.clone().into());
            put("to", to.clone().into());
        }
        FsJob::Remove { path, recursive } => {
            put("op", "remove".into());
            put("path", path.clone().into());
            put("recursive", (*recursive).into());
        }
        FsJob::Copy {
            src,
            dst,
            recursive,
        } => {
            put("op", "copy".into());
            put("src", src.clone().into());
            put("dst", dst.clone().into());
            put("recursive", (*recursive).into());
        }
        FsJob::Realpath { path } => {
            put("op", "realpath".into());
            put("path", path.clone().into());
        }
        FsJob::Which { name } => {
            put("op", "which".into());
            put("name", name.clone().into());
        }
        FsJob::HashFile { path, algo } => {
            put("op", "hash_file".into());
            put("path", path.clone().into());
            put("algo", algo.clone().into());
        }
    }
    serde_json::Value::Object(o)
}

/// Lower a [`GitJob`](bemtvi_lua::GitJob) into the JSON object the Worker forwards as a
/// `git_op` request map: `{ id, op, … }`. The op name + field names match the daemon's
/// `bemtvi_lua::git_job_from_value` decoder. The git twin of [`fs_job_to_json`].
fn git_job_to_json(id: u64, job: &bemtvi_lua::GitJob, local: bool) -> serde_json::Value {
    use bemtvi_lua::GitJob;
    let mut o = serde_json::Map::new();
    o.insert("id".into(), serde_json::json!(id));
    o.insert("local".into(), serde_json::json!(local));
    let mut put = |k: &str, v: serde_json::Value| {
        o.insert(k.into(), v);
    };
    match job {
        GitJob::Discover { path } => {
            put("op", "discover".into());
            put("path", path.clone().into());
        }
        GitJob::Head { path } => {
            put("op", "head".into());
            put("path", path.clone().into());
        }
        GitJob::Show { file, rev } => {
            put("op", "show".into());
            put("file", file.clone().into());
            put("rev", rev.clone().into());
        }
        GitJob::DiffFile { path, file } => {
            put("op", "diff_file".into());
            put("path", path.clone().into());
            put("file", file.clone().into());
        }
        GitJob::Status { path, ignored } => {
            put("op", "status".into());
            put("path", path.clone().into());
            put("ignored", serde_json::json!(ignored));
        }
        // Phase-2 mutation / network verbs. Optional `depth`/`branch` are included only
        // when set, so the daemon decoder reads them back as `None` (matching the rmpv
        // `git_job_to_value` codec) rather than a spurious 0 / empty string.
        GitJob::Clone {
            url,
            dir,
            depth,
            branch,
        } => {
            put("op", "clone".into());
            put("url", url.clone().into());
            put("dir", dir.clone().into());
            if let Some(d) = depth {
                put("depth", serde_json::json!(d));
            }
            if let Some(b) = branch {
                put("branch", b.clone().into());
            }
        }
        GitJob::Checkout { dir, rev, detach } => {
            put("op", "checkout".into());
            put("dir", dir.clone().into());
            put("rev", rev.clone().into());
            put("detach", serde_json::json!(detach));
        }
        GitJob::Fetch { dir, unshallow } => {
            put("op", "fetch".into());
            put("dir", dir.clone().into());
            put("unshallow", serde_json::json!(unshallow));
        }
        GitJob::Pull { dir } => {
            put("op", "pull".into());
            put("dir", dir.clone().into());
        }
        GitJob::SubmoduleUpdate {
            dir,
            init,
            recursive,
        } => {
            put("op", "submodule_update".into());
            put("dir", dir.clone().into());
            put("init", serde_json::json!(init));
            put("recursive", serde_json::json!(recursive));
        }
    }
    serde_json::Value::Object(o)
}

/// Drain the off-tick `btv.fs` ops the editor enqueued since the last call, as a JSON array of
/// `luafs_op` request objects (`[{ id, op, path, … }, …]`), emptying the queue so each is
/// forwarded exactly once. The Worker sends one `luafs_op` request per object and lands the
/// reply via [`eh_fs_op_result`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_fs_op_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        let sink = handle.sink.borrow();
        if sink.fs_ops.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let ops: Vec<serde_json::Value> = std::mem::take(&mut handle.sink.borrow_mut().fs_ops)
        .iter()
        .map(|(id, job, local)| fs_job_to_json(*id, job, *local))
        .collect();
    into_owned_cstr(serde_json::Value::Array(ops).to_string())
}

/// Land the typed result of an off-tick `btv.fs` op (the `luafs_op` leg's reply): resolve /
/// reject the promise enqueued under `id` with the op's outcome, then settle + repaint. `reply`
/// is the `["ok", <fs-value>] | ["err", code, message]` envelope re-encoded to **msgpack bytes**
/// by the Worker (passed as pointer+length, not a C string, because a `read` result carries raw
/// file bytes — NULs / invalid UTF-8 — that a C string would mangle); Rust decodes it through the
/// shared `bemtvi_lua::fswire` codec. `id` is a `double` (a small counter). See
/// [`EditHost::fs_op_result`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable bytes
/// (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_fs_op_result(
    h: *mut WasmEditHost,
    id: f64,
    data: *const u8,
    len: usize,
) {
    let Some(handle) = h.as_mut() else { return };
    let reply = as_byte_vec(data, len);
    handle.host.fs_op_result(id.max(0.0) as u64, reply);
}

/// Drain the off-tick `btv.git` ops the editor enqueued since the last call, as a JSON array of
/// `git_op` request objects (`[{ id, op, path, … }, …]`), emptying the queue so each is
/// forwarded exactly once. The Worker sends one `git_op` request per object over WebTransport
/// and lands the reply via [`eh_git_op_result`]. Only non-empty when a daemon is connected (a
/// serverless git op is rejected loud before it reaches the sink). The git twin of
/// [`eh_take_fs_op_requests`]; caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_git_op_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        let sink = handle.sink.borrow();
        if sink.git_ops.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let ops: Vec<serde_json::Value> = std::mem::take(&mut handle.sink.borrow_mut().git_ops)
        .iter()
        .map(|(id, job, local)| git_job_to_json(*id, job, *local))
        .collect();
    into_owned_cstr(serde_json::Value::Array(ops).to_string())
}

/// Land the typed result of an off-tick `btv.git` op (the `git_op` leg's reply): resolve /
/// reject the promise enqueued under `id`, then settle + repaint. `reply` is the `["ok",
/// <git-value>] | ["err", code, message]` envelope re-encoded to **msgpack bytes** by the
/// Worker (pointer+length, not a C string, because `show`'s blob carries raw bytes); Rust
/// decodes it through the shared `bemtvi_lua::gitwire` codec. The git twin of
/// [`eh_fs_op_result`]. See [`EditHost::git_op_result`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable
/// bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_git_op_result(h: *mut WasmEditHost, id: f64, data: *const u8, len: usize) {
    let Some(handle) = h.as_mut() else { return };
    let reply = as_byte_vec(data, len);
    handle.host.git_op_result(id.max(0.0) as u64, reply);
}

// ============================================================================
// Off-tick `btv.http.fetch` leg (`http_op`). The async HTTP path: the editor enqueues a
// request off the keystroke tick (the browser can't block), the Worker runs the round-trip
// — over a connected daemon's `http_op` leg, else the browser's own `fetch()` — and the
// typed result lands back here to resolve the promise. The browser twin of the native
// event-loop actor's `btv.http` routing. Unlike `btv.fs`, a serverless session needs no
// daemon (the browser always has `fetch()`).
// ============================================================================

/// Lower an [`HttpRequest`](bemtvi_lua::HttpRequest) into the JSON object the Worker forwards
/// (`{ id, method, url, headers, body, timeout_ms }`). `headers` is a list of `[name, value]`
/// pairs; `body` rides as a JSON byte array (the Worker converts it to a byte buffer — for a
/// daemon forward it crosses as msgpack `bin`, for a `fetch()` it becomes the request body).
fn http_request_to_json(id: u64, req: &bemtvi_lua::HttpRequest, local: bool) -> serde_json::Value {
    let headers: Vec<serde_json::Value> = req
        .headers
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();
    let mut o = serde_json::Map::new();
    o.insert("id".into(), serde_json::json!(id));
    // `local` forces the browser `fetch()` (bypass the daemon) — `btv.http.fetch_local`.
    o.insert("local".into(), serde_json::json!(local));
    o.insert("method".into(), req.method.clone().into());
    o.insert("url".into(), req.url.clone().into());
    o.insert("headers".into(), serde_json::Value::Array(headers));
    o.insert("body".into(), serde_json::json!(req.body));
    o.insert("redirect".into(), req.redirect.clone().into());
    o.insert(
        "timeout_ms".into(),
        match req.timeout_ms {
            Some(ms) => serde_json::json!(ms),
            None => serde_json::Value::Null,
        },
    );
    o.insert(
        "max_redirects".into(),
        match req.max_redirects {
            Some(n) => serde_json::json!(n),
            None => serde_json::Value::Null,
        },
    );
    serde_json::Value::Object(o)
}

/// Drain the off-tick `btv.http.fetch` requests the editor enqueued since the last call, as a
/// JSON array of request objects (`[{ id, method, url, headers, body, timeout_ms }, …]`),
/// emptying the queue so each is forwarded exactly once. The Worker runs one round-trip per
/// object (daemon `http_op` or browser `fetch()`) and lands the reply via [`eh_http_result`].
/// Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_http_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        let sink = handle.sink.borrow();
        if sink.http_ops.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let ops: Vec<serde_json::Value> = std::mem::take(&mut handle.sink.borrow_mut().http_ops)
        .iter()
        .map(|(id, req, local)| http_request_to_json(*id, req, *local))
        .collect();
    into_owned_cstr(serde_json::Value::Array(ops).to_string())
}

// =============================================================================
// `btv.http.mount` — the Service Worker leg.
//
// The browser twin of the native `httpmount` listener. A tab cannot bind a TCP port, so a
// Service Worker intercepts `fetch` for the reserved `/plugin/` namespace on the page's own
// origin and relays each request in here; the same `HttpServerRequest`/`HttpServerReply`
// contract the native listener serves, so a plugin's Lua is identical between worlds.
//
// The direction is the interesting difference from the `http_op` leg above. `fetch` is
// editor-initiated (drain → fulfil → land); a mount is BROWSER-initiated, so the inbound
// `eh_http_server_request` is the start of the round-trip, not the end of one, and the reply
// leaves through a drain rather than arriving through a call.
// =============================================================================

/// Drain the `btv.http.mount` publications the editor enqueued since the last call, as a JSON
/// array (`[{ id, name }, …]`), emptying the queue so each is handled exactly once. The
/// Worker registers/awaits the Service Worker and reports the resulting origin (or the reason
/// there is none) via [`eh_http_mount_result`]. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_http_mounts(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        if handle.sink.borrow().http_mounts.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let mounts: Vec<serde_json::Value> = std::mem::take(&mut handle.sink.borrow_mut().http_mounts)
        .into_iter()
        .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
        .collect();
    into_owned_cstr(serde_json::Value::Array(mounts).to_string())
}

/// Drain the `mount:close()` retirements the editor enqueued since the last call, as a JSON
/// array of mount ids. The editor has already dropped the route (a later request 404s); this
/// only lets the Worker forget its own bookkeeping. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_http_unmounts(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        if handle.sink.borrow().http_unmounts.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let ids: Vec<serde_json::Value> = std::mem::take(&mut handle.sink.borrow_mut().http_unmounts)
        .into_iter()
        .map(|id| serde_json::json!(id))
        .collect();
    into_owned_cstr(serde_json::Value::Array(ids).to_string())
}

/// Settle an `btv.http.mount` promise: `ok` non-zero resolves it with `text` as the bound
/// origin (`"https://demo.bemtvi.dev"` — the page's own), zero rejects it with `text` as the
/// reason (no Service Worker on an insecure origin, a registration failure). Rejecting is
/// what keeps a browser mount honest: a plugin must never be handed a URL that would 404.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `text` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_http_mount_result(
    h: *mut WasmEditHost,
    id: f64,
    ok: i32,
    text: *const c_char,
) {
    let Some(handle) = h.as_mut() else { return };
    let text = as_str(text).to_string();
    handle
        .host
        .http_mount_result(id.max(0.0) as u64, ok != 0, text);
}

/// Feed one Service-Worker-intercepted request into a mount's handler. `json` is
/// `{ req_id, method, path, query, headers: [[name, value], …], body: [<bytes>] }` — `path`
/// is the FULL path (`/plugin/example/style.css`), which the editor splits and routes exactly
/// as the native listener does. The handler's `respond(res)` leaves via
/// [`eh_take_http_server_replies`], keyed by the same `req_id`.
///
/// A path naming no live mount replies `404` immediately, without entering Lua — the same
/// answer the native listener gives.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `json` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_http_server_request(h: *mut WasmEditHost, json: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(as_str(json)) else {
        // The Worker builds this, so a malformed relay is a bug in our own JS — and with no
        // usable `req_id` there is nothing to answer, so it must not pass silently.
        // emscripten routes stderr to the browser console — the only loud channel available
        // here, since there is no `req_id` to answer with and no editor message line reachable
        // from this side of the FFI.
        eprintln!("bemtvi: btv.http.mount got a malformed request relay from the Worker");
        return;
    };
    let req_id = v.get("req_id").and_then(|x| x.as_u64()).unwrap_or(0);
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("GET");
    let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("/");
    let query = v.get("query").and_then(|x| x.as_str()).unwrap_or("");
    let headers = v
        .get("headers")
        .and_then(|x| x.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some((
                        pair.first()?.as_str()?.to_string(),
                        pair.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let body: Vec<u8> = v
        .get("body")
        .and_then(|x| x.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|b| b.as_u64())
                .map(|b| b as u8)
                .collect()
        })
        .unwrap_or_default();
    handle
        .host
        .http_server_request(req_id, method, path, query, headers, body);
}

/// Drain the mount-handler replies the editor produced since the last call, as a JSON array
/// (`[{ req_id, status, headers: [[name, value], …], body: [<bytes>] }, …]`). The Worker
/// relays each to the window, which posts it down the Service Worker's `MessageChannel` port.
/// Caller frees with [`eh_free_string`].
///
/// A body rides as a JSON byte array rather than a string: a mount serves images and fonts,
/// not just text, and a lossy UTF-8 round-trip would corrupt them.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_http_server_replies(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr("[]".into());
    };
    {
        if handle.sink.borrow().http_server_replies.is_empty() {
            return into_owned_cstr("[]".into());
        }
    }
    let replies: Vec<serde_json::Value> =
        std::mem::take(&mut handle.sink.borrow_mut().http_server_replies)
            .into_iter()
            .map(|(req_id, reply)| {
                serde_json::json!({
                    "req_id": req_id,
                    "status": reply.status,
                    "headers": reply
                        .headers
                        .iter()
                        .map(|(n, v)| serde_json::json!([n, v]))
                        .collect::<Vec<_>>(),
                    "body": reply.body,
                })
            })
            .collect();
    into_owned_cstr(serde_json::Value::Array(replies).to_string())
}

/// Land the typed result of an off-tick `btv.http.fetch` (the `http_op` leg's reply, or the
/// browser `fetch()`'s result msgpack-encoded by the Worker): resolve / reject the promise
/// enqueued under `id`, then settle + repaint. `reply` is the `["ok", <response>] | ["err",
/// message]` envelope as **msgpack bytes** (pointer+length, not a C string — a response body
/// carries raw bytes a C string would mangle); Rust decodes it through the shared
/// `bemtvi_lua::httpwire` codec. See [`EditHost::http_result`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable
/// bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_http_result(
    h: *mut WasmEditHost,
    id: f64,
    data: *const u8,
    len: usize,
) {
    let Some(handle) = h.as_mut() else { return };
    let reply = as_byte_vec(data, len);
    handle.host.http_result(id.max(0.0) as u64, reply);
}
/// Drain the streaming `btv.fs.watch` arm/disarm requests the editor enqueued since the last call,
/// as JSON `{"arm":[{"id":N,"path":"…","recursive":bool}],"disarm":[N]}` — the `luafs_watch` leg's
/// outbound half. The Worker forwards each arm as `luafs_watch [id, path, recursive]` and each
/// disarm as `luafs_unwatch [id]` over WebTransport; the daemon's change batches return via
/// [`eh_fs_watch_change`] / a terminal error via [`eh_fs_watch_err`]. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_fs_watch_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"arm":[],"disarm":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.fs_watch_arms.is_empty() && sink.fs_watch_disarms.is_empty() {
        return into_owned_cstr(r#"{"arm":[],"disarm":[]}"#.into());
    }
    let arm: Vec<serde_json::Value> = std::mem::take(&mut sink.fs_watch_arms)
        .into_iter()
        .map(|(id, path, recursive)| serde_json::json!({ "id": id, "path": path, "recursive": recursive }))
        .collect();
    let disarm: Vec<u64> = std::mem::take(&mut sink.fs_watch_disarms);
    into_owned_cstr(serde_json::json!({ "arm": arm, "disarm": disarm }).to_string())
}

/// Land a streaming `btv.fs.watch` change batch (the daemon `luafs_change` push): fire the stream
/// `id`'s pump with the coalesced change `kind` and its `paths` (a JSON string array), then settle
/// + repaint. `id` is a `double` (a small stream counter). See [`EditHost::fs_watch_event`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `kind` / `paths_json` valid C strings.
#[no_mangle]
pub unsafe extern "C" fn eh_fs_watch_change(
    h: *mut WasmEditHost,
    id: f64,
    kind: *const c_char,
    paths_json: *const c_char,
) {
    let Some(handle) = h.as_mut() else { return };
    let paths: Vec<String> = serde_json::from_str(as_str(paths_json)).unwrap_or_default();
    handle
        .host
        .fs_watch_event(id.max(0.0) as u64, as_str(kind).to_string(), paths);
}

/// Land a streaming `btv.fs.watch` terminal error (the daemon `luafs_watch_err` push): reject the
/// stream `id`'s pull with `message` (ending the iteration loud), then settle. See
/// [`EditHost::fs_watch_error`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `message` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_fs_watch_err(h: *mut WasmEditHost, id: f64, message: *const c_char) {
    let Some(handle) = h.as_mut() else { return };
    handle
        .host
        .fs_watch_error(id.max(0.0) as u64, as_str(message).to_string());
}

/// Drain the terminal ops the editor enqueued since the last call (the web `:terminal` —
/// Phase 7), as JSON the Worker forwards to the daemon:
/// `{"open":[{"buf":N,"argv":["…"],"cwd":"…"|null,"rows":R,"cols":C}],
///   "write":[{"buf":N,"bytes":[byte,…]}],"resize":[{"buf":N,"rows":R,"cols":C}],"kill":[N]}`.
/// Each op is dispatched exactly once (the queues are emptied). The daemon answers with
/// `term_data`/`term_exit` pushes the Worker lands via [`eh_terminal_data`] / [`eh_terminal_exit`].
/// Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_terminal_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(
            r#"{"open":[],"write":[],"resize":[],"kill":[],"interrupt":[]}"#.into(),
        );
    };
    let mut sink = handle.sink.borrow_mut();
    if sink.term_opens.is_empty()
        && sink.term_writes.is_empty()
        && sink.term_resizes.is_empty()
        && sink.term_kills.is_empty()
        && sink.term_interrupts.is_empty()
    {
        return into_owned_cstr(
            r#"{"open":[],"write":[],"resize":[],"kill":[],"interrupt":[]}"#.into(),
        );
    }
    let open: Vec<serde_json::Value> = std::mem::take(&mut sink.term_opens)
        .into_iter()
        .map(|(buf, argv, cwd, rows, cols)| {
            serde_json::json!({ "buf": buf, "argv": argv, "cwd": cwd, "rows": rows, "cols": cols })
        })
        .collect();
    let write: Vec<serde_json::Value> = std::mem::take(&mut sink.term_writes)
        .into_iter()
        .map(|(buf, bytes)| serde_json::json!({ "buf": buf, "bytes": bytes }))
        .collect();
    let resize: Vec<serde_json::Value> = std::mem::take(&mut sink.term_resizes)
        .into_iter()
        .map(|(buf, rows, cols)| serde_json::json!({ "buf": buf, "rows": rows, "cols": cols }))
        .collect();
    let kill: Vec<u64> = std::mem::take(&mut sink.term_kills);
    let interrupt: Vec<u64> = std::mem::take(&mut sink.term_interrupts);
    into_owned_cstr(
        serde_json::json!({ "open": open, "write": write, "resize": resize, "kill": kill, "interrupt": interrupt })
            .to_string(),
    )
}

/// Land a daemon `term_data` push: feed `buf`'s vt100 emulator the child's raw PTY output.
/// **Feed only** — it does not project or repaint; the Worker calls [`eh_terminal_flush`]
/// once after draining the whole push batch, so a flood costs one projection per repaint
/// (the native leg's "project once per repaint" rule). The bytes are passed as pointer +
/// length (not a C string) because PTY output is arbitrary bytes (NULs / invalid UTF-8). `buf`
/// is a `double` (a small buffer id). See [`EditHost::terminal_feed`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable
/// bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_terminal_data(
    h: *mut WasmEditHost,
    buf: f64,
    data: *const u8,
    len: usize,
) {
    let Some(handle) = h.as_mut() else { return };
    let bytes = as_bytes(data, len);
    handle
        .host
        .terminal_feed(BufferId(buf.max(0.0) as u64), bytes);
}

/// Project every live terminal once and settle + repaint — call after a batch of
/// [`eh_terminal_data`] feeds (one `term_data` push-drain). See [`EditHost::terminal_flush`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_terminal_flush(h: *mut WasmEditHost) {
    if let Some(handle) = h.as_mut() {
        handle.host.terminal_flush();
    }
}

/// Land a daemon `term_exit` push: record `buf`'s child exit (leave terminal mode, append the
/// `[Process exited]` notice), drop the emulator, then settle + repaint. `buf` / `code` are
/// `double`s; a killed child arrives as `code == -1`. See [`EditHost::terminal_exit`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_terminal_exit(h: *mut WasmEditHost, buf: f64, code: f64) {
    if let Some(handle) = h.as_mut() {
        handle
            .host
            .terminal_exit(BufferId(buf.max(0.0) as u64), code as i32);
    }
}

// ============================================================================
// Off-tick daemon LSP leg (Phase 6e). The browser has no process host, so language
// servers run on the daemon: the editor's `vim.lsp.start` / document-sync / feature
// requests feed the in-Worker `SyncLspClient`, whose raw JSON-RPC the Worker forwards
// over WebTransport (`lsp_spawn`/`lsp_stdin`/`lsp_kill`), and the daemon's
// `lsp_stdout`/`lsp_stderr`/`lsp_exited` pushes feed back here. The browser twin of the
// native run loop's `lsp_events` arm — the daemon side (`serve_one_lsp`) is unchanged.
// ============================================================================

/// Drain the LSP wire ops the editor's `SyncLspClient` produced since the last call, as
/// JSON the Worker forwards to the daemon:
/// `{"spawn":[{"id":N,"program":"…","args":["…"],"cwd":"…"}],
///   "stdin":[{"id":N,"bytes":[byte,…]}],"kill":[N]}`. The queue is emptied (each op is
/// dispatched exactly once). The Worker sends every `spawn` first, then `stdin`, then
/// `kill` — preserving the client's "spawn precedes its first `stdin`" ordering (the
/// daemon must `lsp_spawn` before the `initialize` `lsp_stdin` that follows) without an
/// interleaved ordered array. `bytes` is the framed JSON-RPC chunk as a byte array (the
/// Worker re-encodes it to a msgpack `bin`, like the proc leg's `stdin`). Caller frees
/// with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_lsp_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"spawn":[],"stdin":[],"kill":[]}"#.into());
    };
    if handle.sink.borrow().lsp_ops.is_empty() {
        return into_owned_cstr(r#"{"spawn":[],"stdin":[],"kill":[]}"#.into());
    }
    let ops = std::mem::take(&mut handle.sink.borrow_mut().lsp_ops);
    let mut spawn = Vec::new();
    let mut stdin = Vec::new();
    let mut kill = Vec::new();
    for op in ops {
        match op {
            WireOp::Spawn {
                id,
                program,
                args,
                cwd,
                env,
            } => spawn.push(
                serde_json::json!({ "id": id, "program": program, "args": args, "cwd": cwd, "env": env }),
            ),
            WireOp::Stdin { id, bytes } => {
                stdin.push(serde_json::json!({ "id": id, "bytes": bytes }))
            }
            WireOp::Kill { id } => kill.push(id),
        }
    }
    into_owned_cstr(serde_json::json!({ "spawn": spawn, "stdin": stdin, "kill": kill }).to_string())
}

/// Land a daemon `lsp_stdout` push: feed the server (wire `id`)'s byte buffer into the
/// `SyncLspClient`, which parses every complete `Content-Length`-framed JSON-RPC frame
/// (completing a handshake, answering a `workspace/configuration` pull, landing a feature
/// reply / diagnostics), then drains its events into `on_lsp_event` and repaints. The
/// outbound JSON-RPC any of that issues is left in the Sink for the Worker to drain (via
/// [`eh_take_lsp_requests`]) after this call. The bytes are passed as pointer+length (not a
/// C string): LSP framing is UTF-8 but a payload may legitimately carry embedded NULs that a
/// C string would truncate. `id` is a `double` (a small wire counter). See
/// [`EditHost::lsp_stdout`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable
/// bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_lsp_stdout(h: *mut WasmEditHost, id: f64, data: *const u8, len: usize) {
    let Some(handle) = h.as_mut() else { return };
    let bytes = as_byte_vec(data, len);
    handle.host.lsp_stdout(id.max(0.0) as u64, bytes);
}

/// Land a daemon `lsp_stderr` push (the server's diagnostic output): fed to the
/// `SyncLspClient`, which drops it — the browser has no LSP log file. Kept so the Worker has
/// a sink to call rather than silently discarding the wire method. Bytes cross as
/// pointer+length (server stderr is arbitrary bytes). `id` is a `double`. See
/// [`EditHost::lsp_stderr`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `data` must point to `len` readable
/// bytes (or be null when `len` is 0).
#[no_mangle]
pub unsafe extern "C" fn eh_lsp_stderr(h: *mut WasmEditHost, id: f64, data: *const u8, len: usize) {
    let Some(handle) = h.as_mut() else { return };
    let bytes = as_byte_vec(data, len);
    handle.host.lsp_stderr(id.max(0.0) as u64, bytes);
}

/// Land a daemon `lsp_exited` push: the server (wire `id`) exited or its pipe closed. The
/// `SyncLspClient` surfaces an `LspEvent::ServerExited` (the editor tells the user and
/// re-`ensure`s on the next FileType), then settles + repaints. `code` / `signal` are
/// `double`s; a negative value means "not collected" (a kill, a dropped link), per the
/// proc-leg convention. See [`EditHost::lsp_exited`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_lsp_exited(h: *mut WasmEditHost, id: f64, code: f64, signal: f64) {
    if let Some(handle) = h.as_mut() {
        handle
            .host
            .lsp_exited(id.max(0.0) as u64, code as i32, signal as i32);
    }
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
            // Map the frame's elements straight to JSON. Don't `Value::Array(params.clone())`
            // first — that deep-clones the whole redraw tree (every grid line / cell) on the
            // hot per-keystroke read path just to wrap it; `value_to_json` borrows, so build
            // the JSON array directly from the slice.
            Some(params) => {
                serde_json::Value::Array(params.iter().map(value_to_json).collect()).to_string()
            }
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

/// JSON object `{ file_name: full_text }` for every visible, non-terminal buffer that
/// is **not** the focused one — the background buffers the UI's JS highlighter can't
/// otherwise see (their text never rode [`eh_lines`], so a window beneath a grabbing
/// float renders un-highlighted until focused). `{}` when only the focused buffer is
/// on screen. Caller frees with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_aux_lines(h: *mut WasmEditHost) -> *mut c_char {
    match h.as_mut() {
        Some(handle) => {
            let obj: serde_json::Map<String, serde_json::Value> = handle
                .host
                .aux_visible_lines()
                .into_iter()
                .map(|(name, text)| (name, serde_json::Value::String(text)))
                .collect();
            into_owned_cstr(serde_json::Value::Object(obj).to_string())
        }
        None => into_owned_cstr("{}".to_string()),
    }
}

// ============================================================================
// Persistence (shada) — serverless OPFS. The editor's cross-session state (registers,
// marks, history, jumplist, …) is the pure [`PersistState`] core hands out; the Worker
// serializes it to a single JSON blob in OPFS and restores it at boot. This is the
// browser analogue of `bemtvi-server`'s redb store, minus the multi-instance merge a tab
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
