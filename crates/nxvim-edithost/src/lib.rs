//! The wasm (emscripten) edit-host — Phase 5 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`.
//!
//! This drives the **real** synchronous [`EditHost`] tick (the same one
//! `nxvim-server`'s native [`run`](nxvim_server) loop drives — core + the PUC Lua 5.4
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
//! [`eh_fs_read_complete`] / [`eh_fs_write_complete`]). **Tree-sitter indentation** is wired
//! through the [`WasmSyntax`] engine, which calls the worker's web-tree-sitter indenter
//! synchronously over the `eh_js_ts_*` FFI bridge (highlighting stays a UI-thread overlay).
//! LSP / process spawn remain unavailable and fail loud (a later daemon slice re-enables them).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::rc::Rc;

use nxvim_core::{
    BufferEdit, BufferId, Clipboard, DirEntry, Editor, IndentParams, NumberedMark, OpenOutcome,
    PendingSave, PersistState, Span, SyntaxEngine,
};
use nxvim_lua::{BlockingSystem, LuaRuntime, SystemOutput, SystemSpec};
use nxvim_server::{EditHost, HostEffects};
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
}

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
    /// File paths the editor newly asked to **watch** this convergence (the remote watch
    /// leg — Phase 6 watch slice), recorded by [`fs_watch`](HostEffects::fs_watch). Drained
    /// by [`eh_take_watch_requests`] for the Worker to arm on the daemon (`fs_watch [path]`
    /// over WebTransport); a serverless OPFS session has no change source, so the Worker
    /// drops them. A `fs_changed` push the arm yields lands back via [`eh_remote_file_changed`].
    watch_arms: Vec<String>,
    /// File paths the editor newly asked to **stop watching** (a buffer closed / lost its
    /// file), recorded by [`fs_unwatch`](HostEffects::fs_unwatch); the disarm twin of
    /// [`watch_arms`](Sink::watch_arms), drained by [`eh_take_watch_requests`].
    watch_disarms: Vec<String>,
    /// Async process spawns the editor enqueued this convergence (the proc leg — Phase 6d):
    /// `(id, argv, cwd, env, stdin)` per `vim.system` / `jobstart` with an `on_exit`,
    /// recorded by [`proc_spawn`](HostEffects::proc_spawn). Drained by
    /// [`eh_take_proc_requests`] for the Worker to forward over WebTransport
    /// (`proc_spawn [id, argv, cwd?, env, stdin]`); the child's pid/exit return via
    /// [`eh_proc_spawned`] / [`eh_proc_exited`]. Only ever enqueued in a daemon session
    /// (the tick gates on [`daemon_connected`](Sink::daemon_connected)).
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
    /// Terminal PTYs the editor newly asked to **open** this convergence (the web `:terminal`
    /// — Phase 7): `(buf, argv, cwd, rows, cols)` per `:terminal`, recorded by
    /// [`term_open`](HostEffects::term_open). Drained by [`eh_take_terminal_requests`] for the
    /// Worker to forward over WebTransport (`term_open [buf, argv, cwd?, rows, cols]`); the
    /// child's output/exit return as `term_data`/`term_exit` pushes (`eh_terminal_data` /
    /// `eh_terminal_exit`). Only enqueued in a daemon session (the dispatch gates on
    /// [`daemon_connected`](Sink::daemon_connected); serverless OPFS has no PTY host).
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
    /// Whether a daemon (and thus a process host) is currently connected — flipped by the
    /// Worker via [`eh_set_daemon_connected`] on a `?daemon=` boot / runtime `:connect` /
    /// disconnect. Read by [`has_remote_proc`](HostEffects::has_remote_proc) to gate the
    /// editor's async-spawn branch: serverless OPFS has no process host, so a `vim.system`
    /// must fail loud in the tick, never silently enqueue a spawn no transport can fulfil.
    daemon_connected: bool,
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
    /// trailing `\n`, exactly as `nxvim-server`'s native `SystemClipboard` (pbcopy/pbpaste)
    /// treats it — so a value agrees whether read back in-editor or after a round trip
    /// through the OS clipboard. Written by [`WasmClipboard`] (the editor owns it).
    clipboard_get: Option<String>,
    clipboard_writes: Vec<String>,
}

/// The `"+` / `"*` clipboard provider for the browser build — the wasm twin of
/// `nxvim-server`'s `SystemClipboard` (which shells out to pbcopy/pbpaste). The synchronous
/// [`Clipboard`] seam can't await `navigator.clipboard` (it's async, and unreachable off the
/// UI thread anyway), so it bridges through the [`Sink`]: [`get`](Clipboard::get) returns the
/// value the UI last pushed in ([`eh_clipboard_push`]), and [`set`](Clipboard::set) updates
/// that mirror *and* queues the text for the UI to write out ([`eh_take_clipboard_writes`]).
struct WasmClipboard {
    sink: Rc<RefCell<Sink>>,
}

// SAFETY: the entire wasm edit-host runs on the single Web Worker thread, so the `Rc` is
// never sent across threads. The `Send` bound on `Clipboard` exists for `nxvim-server`'s
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

