//! The Lua runtime and the beginnings of the `vim.*` standard library.
//!
//! nxvim embeds Lua 5.1 (mlua's `lua51`, the dialect LuaJIT — and therefore
//! neovim — is compatible with). Scripts run inside the *server*, exactly as in
//! neovim, and influence the editor through the same mechanisms RPC clients use.
//!
//! The surface is split in two: editor-touching functions are installed from
//! Rust here (`vim.cmd`, `vim.api.nvim_command`/`nvim_echo`/`nvim_set_hl`,
//! `vim.fn.*`, and the `print` capture), while the broad pure-Lua part of
//! `vim.*` — the table / list / string helpers, `vim.g`/`vim.o`/`vim.opt`/
//! `vim.env`, `vim.iter`, and the registration APIs (`nvim_create_user_command`,
//! `nvim_create_autocmd`, the `vim._fire` autocmd dispatcher) — lives in
//! [`prelude.lua`], run once at init. The data-flow stays "Lua -> queued
//! commands / output / highlights -> core mutation": effects are buffered in
//! [`Shared`] and drained by the server after each chunk.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, LuaOptions, StdLib, Table, Variadic};

/// A highlight-group definition produced by `nvim_set_hl(0, name, opts)`, in
/// the wire-ish form the server translates into `nxvim_core`'s `HlDef`. Colors
/// are kept as the strings the opts table carried (`"#rrggbb"` / `"NONE"` /
/// named, with integer colors normalized to `#rrggbb`); the core parses them,
/// so this crate stays free of any color/registry types.
#[derive(Clone, Debug, Default)]
pub struct HlSet {
    pub name: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub sp: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    pub link: Option<String>,
}

/// A request to the bottom message panel, queued by the `vim.panel.*` functions
/// and drained by the server into the core (which owns the panel state). nxvim's
/// own surface — the panel is not a neovim concept.
#[derive(Clone, Debug)]
pub enum PanelOp {
    /// `vim.panel.open(title, lines[, on_select[, cursor]])` — open (or replace)
    /// and focus the panel. `wants_select` is set when an `on_select` callback
    /// was given, enabling `<CR>` select events. `cursor` is the initially
    /// selected line (0-based; the panel scrolls to keep it visible).
    Open {
        title: String,
        lines: Vec<String>,
        wants_select: bool,
        cursor: usize,
    },
    /// `vim.panel.set_lines(lines)` — replace the open panel's content.
    SetLines(Vec<String>),
    /// `vim.panel.on_select(fn|nil)` — enable/disable `<CR>` select events on the
    /// open panel (the callback itself lives in the Lua registry).
    OnSelect(bool),
    /// `vim.panel.set_cursor(line)` — move the open panel's selection to the
    /// given line (0-based) and scroll it into view.
    SetCursor(usize),
    /// `vim.panel.close()` — close the panel.
    Close,
}

/// Lua registry key under which the panel's `on_select` callback is stored.
const PANEL_ON_SELECT: &str = "nxvim_panel_on_select";

/// Side effects produced by running Lua, drained by the server.
#[derive(Default)]
struct Shared {
    /// Ex-commands requested via `vim.cmd(...)`.
    commands: Vec<String>,
    /// Text emitted via `print(...)` / `vim.api.nvim_echo(...)`.
    output: Vec<String>,
    /// Highlight-group definitions from `nvim_set_hl`, applied to the core
    /// registry after the chunk drains (so the core stays the sole mutator).
    highlights: Vec<HlSet>,
    /// Panel requests from `vim.panel.*`, applied to the core after the chunk.
    panel_ops: Vec<PanelOp>,
}

/// An embedded Lua VM with nxvim's `vim` global installed.
///
/// `!Send` (Lua state is thread-local); it lives on the server's single thread.
pub struct LuaRuntime {
    lua: Lua,
    shared: Rc<RefCell<Shared>>,
    /// The directories Lua searches: their `lua/` feeds `package.path` (so
    /// `require` resolves plugin modules), and their roots hold `colors/`,
    /// `after/`, … for later phases. nxvim's analogue of neovim's runtimepath.
    runtimepath: Vec<PathBuf>,
}

