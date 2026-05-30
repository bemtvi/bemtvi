//! The Lua runtime and the beginnings of the `vim.*` standard library.
//!
//! nxvim embeds Lua 5.1 (mlua's `lua51`, the dialect LuaJIT — and therefore
//! neovim — is compatible with). Scripts run inside the *server*, exactly as in
//! neovim, and influence the editor through the same mechanisms RPC clients use.
//!
//! For now the bridge is intentionally narrow: `vim.cmd` queues ex-commands and
//! `print`/`vim.api.nvim_echo` capture output. Both are drained by the server
//! after each chunk runs. Binding the full `nvim_*` API surface is future work,
//! but the data-flow (Lua -> queued API calls -> core mutation) is the target
//! shape already.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Variadic};

/// Side effects produced by running Lua, drained by the server.
#[derive(Default)]
struct Shared {
    /// Ex-commands requested via `vim.cmd(...)`.
    commands: Vec<String>,
    /// Text emitted via `print(...)` / `vim.api.nvim_echo(...)`.
    output: Vec<String>,
}

/// An embedded Lua VM with nxvim's `vim` global installed.
///
/// `!Send` (Lua state is thread-local); it lives on the server's single thread.
pub struct LuaRuntime {
    lua: Lua,
    shared: Rc<RefCell<Shared>>,
}

impl LuaRuntime {
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let shared = Rc::new(RefCell::new(Shared::default()));
        install_vim(&lua, &shared)?;
        Ok(LuaRuntime { lua, shared })
    }

    /// Run a Lua chunk. Errors are returned for the server to surface.
    pub fn exec(&self, chunk: &str) -> mlua::Result<()> {
        self.lua.load(chunk).exec()
    }

    /// Take ex-commands queued by `vim.cmd` since the last drain.
    pub fn take_commands(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().commands)
    }

    /// Take captured `print` output since the last drain.
    pub fn take_output(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().output)
    }
}

fn install_vim(lua: &Lua, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    let vim = lua.create_table()?;

    let sh = shared.clone();
    vim.set(
        "cmd",
        lua.create_function(move |_, cmd: String| {
            sh.borrow_mut().commands.push(cmd);
            Ok(())
        })?,
    )?;

    vim.set("version", "nxvim 0.1.0")?;

    // A minimal `vim.api` namespace; grows toward the full nvim_* surface.
    let api = lua.create_table()?;
    let sh = shared.clone();
    api.set(
        "nvim_command",
        lua.create_function(move |_, cmd: String| {
            sh.borrow_mut().commands.push(cmd);
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    api.set(
        "nvim_echo",
        lua.create_function(move |_, msg: String| {
            sh.borrow_mut().output.push(msg);
            Ok(())
        })?,
    )?;
    vim.set("api", api)?;

    lua.globals().set("vim", vim)?;

    // Capture `print` so output can be shown on the message line.
    let sh = shared.clone();
    lua.globals().set(
        "print",
        lua.create_function(move |lua, args: Variadic<mlua::Value>| {
            let parts: Vec<String> = args.iter().map(|v| stringify(lua, v)).collect();
            sh.borrow_mut().output.push(parts.join("\t"));
            Ok(())
        })?,
    )?;

    Ok(())
}

fn stringify(lua: &Lua, value: &mlua::Value) -> String {
    // Prefer Lua's own tostring (honors __tostring); fall back to a debug form.
    match lua.coerce_string(value.clone()) {
        Ok(Some(s)) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        _ => format!("{value:?}"),
    }
}