/// The wasm [`SyntaxEngine`]: the browser twin of `nxvim-ts`'s native `Engine`, but it
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
        let Some(state) = self.buffers.get(&buffer) else {
            return false;
        };
        let Some(lang) = Self::lang_cstr(&state.language) else {
            return false;
        };
        unsafe { eh_js_ts_available(lang.as_ptr()) != 0 }
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

    fn fs_watch(&mut self, path: String) {
        // The remote watch leg (Phase 6): record the arm for the Worker to forward to the
        // daemon (`fs_watch [path]` over WebTransport) — the wasm twin of the native
        // `sync_buffer_watches` arming a watch. A serverless OPFS session has no external
        // writer, so the Worker drops it; either way the editor arms uniformly.
        self.sink.borrow_mut().watch_arms.push(path);
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
        // spawn (`nx.run_stream`'s streamed stdout, e.g. a picker source) also streams stdout back
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
        // A `vim.system` is only possible against a connected daemon — serverless OPFS has
        // no process host. The Worker flips this on `:connect` / `?daemon=` (`eh_set_daemon_connected`);
        // when false the tick fails the spawn loud instead of enqueuing it.
        self.sink.borrow().daemon_connected
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

    fn ts_install(&mut self, lang: String) {
        // `:TSInstall <lang>` on the browser build: record the request for the Worker to
        // forward to the UI thread (`eh_take_ts_requests` → `ts_install` postMessage),
        // where web-tree-sitter lives. The UI fetches the prebuilt grammar (offline
        // bundle / OPFS cache / jsDelivr), registers it, and lands the outcome back via
        // `eh_ts_install_complete`. Fire-and-forget — the editor tick doesn't block on it.
        self.sink.borrow_mut().ts_requests.push(lang);
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

/// Drain the remote-watch arm/disarm requests the editor enqueued since the last call, as
/// JSON `{"arm":["…"],"disarm":["…"]}` — the watch leg's outbound half. In a daemon session
/// the Worker forwards each as an `fs_watch` / `fs_unwatch` notification over WebTransport;
/// serverless OPFS has no change source, so the Worker drops them. A `fs_changed` push the
/// daemon sends in response lands back through [`eh_remote_file_changed`]. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_take_watch_requests(h: *mut WasmEditHost) -> *mut c_char {
    let Some(handle) = h.as_mut() else {
        return into_owned_cstr(r#"{"arm":[],"disarm":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
    let arm: Vec<String> = std::mem::take(&mut sink.watch_arms);
    let disarm: Vec<String> = std::mem::take(&mut sink.watch_disarms);
    into_owned_cstr(serde_json::json!({ "arm": arm, "disarm": disarm }).to_string())
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
/// the shared encoding seam ([`crate::encoding::decode_to_rope`]) exactly like the native
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
) {
    let Some(handle) = h.as_mut() else { return };
    let buffer = BufferId(buffer.max(0.0) as u64);
    let path = as_str(path).to_string();
    if kind == 2 {
        handle
            .host
            .complete_fs_read_dir(buffer, path, parse_dir_entries(as_str(contents)));
    } else {
        let bytes = if data.is_null() || len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data, len)
        };
        handle
            .host
            .complete_fs_read(buffer, path, kind, bytes, as_str(contents));
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

// ============================================================================
// Off-tick daemon process leg (Phase 6d). The async `vim.system` / `jobstart` path: the
// editor enqueues a spawn off the keystroke tick (it can't run a process in the browser),
// the Worker forwards it over WebTransport to a connected daemon, and the child's pid/exit
// return as daemon→edit-host pushes the Worker lands back here. The browser twin of the
// native event-loop actor's proc routing (the daemon side is unchanged — Phase 3c/3q).
// ============================================================================

/// Tell the core whether a daemon (process host) is connected, flipping the editor tick's
/// async-spawn branch: `on != 0` enqueues a `vim.system` for the Worker to forward; `0`
/// (serverless OPFS) fails it loud in the tick. The Worker calls this on a `?daemon=` boot /
/// runtime `:connect` (1) and on disconnect (0). Unlike the off-tick fs (always on — OPFS is
/// the serverless fallback), processes have no serverless analogue, so this gate is real.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_set_daemon_connected(h: *mut WasmEditHost, on: i32) {
    if let Some(handle) = h.as_mut() {
        handle.sink.borrow_mut().daemon_connected = on != 0;
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

/// Land a daemon `proc_stdout` push: a streaming child (`nx.run_stream`'s streamed stdout, e.g. a
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
    let stdout = if out.is_null() || out_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(out, out_len).to_vec()
    };
    let stderr = if err.is_null() || err_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(err, err_len).to_vec()
    };
    handle
        .host
        .proc_exited(id.max(0.0) as u64, code as i32, stdout, stderr);
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
        return into_owned_cstr(r#"{"open":[],"write":[],"resize":[],"kill":[],"interrupt":[]}"#.into());
    };
    let mut sink = handle.sink.borrow_mut();
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
    let bytes = if data.is_null() || len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(data, len)
    };
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
