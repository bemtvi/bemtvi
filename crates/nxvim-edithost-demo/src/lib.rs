//! ⚠️⚠️⚠  TEMPORARY — CONCEPT-VALIDATION DEMO ONLY — DELETE ME  ⚠️⚠️⚠
//!
//! **What this proves (and *only* this):** `nxvim-core` (the editor) and
//! `nxvim-lua` (the PUC Lua 5.1 VM + `vim.*` bindings) compile to
//! `wasm32-unknown-emscripten` and run *together* in one wasm module — you can feed
//! vim keys, execute Lua, let Lua drive an `:`-command into the buffer, and read the
//! buffer back. That is the open question Phase 4 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` exists to answer: *does the
//! Lua-bearing editor run in wasm at all?* This says yes, by behavior, in a browser
//! engine (driven here by a headless node harness — `harness.mjs`).
//!
//! **What this is NOT:** it is not the edit-host. The real edit-host reuses
//! `nxvim-server`'s synchronous tick (`dispatch` → `run_pending` → `apply_lua_effects`
//! + the buffer/option/register *mirrors* that let Lua *read* editor state, autocmds,
//! redraw projection, and an async-effect seam for timers/fs/proc). NONE of that is
//! here. This file hand-wires the *crudest possible* tie-in — `editor.input(...)`,
//! `lua.eval(...)`, then drain `take_commands()` into `editor.command(...)` exactly
//! as `effects.rs::apply_lua_effects` does for that one queue — and nothing else.
//! Lua here cannot read the buffer (no mirror), there are no autocmds, no redraw, no
//! async. **Do not build on this.** When the real edit-host lands, delete this crate.

use std::ffi::{c_char, CStr, CString};

use nxvim_core::{parse_keys, Editor};
use nxvim_lua::LuaRuntime;

/// The throwaway demo state: the real editor + the real Lua VM, side by side. The
/// production edit-host would own far more (mirrors, autocmd plumbing, an effect
/// seam); this is deliberately the two raw pieces and nothing between them.
pub struct DemoEditHost {
    editor: Editor,
    lua: LuaRuntime,
}

/// Borrow a C string as `&str` (lossy-empty on bad UTF-8 — fine for a demo).
///
/// # Safety
/// `p` must be a valid, NUL-terminated C string pointer for the call's duration.
unsafe fn as_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    CStr::from_ptr(p).to_str().unwrap_or("")
}

/// Move a `String` out to the JS side as an owned `char*`. The caller MUST hand the
/// pointer back to [`eh_free_string`] (the harness does this in `readStr`).
fn into_owned_cstr(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).unwrap().into_raw()
}

/// Construct the demo: a fresh editor + a fresh Lua VM (empty runtimepath — no
/// plugins/config). Returns null if the Lua VM fails to initialize.
#[no_mangle]
pub extern "C" fn eh_new() -> *mut DemoEditHost {
    let editor = Editor::new();
    let lua = match LuaRuntime::new(Vec::new()) {
        Ok(lua) => lua,
        Err(_) => return std::ptr::null_mut(),
    };
    Box::into_raw(Box::new(DemoEditHost { editor, lua }))
}

/// Feed vim key-notation (e.g. `"ihello<Esc>"`) to the editor — the pure-core input
/// path, identical to what `WebEditor::input` does in the serverless build.
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `notation` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_input(h: *mut DemoEditHost, notation: *const c_char) {
    let Some(host) = h.as_mut() else { return };
    for key in parse_keys(as_str(notation)) {
        host.editor.input(key);
    }
}

/// Execute a Lua chunk, then apply any `:`-commands it queued (via `vim.cmd`) to the
/// editor — the one effect this demo wires, mirroring
/// `effects.rs::apply_lua_effects`'s `take_commands` loop. Returns the eval result
/// rendered as a string (`int` verbatim, else `Debug`), prefixed `ok:` / `err:`.
/// Caller frees the returned pointer with [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed; `code` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn eh_exec_lua(h: *mut DemoEditHost, code: *const c_char) -> *mut c_char {
    let Some(host) = h.as_mut() else {
        return into_owned_cstr("err:null host".into());
    };
    let rendered = match host.lua.eval_to_value(as_str(code)) {
        Ok(value) => {
            let shown = value
                .as_i64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("{value:?}"));
            format!("ok:{shown}")
        }
        Err(e) => format!("err:{e}"),
    };
    // The single editor-affecting effect this demo honors: Lua's queued ex-commands.
    for cmd in host.lua.take_commands() {
        host.editor.command(&cmd);
    }
    // Drain (and discard) captured print/echo output so it can't accumulate.
    let _ = host.lua.take_output();
    into_owned_cstr(rendered)
}

/// Return the current buffer's lines joined by `\n`. Caller frees with
/// [`eh_free_string`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not yet be freed.
#[no_mangle]
pub unsafe extern "C" fn eh_lines(h: *mut DemoEditHost) -> *mut c_char {
    match h.as_ref() {
        Some(host) => into_owned_cstr(host.editor.lines().join("\n")),
        None => into_owned_cstr(String::new()),
    }
}

/// Free a string returned by [`eh_exec_lua`] / [`eh_lines`].
///
/// # Safety
/// `p` must be a pointer previously returned by one of those, freed exactly once.
#[no_mangle]
pub unsafe extern "C" fn eh_free_string(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Destroy a demo host from [`eh_new`].
///
/// # Safety
/// `h` must come from [`eh_new`] and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn eh_free(h: *mut DemoEditHost) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}