impl LuaRuntime {
    /// Build the VM and point `require` at `runtimepath`: each entry's `lua/`
    /// subdirectory is prepended to `package.path` as `<rt>/lua/?.lua` and
    /// `<rt>/lua/?/init.lua` (the layout neovim plugins ship), so a plugin
    /// dropped on the runtimepath is `require`-able by module name.
    pub fn new(runtimepath: Vec<PathBuf>) -> mlua::Result<Self> {
        // Load the full safe stdlib *plus* `debug`. Real plugins (catppuccin
        // among them) call `debug.getinfo` to locate their own install path, and
        // neovim exposes the full `debug` library to its trusted user config —
        // so nxvim does the same. mlua only permits `debug` via its unsafe
        // constructor (it also re-enables C-module loading, which a user config
        // is already trusted to do); the VM is otherwise the standard safe set.
        let lua = unsafe {
            Lua::unsafe_new_with(StdLib::ALL_SAFE | StdLib::DEBUG, LuaOptions::default())
        };
        let shared = Rc::new(RefCell::new(Shared::default()));
        install_vim(&lua, &shared)?;
        seed_package_path(&lua, &runtimepath)?;
        // The pure-Lua half of `vim.*`, layered over the Rust bridge above.
        lua.load(include_str!("prelude.lua"))
            .set_name("nxvim:prelude")
            .exec()?;
        Ok(LuaRuntime {
            lua,
            shared,
            runtimepath,
        })
    }

    /// The runtimepath this VM searches (read by the colorscheme/`require`
    /// machinery to locate `colors/<name>.lua` and friends).
    pub fn runtimepath(&self) -> &[PathBuf] {
        &self.runtimepath
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

    /// Take the highlight-group definitions queued by `nvim_set_hl` since the
    /// last drain, for the server to apply to the core registry.
    pub fn take_highlights(&self) -> Vec<HlSet> {
        std::mem::take(&mut self.shared.borrow_mut().highlights)
    }

    /// Take the panel requests queued by `vim.panel.*` since the last drain, for
    /// the server to apply to the core (which owns the panel state).
    pub fn take_panel_ops(&self) -> Vec<PanelOp> {
        std::mem::take(&mut self.shared.borrow_mut().panel_ops)
    }

    /// Fire the panel's `on_select` callback for the line at `index` (0-based,
    /// passed to Lua 1-based) with text `line`. A no-op when no callback is
    /// registered. Errors (a throwing handler) are returned for the server to
    /// surface. Called when the user hits `<CR>` on a select-enabled panel.
    pub fn run_panel_select(&self, index: usize, line: &str) -> mlua::Result<()> {
        let cb: Option<mlua::Function> = self.lua.named_registry_value(PANEL_ON_SELECT)?;
        if let Some(f) = cb {
            f.call::<()>((line.to_string(), index as i64 + 1))?;
        }
        Ok(())
    }

    /// Set `vim.g[key] = value` from Rust — used to record `g:colors_name` when
    /// `:colorscheme` loads a theme, so Lua and the editor agree on the name.
    pub fn set_global_var(&self, key: &str, value: &str) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
        let g: Table = vim.get("g")?;
        g.set(key, value)
    }

    /// Fire every autocmd registered for `event` whose pattern matches
    /// `pattern` (used for `ColorScheme` when a theme loads). Delegates to the
    /// prelude's `vim._fire`, which runs callbacks / queues `command` strings;
    /// effects land in [`Shared`] and drain like any other chunk.
    pub fn fire_autocmd(&self, event: &str, pattern: &str) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
        let fire: mlua::Function = vim.get("_fire")?;
        fire.call((event, pattern))
    }

    /// Whether `name` was registered via `nvim_create_user_command` (so the
    /// server can route a deferred `:Name …` to its Lua callback).
    pub fn has_user_command(&self, name: &str) -> bool {
        self.user_command(name)
            .map(|v| !v.is_nil())
            .unwrap_or(false)
    }

    /// Invoke the user command `name` with `args` (the text after the name).
    /// A function command is called with an opts table (`name`, `args`,
    /// `fargs`, `bang`); a string command is queued as an ex-command. Effects
    /// land in [`Shared`] and are drained by the server like any other chunk.
    pub fn run_user_command(&self, name: &str, args: &str) -> mlua::Result<()> {
        match self.user_command(name)? {
            mlua::Value::Function(f) => {
                let opts = self.lua.create_table()?;
                opts.set("name", name)?;
                opts.set("args", args)?;
                let fargs = self.lua.create_table()?;
                for (i, a) in args.split_whitespace().enumerate() {
                    fargs.set(i + 1, a)?;
                }
                opts.set("fargs", fargs)?;
                opts.set("bang", false)?;
                f.call::<()>(opts)
            }
            mlua::Value::String(s) => {
                self.shared
                    .borrow_mut()
                    .commands
                    .push(s.to_str()?.to_string());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Look up the stored `vim._user_commands[name]` entry (function or string).
    fn user_command(&self, name: &str) -> mlua::Result<mlua::Value> {
        let vim: Table = self.lua.globals().get("vim")?;
        let commands: Table = vim.get("_user_commands")?;
        commands.get(name)
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
    // `nvim_set_hl(ns, name, opts)`: capture the group definition for the server
    // to fold into the core registry. `ns` is accepted but ignored (namespace 0
    // only); the full opts shape — colors, the boolean attrs, and `link` — is
    // read here so a colorscheme's hundreds of calls all land.
    let sh = shared.clone();
    api.set(
        "nvim_set_hl",
        lua.create_function(move |_, (_ns, name, opts): (i64, String, Option<Table>)| {
            let mut def = HlSet {
                name,
                ..Default::default()
            };
            if let Some(opts) = &opts {
                def.fg = color_field(opts, "fg")?;
                def.bg = color_field(opts, "bg")?;
                def.sp = color_field(opts, "sp")?;
                def.bold = flag_field(opts, "bold")?;
                def.italic = flag_field(opts, "italic")?;
                def.underline = flag_field(opts, "underline")?;
                def.undercurl = flag_field(opts, "undercurl")?;
                def.strikethrough = flag_field(opts, "strikethrough")?;
                def.reverse = flag_field(opts, "reverse")?;
                def.link = opts.get::<Option<String>>("link")?;
            }
            sh.borrow_mut().highlights.push(def);
            Ok(())
        })?,
    )?;
    vim.set("api", api)?;

    // `vim.panel`: nxvim's scriptable handle on the bottom message panel
    // (`:messages` / `:ls`'s home). Each call queues a [`PanelOp`] the server
    // drains into the core after the chunk runs — same "Lua queues, core
    // mutates" flow as `vim.cmd` / `nvim_set_hl`.
    let panel = lua.create_table()?;
    // `open(title, lines[, on_select[, cursor]])`: `on_select` is a
    // `function(line, index)` called when the user hits `<CR>` on a line (index
    // is 1-based). It is stored in the Lua registry; passing it enables select
    // events for this panel. `cursor` is the initially selected line (1-based,
    // matching the `on_select` index); it defaults to the first line.
    let sh = shared.clone();
    panel.set(
        "open",
        lua.create_function(
            move |lua,
                  (title, lines, on_select, cursor): (
                String,
                Option<Vec<String>>,
                Option<mlua::Function>,
                Option<usize>,
            )| {
                store_panel_callback(lua, on_select.clone())?;
                sh.borrow_mut().panel_ops.push(PanelOp::Open {
                    title,
                    lines: lines.unwrap_or_default(),
                    wants_select: on_select.is_some(),
                    cursor: cursor.map(|c| c.saturating_sub(1)).unwrap_or(0),
                });
                Ok(())
            },
        )?,
    )?;
    let sh = shared.clone();
    panel.set(
        "set_lines",
        lua.create_function(move |_, lines: Vec<String>| {
            sh.borrow_mut().panel_ops.push(PanelOp::SetLines(lines));
            Ok(())
        })?,
    )?;
    // `on_select(fn|nil)`: set or clear the open panel's `<CR>` handler.
    let sh = shared.clone();
    panel.set(
        "on_select",
        lua.create_function(move |lua, on_select: Option<mlua::Function>| {
            store_panel_callback(lua, on_select.clone())?;
            sh.borrow_mut()
                .panel_ops
                .push(PanelOp::OnSelect(on_select.is_some()));
            Ok(())
        })?,
    )?;
    // `set_cursor(line)`: move the open panel's selection (1-based, matching the
    // `on_select` index) and scroll it into view.
    let sh = shared.clone();
    panel.set(
        "set_cursor",
        lua.create_function(move |_, line: usize| {
            sh.borrow_mut()
                .panel_ops
                .push(PanelOp::SetCursor(line.saturating_sub(1)));
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    panel.set(
        "close",
        lua.create_function(move |lua, ()| {
            store_panel_callback(lua, None)?; // drop the handler with the panel
            sh.borrow_mut().panel_ops.push(PanelOp::Close);
            Ok(())
        })?,
    )?;
    vim.set("panel", panel)?;

    // `vim.fn`: the Vimscript builtins the load path calls. Only the ones that
    // need real filesystem / environment access are Rust-backed; the rest of
    // `vim.*` is pure Lua in the prelude.
    let func = lua.create_table()?;
    func.set(
        "stdpath",
        lua.create_function(|_, what: String| Ok(stdpath(&what)))?,
    )?;
    func.set(
        "getftime",
        lua.create_function(|_, path: String| Ok(getftime(&path)))?,
    )?;
    func.set(
        "isdirectory",
        lua.create_function(|_, path: String| {
            Ok(if std::path::Path::new(&path).is_dir() {
                1i64
            } else {
                0
            })
        })?,
    )?;
    func.set(
        "mkdir",
        lua.create_function(|_, (path, _flags): (String, Option<String>)| {
            // `mkdir(path, "p")` — create parents; return 1 on success, 0 on
            // failure, matching Vimscript's truthy/falsey convention.
            Ok(if std::fs::create_dir_all(&path).is_ok() {
                1i64
            } else {
                0
            })
        })?,
    )?;
    func.set(
        "has",
        lua.create_function(|_, feature: String| {
            // Claim a modern neovim so version-gated code takes its full path;
            // unknown features report absent. Refined as needs surface.
            Ok(if feature.starts_with("nvim-") {
                1i64
            } else {
                0
            })
        })?,
    )?;
    vim.set("fn", func)?;

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

/// Store (or clear) the panel's `on_select` callback in the Lua registry. `None`
/// stores `nil`, so [`LuaRuntime::run_panel_select`] reads it back as "no
/// handler" — keeping a closed/replaced panel from firing a stale callback.
fn store_panel_callback(lua: &Lua, cb: Option<mlua::Function>) -> mlua::Result<()> {
    match cb {
        Some(f) => lua.set_named_registry_value(PANEL_ON_SELECT, f),
        None => lua.set_named_registry_value(PANEL_ON_SELECT, mlua::Value::Nil),
    }
}

/// Prepend each runtimepath entry's `lua/` directory to Lua's `package.path`,
/// so `require("foo")` finds `<rt>/lua/foo.lua` or `<rt>/lua/foo/init.lua`. The
/// stock `package.path` is kept as a suffix. No-op when the runtimepath is empty.
fn seed_package_path(lua: &Lua, runtimepath: &[PathBuf]) -> mlua::Result<()> {
    if runtimepath.is_empty() {
        return Ok(());
    }
    let mut patterns: Vec<String> = Vec::with_capacity(runtimepath.len() * 2);
    for rt in runtimepath {
        let lua_dir = rt.join("lua");
        patterns.push(lua_dir.join("?.lua").to_string_lossy().into_owned());
        patterns.push(
            lua_dir
                .join("?")
                .join("init.lua")
                .to_string_lossy()
                .into_owned(),
        );
    }
    let package: Table = lua.globals().get("package")?;
    let existing: String = package.get("path").unwrap_or_default();
    let combined = if existing.is_empty() {
        patterns.join(";")
    } else {
        format!("{};{existing}", patterns.join(";"))
    };
    package.set("path", combined)?;
    Ok(())
}

/// Resolve a `vim.fn.stdpath(what)` directory under an `nxvim` subdir, the way
/// neovim derives its standard paths from XDG (with `$HOME` fallbacks). `config`
/// additionally honors `$NXVIM_CONFIG`. Unknown `what` falls back to the cache
/// dir rather than erroring.
fn stdpath(what: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| -> PathBuf {
        if let Some(dir) = std::env::var_os(var) {
            PathBuf::from(dir).join("nxvim")
        } else if let Some(home) = &home {
            home.join(fallback).join("nxvim")
        } else {
            PathBuf::from("nxvim")
        }
    };
    let path = match what {
        "config" => std::env::var_os("NXVIM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config")),
        "data" => xdg("XDG_DATA_HOME", ".local/share"),
        "state" => xdg("XDG_STATE_HOME", ".local/state"),
        _ => xdg("XDG_CACHE_HOME", ".cache"),
    };
    path.to_string_lossy().into_owned()
}

/// `vim.fn.getftime(path)`: the file's mtime in whole seconds since the Unix
/// epoch, or `-1` if it can't be stat'd (matching Vimscript).
fn getftime(path: &str) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

/// Read a color field (`fg`/`bg`/`sp`) from an `nvim_set_hl` opts table. A
/// string (`"#rrggbb"` / `"NONE"` / a name) is kept verbatim; an integer color
/// is normalized to `#rrggbb`; anything else (incl. absent) is `None`. The core
/// does the actual parsing.
fn color_field(opts: &Table, key: &str) -> mlua::Result<Option<String>> {
    match opts.get::<mlua::Value>(key)? {
        mlua::Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        mlua::Value::Integer(n) => Ok(Some(format!("#{:06x}", n & 0xff_ffff))),
        mlua::Value::Number(n) => Ok(Some(format!("#{:06x}", (n as i64) & 0xff_ffff))),
        _ => Ok(None),
    }
}

/// Read a boolean attribute (`bold`, `italic`, …) from an opts table; absent or
/// non-boolean reads as `false`.
fn flag_field(opts: &Table, key: &str) -> mlua::Result<bool> {
    Ok(opts.get::<Option<bool>>(key)?.unwrap_or(false))
}

fn stringify(lua: &Lua, value: &mlua::Value) -> String {
    // Prefer Lua's own tostring (honors __tostring); fall back to a debug form.
    match lua.coerce_string(value.clone()) {
        Ok(Some(s)) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        _ => format!("{value:?}"),
    }
}
