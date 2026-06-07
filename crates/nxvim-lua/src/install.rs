//! Installing nxvim's `vim.*` bridge into a fresh Lua VM. Two passes, split only
//! by what they need to capture: [`install_vim`] wires the editor-touching
//! funnels that need just the [`Shared`] effect buffer (`vim.cmd`, `vim.api.*`,
//! `vim.panel.*`, the async-loop queue, the `vim.fn` basics, `print`), and
//! [`install_runtime_api`] adds the rest — the functions that also need the host
//! filesystem / environment / runtimepath (LSP queueing, `vim.uv`, the
//! filesystem `vim.fn.*`, the JSON / regex / process primitives). The pure-Lua
//! half of `vim.*` is layered on top from the `src/prelude/` modules by [`crate::LuaRuntime::new`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, Table, Variadic};

use crate::convert::{
    color_field, env_pairs, flag_field, json_to_lua, lua_to_json, opt_table_to_json, stringify,
};
use crate::host::{
    create_dir_all_mode, find_executable, get_runtime_file, getftime, glob_paths, parse_mode,
    stdpath,
};
use crate::ops::{
    BufOp, ConfirmReq, GlobalOptionOp, HlSet, LoopOp, LspOp, OptionValue, PanelOp, UiInputReq,
    WindowOp,
};
use crate::runtime::Shared;
use crate::vimregex;

/// Lua registry key under which the panel's `on_select` callback is stored.
pub(crate) const PANEL_ON_SELECT: &str = "nxvim_panel_on_select";

pub(crate) fn install_vim(lua: &Lua, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
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
    // INCOMPLETE: a non-zero `ns` is silently folded into the global namespace
    // instead of a separate one, so `nvim_win_set_hl_ns` / per-window highlight
    // namespaces can't be honored — a config that sets group X in two namespaces
    // gets last-write-wins on the global table. A real impl would key HlSet by ns.
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

    // ----- the async runtime bridge (the "event loop") -----------------------
    // Lua queues a [`LoopOp`] carrying a callback id; the server drains it in
    // `apply_lua_effects` and either services it directly (`Schedule`) or forwards
    // it to the background event-loop actor (timers, processes). Same "Lua queues,
    // the server drives" flow as `vim.cmd` / panel / lsp ops — the callback itself
    // stays in the Lua registry (`vim._cb_fns[id]`) and runs on the server thread.

    // `vim._schedule(id)`: defer callback `id` to the end of the current
    // convergence (the strict, non-nested `vim.schedule`).
    let sh = shared.clone();
    vim.set(
        "_schedule",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::Schedule { id });
            Ok(())
        })?,
    )?;
    // `vim._timer_start(id, delay_ms, repeat_ms)`: arm a timer firing callback
    // `id` after `delay_ms`, then every `repeat_ms` (`0` ⇒ one-shot).
    let sh = shared.clone();
    vim.set(
        "_timer_start",
        lua.create_function(move |_, (id, delay_ms, repeat_ms): (u64, u64, u64)| {
            sh.borrow_mut().loop_ops.push(LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            });
            Ok(())
        })?,
    )?;
    // `vim._timer_stop(id)`: cancel the timer armed under `id`.
    let sh = shared.clone();
    vim.set(
        "_timer_stop",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::TimerStop { id });
            Ok(())
        })?,
    )?;
    // `vim._system_async(id, cmd, cwd, env)`: spawn `cmd` (an argv list) in the
    // event-loop actor and run callback `id` with `{ code, stdout, stderr }` when
    // it exits — the off-tick `vim.system`. Returns the child's OS pid immediately
    // (the actor sends it back over a oneshot the bridge blocks on *briefly* — only
    // until the spawn itself completes, not the run), so the `vim.system` handle
    // carries a real pid while the wait stays async. A spawn failure surfaces as a
    // `nil` pid (the `on_exit` still fires later with `code = -1`).
    let sh = shared.clone();
    vim.set(
        "_system_async",
        lua.create_function(
            move |_, (id, cmd, cwd, env): (u64, Vec<String>, Option<String>, Option<Table>)| {
                let env = env_pairs(env)?;
                sh.borrow_mut()
                    .loop_ops
                    .push(LoopOp::Spawn { id, cmd, cwd, env });
                Ok(())
            },
        )?,
    )?;
    // `vim._system_kill(id, signal)`: terminate the async child running under
    // `id`. `signal` is accepted (neovim's `handle:kill(signal)`) but ignored —
    // the actor terminates the child unconditionally (see [`LoopOp::Kill`]).
    let sh = shared.clone();
    vim.set(
        "_system_kill",
        lua.create_function(move |_, (id, _signal): (u64, Option<i32>)| {
            sh.borrow_mut().loop_ops.push(LoopOp::Kill { id });
            Ok(())
        })?,
    )?;

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
        lua.create_function(
            |_, (path, _flags, prot): (String, Option<String>, Option<mlua::Value>)| {
                // `mkdir(path, "p" [, prot])` — create parents; return 1 on
                // success, 0 on failure (Vimscript's truthy/falsey convention).
                // `prot` is the permission mask (neovim's third arg): an octal
                // string like "0700" or a numeric mode. Honoring it means a
                // private data/state dir isn't silently created world-readable.
                Ok(if create_dir_all_mode(&path, parse_mode(prot)) {
                    1i64
                } else {
                    0
                })
            },
        )?,
    )?;
    func.set(
        "has",
        lua.create_function(|_, feature: String| {
            // Claim a modern neovim so version-gated code takes its full path;
            // unknown features report absent. Refined as needs surface.
            // INCOMPLETE: a coarse heuristic, not a real feature table. EVERY
            // `nvim-X.Y` returns 1 — including future/bogus versions
            // (`has('nvim-0.99')` → 1, wrong) — and every non-`nvim-` feature
            // returns 0, including real ones nxvim could answer (`unix`, `mac`,
            // `win32`, `gui_running`, …). A real impl would consult an actual
            // feature/version table instead of pattern-matching the prefix.
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

/// The argument tuple of `vim._lsp_start`: the original five
/// (`name`, `cmd`, `root`, `filetype`, `bufnr`) plus the Phase-2 config payloads
/// (`init_options`, `settings`, `capabilities`, each a table or `nil`).
type LspStartArgs = (
    String,
    Vec<String>,
    Option<String>,
    String,
    u64,
    Option<Table>,
    Option<Table>,
    Option<Table>,
);

/// Install the `vim.*` functions that need the host filesystem / environment /
/// runtimepath and feed the LSP framework (Phase 7a): `nvim_get_runtime_file`
/// (runtimepath `lsp/` discovery), `vim.fn.getcwd`, the `vim._read_file` /
/// `vim._readdir` filesystem primitives the pure-Lua `vim.fs` builds on, and
/// `vim._lsp_start` (the queue `vim.lsp.start` pushes onto). Separated from
/// [`install_vim`] because these capture the runtimepath, known only here.
pub(crate) fn install_runtime_api(
    lua: &Lua,
    shared: &Rc<RefCell<Shared>>,
    runtimepath: &[PathBuf],
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;
    let func: Table = vim.get("fn")?;

    // `nvim_get_runtime_file(name, all)`: full paths of files matching `name`
    // (a runtimepath-relative path, the final component optionally globbed with
    // `*`) across the runtimepath. `all=false` returns the first match only. The
    // `lsp/<server>.lua` config-discovery primitive.
    let rtp = runtimepath.to_vec();
    api.set(
        "nvim_get_runtime_file",
        lua.create_function(move |lua, (name, all): (String, Option<bool>)| {
            let hits = get_runtime_file(&rtp, &name, all.unwrap_or(false));
            lua.create_sequence_from(hits)
        })?,
    )?;

    // `vim.fn.getcwd()`: the process working directory (the root fallback and the
    // base for relative->absolute path math in `vim.fs`/`fnamemodify`).
    func.set(
        "getcwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default())
        })?,
    )?;

    // `vim._read_file(path)`: the file's contents, or nil if unreadable. Backs the
    // pure-Lua loader that sources an `lsp/<name>.lua` config (via `loadstring`),
    // sidestepping any `loadfile` sandbox question.
    vim.set(
        "_read_file",
        lua.create_function(|_, path: String| Ok(std::fs::read_to_string(&path).ok()))?,
    )?;

    // `vim._readdir(path)`: the entry names directly under `path` (no `.`/`..`),
    // or an empty list if it can't be read. Backs `vim.fs.find`/predicate markers.
    vim.set(
        "_readdir",
        lua.create_function(|lua, path: String| {
            let names: Vec<String> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            lua.create_sequence_from(names)
        })?,
    )?;

    // `vim._lsp_start(name, cmd, root, filetype, bufnr, init_options, settings,
    // capabilities)`: queue an [`LspOp::Start`] for the server to drain. The
    // Lua-facing `vim.lsp.start` wrapper (prelude) resolves the config and root,
    // then calls this. The trailing three are the config's `init_options` /
    // `settings` / `capabilities` tables (each `nil` when unset); they convert
    // through the same `lua_to_json` bridge `vim.json.encode` uses, so the server
    // forwards them at `initialize` exactly as the config wrote them (Phase 2).
    let sh = shared.clone();
    vim.set(
        "_lsp_start",
        lua.create_function(move |_, args: LspStartArgs| {
            let (name, cmd, root, filetype, bufnr, init_options, settings, capabilities) = args;
            sh.borrow_mut().lsp_ops.push(LspOp::Start {
                name,
                cmd,
                root,
                filetype,
                bufnr,
                init_options: opt_table_to_json(init_options)?,
                settings: opt_table_to_json(settings)?,
                capabilities: opt_table_to_json(capabilities)?,
            });
            Ok(())
        })?,
    )?;

    // `vim._buf_set_lines(bufnr, start, end, repl)`: queue a [`BufOp::SetLines`]
    // for the server to apply to the live editor (Phase 6). The Lua-facing
    // `vim.api.nvim_buf_set_lines` wrapper (prelude) has already updated the
    // `vim._bufs` mirror (write-through) and resolved `bufnr` to a concrete id; the
    // server normalizes the indices and converts the line range to a byte range.
    let sh = shared.clone();
    vim.set(
        "_buf_set_lines",
        lua.create_function(
            move |_, (bufnr, start, end, repl): (u64, i64, i64, Vec<String>)| {
                sh.borrow_mut().buf_ops.push(BufOp::SetLines {
                    bufnr,
                    start,
                    end,
                    repl,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._buf_set_option(bufnr, name, value)`: queue a [`BufOp::SetOption`] for
    // the server to apply to the live editor's buffer (Phase 6). The prelude
    // (`vim.bo` / `nvim_set_option_value`) has canonicalized `name` and updated
    // its option mirror (write-through); a number value rides as `Number`, a
    // boolean as `Bool`. Other Lua types are ignored (the option set is typed:
    // tabstop/shiftwidth are numbers, expandtab a boolean).
    let sh = shared.clone();
    vim.set(
        "_buf_set_option",
        lua.create_function(move |_, (bufnr, name, value): (u64, String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(n)),
                mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                _ => None,
            };
            if let Some(value) = value {
                sh.borrow_mut()
                    .buf_ops
                    .push(BufOp::SetOption { bufnr, name, value });
            }
            Ok(())
        })?,
    )?;

    // `vim._set_global_option(name, value)`: queue a [`GlobalOptionOp`] for the
    // server to apply to the editor's global options. The prelude (`vim.o`) has
    // canonicalized `name` and written through its `vim._go_mirror`; the wired
    // global options are all boolean, but a number rides as `Number` for symmetry
    // with the buffer/window bridges. Other Lua types are ignored.
    let sh = shared.clone();
    vim.set(
        "_set_global_option",
        lua.create_function(move |_, (name, value): (String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(n)),
                mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                _ => None,
            };
            if let Some(value) = value {
                sh.borrow_mut()
                    .global_ops
                    .push(GlobalOptionOp { name, value });
            }
            Ok(())
        })?,
    )?;

    // `vim._win_op(...)`: the window-mutation bridges (Phase 5). Each queues a
    // [`WindowOp`] the server drains into the live editor after the chunk; the
    // Lua-facing `vim.api.nvim_win_*` wrappers (prelude) have already updated the
    // `vim._wins` mirror (write-through) where a read-after-write needs it.
    let sh = shared.clone();
    vim.set(
        "_set_current_win",
        lua.create_function(move |_, win: u64| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetCurrent { win });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_set_buf",
        lua.create_function(move |_, (win, buf): (u64, u64)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetBuf { win, buf });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_set_cursor",
        lua.create_function(move |_, (win, line, col): (u64, usize, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetCursor { win, line, col });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_set_width",
        lua.create_function(move |_, (win, width): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetWidth { win, width });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_set_height",
        lua.create_function(move |_, (win, height): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetHeight { win, height });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_set_option",
        lua.create_function(move |_, (win, name, value): (u64, String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(n)),
                mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                _ => None,
            };
            if let Some(value) = value {
                sh.borrow_mut()
                    .window_ops
                    .push(WindowOp::SetOption { win, name, value });
            }
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_win_close",
        lua.create_function(move |_, (win, force): (u64, bool)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::Close { win, force });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    vim.set(
        "_open_win",
        lua.create_function(move |_, (buf, vertical, enter): (u64, bool, bool)| {
            sh.borrow_mut().window_ops.push(WindowOp::Open {
                buf,
                vertical,
                enter,
            });
            Ok(())
        })?,
    )?;
    // `vim._open_float(cfg)`: queue the float form of `nvim_open_win`. The prelude
    // builds `cfg` (a validated table of primitive fields) and calls this; the
    // server drains the op into `Editor::open_float_window`. The split form keeps
    // its own `_open_win` bridge above.
    let sh = shared.clone();
    vim.set(
        "_open_float",
        lua.create_function(move |_, cfg: Table| {
            sh.borrow_mut().window_ops.push(WindowOp::OpenFloat {
                buf: cfg.get("buf")?,
                enter: cfg.get("enter")?,
                relative: cfg.get("relative")?,
                win: cfg.get::<Option<u64>>("win")?.unwrap_or(0),
                anchor: cfg.get("anchor")?,
                row: cfg.get("row")?,
                col: cfg.get("col")?,
                width: cfg.get("width")?,
                height: cfg.get("height")?,
                zindex: cfg.get("zindex")?,
                focusable: cfg.get("focusable")?,
                border: cfg.get("border")?,
                title: cfg.get::<Option<String>>("title")?,
            });
            Ok(())
        })?,
    )?;
    // `vim._win_set_config(win, cfg)`: queue the partial reconfigure of
    // `nvim_win_set_config`. `cfg` carries only the keys the caller passed (the
    // prelude already validated the enumerated strings); a missing key stays
    // `None` so the core leaves it unchanged. `relative = ""` rides through as the
    // re-tile form.
    let sh = shared.clone();
    vim.set(
        "_win_set_config",
        lua.create_function(move |_, (win, cfg): (u64, Table)| {
            sh.borrow_mut().window_ops.push(WindowOp::SetConfig {
                win,
                relative: cfg.get::<Option<String>>("relative")?,
                parent: cfg.get::<Option<u64>>("win")?.unwrap_or(0),
                anchor: cfg.get::<Option<String>>("anchor")?,
                row: cfg.get::<Option<i64>>("row")?,
                col: cfg.get::<Option<i64>>("col")?,
                width: cfg.get::<Option<u64>>("width")?,
                height: cfg.get::<Option<u64>>("height")?,
                zindex: cfg.get::<Option<u32>>("zindex")?,
                focusable: cfg.get::<Option<bool>>("focusable")?,
                border: cfg.get::<Option<String>>("border")?,
                title: cfg.get::<Option<String>>("title")?,
            });
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf(kind)`: queue a position-family `vim.lsp.buf.*` request
    // ([`LspOp::BufRequest`]) or one of the edit ops (`Format`/`CodeAction`),
    // selected by the `LspReqKind::as_u16` the prelude passes. The single Rust
    // entry the bare `vim.lsp.buf` functions route through (rename has its own,
    // below, since it carries an argument).
    let sh = shared.clone();
    vim.set(
        "_lsp_buf",
        lua.create_function(move |_, kind: u16| {
            sh.borrow_mut().lsp_ops.push(LspOp::BufRequest { kind });
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_format()`: queue [`LspOp::Format`]. Kept distinct from
    // `_lsp_buf` because formatting has no `{uri, position}` shape (it routes to
    // `request_lsp_format`, not `request_lsp`).
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_format",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::Format);
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_code_action()`: queue [`LspOp::CodeAction`].
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_code_action",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::CodeAction);
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_rename(name)`: queue [`LspOp::Rename`]. The prelude requires
    // the argument (echoing `E471` on nil), so a name always arrives here.
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_rename",
        lua.create_function(move |_, new_name: String| {
            sh.borrow_mut().lsp_ops.push(LspOp::Rename { new_name });
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_goto(forward, severity)`: queue [`LspOp::DiagnosticGoto`]
    // — the cursor move `vim.diagnostic.goto_next`/`goto_prev` drive.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_goto",
        lua.create_function(move |_, (forward, severity): (bool, Option<u16>)| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticGoto {
                forward,
                severity: severity.map(|s| s as u8),
            });
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_setloclist()`: queue [`LspOp::DiagnosticSetloclist`] — open
    // the diagnostics location-list panel.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_setloclist",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticSetloclist);
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_config(underline)`: queue [`LspOp::DiagnosticConfig`] — the
    // prelude resolves the merged `underline` to a bool and pushes it so the
    // server gates the squiggle rendering.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_config",
        lua.create_function(move |_, underline: bool| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::DiagnosticConfig { underline });
            Ok(())
        })?,
    )?;

    // `vim._lsp_client_request(client_id, method, params, cb_id)`: queue a generic
    // `client:request` ([`LspOp::ClientRequest`]). The handler is already stored in
    // `vim._cb_fns[cb_id]` by the Lua wrapper; the server forwards the request and
    // runs the callback with `(err, result)` when the reply lands (Phase 5).
    // `params` is any Lua value (a table / nil), converted through the same
    // `lua_to_json` bridge `vim.json.encode` uses.
    let sh = shared.clone();
    vim.set(
        "_lsp_client_request",
        lua.create_function(
            move |_, (client_id, method, params, cb_id): (u64, String, mlua::Value, u64)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ClientRequest {
                    client_id,
                    method,
                    params: lua_to_json(&params)?,
                    cb_id,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._lsp_client_notify(client_id, method, params)`: queue a generic
    // fire-and-forget `client:notify` ([`LspOp::ClientNotify`]).
    let sh = shared.clone();
    vim.set(
        "_lsp_client_notify",
        lua.create_function(
            move |_, (client_id, method, params): (u64, String, mlua::Value)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ClientNotify {
                    client_id,
                    method,
                    params: lua_to_json(&params)?,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._lsp_apply_workspace_edit(edit)`: queue [`LspOp::ApplyWorkspaceEdit`]
    // (Phase 7). `edit` is the LSP-shape WorkspaceEdit table, converted to JSON
    // through the same `lua_to_json` bridge `client:request` params use; the server
    // deserializes, normalizes, and applies it across the open buffers it names.
    let sh = shared.clone();
    vim.set(
        "_lsp_apply_workspace_edit",
        lua.create_function(move |_, edit: mlua::Value| {
            sh.borrow_mut().lsp_ops.push(LspOp::ApplyWorkspaceEdit {
                edit: lua_to_json(&edit)?,
            });
            Ok(())
        })?,
    )?;

    // `vim._lsp_show_document(uri, line, character, encoding)`: queue
    // [`LspOp::ShowDocument`] (Phase 7) — the server builds an LSP location and
    // reuses the native single-location goto (open + cursor jump).
    let sh = shared.clone();
    vim.set(
        "_lsp_show_document",
        lua.create_function(
            move |_, (uri, line, character, encoding): (String, u32, u32, String)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ShowDocument {
                    uri,
                    line,
                    character,
                    encoding,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._ui_input(prompt, default, cb_id)`: queue a `vim.ui.input` prompt
    // ([`UiInputReq`]). The server opens the editor's command line labelled
    // `prompt` (prefilled with `default`) and fires `vim._cb_fns[cb_id]` with the
    // typed text — or `nil` on cancel — when the user submits (Phase 8).
    let sh = shared.clone();
    vim.set(
        "_ui_input",
        lua.create_function(move |_, (prompt, default, cb_id): (String, String, u64)| {
            sh.borrow_mut().ui_inputs.push(UiInputReq {
                prompt,
                default,
                cb_id,
            });
            Ok(())
        })?,
    )?;

    // `vim._confirm(label, accelerators, default, cb_id)`: queue a `vim.fn.confirm`
    // button dialog ([`ConfirmReq`]). The server opens the command line as a
    // single-key confirm prompt showing `label`; a keypress matching one of
    // `accelerators` (or `<CR>` → `default`, `<Esc>` → 0) resolves it, firing
    // `vim._cb_fns[cb_id]` with the chosen 1-based index to resume the blocked
    // `vim.fn.confirm` call.
    let sh = shared.clone();
    vim.set(
        "_confirm",
        lua.create_function(
            move |_, (label, accelerators, default, cb_id): (String, Vec<String>, i64, u64)| {
                sh.borrow_mut().confirms.push(ConfirmReq {
                    label,
                    accelerators,
                    default,
                    cb_id,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._ui_opener()`: the OS file/URL opener argv prefix `vim.ui.open` spawns
    // (via the async `vim.system`), chosen by platform — `open` on macOS,
    // `xdg-open` elsewhere (Phase 8). The path is appended by the Lua wrapper.
    vim.set(
        "_ui_opener",
        lua.create_function(|_, ()| {
            Ok(match std::env::consts::OS {
                "macos" => vec!["open".to_string()],
                "windows" => vec!["explorer".to_string()],
                _ => vec!["xdg-open".to_string()],
            })
        })?,
    )?;

    // `vim._substitute(input, pat, sub, flags)`: the engine behind
    // `vim.fn.substitute` — a real vim-regex substitution (vim's magic dialect +
    // replacement syntax, NOT nxvim's standard-regex `/` search). An invalid or
    // unsupported pattern raises (fail loud), never a fake identity result.
    vim.set(
        "_substitute",
        lua.create_function(
            |_, (input, pat, sub, flags): (String, String, String, String)| {
                vimregex::substitute(&input, &pat, &sub, &flags).map_err(mlua::Error::RuntimeError)
            },
        )?,
    )?;

    // `vim._system(cmd, cwd, env, text)`: spawn `cmd` (an argv list — no shell),
    // block until it exits, and return `{ code, stdout, stderr }`. The pure-Lua
    // `vim.system` wrapper layers neovim's object shape (`:wait()` / `on_exit`)
    // over it. It is synchronous because the Lua VM has no event loop yet (see
    // `vim.schedule`): an `lsp/<server>.lua` `root_dir` that shells out — e.g.
    // rust_analyzer's `cargo metadata` / `rustc --print sysroot` — runs to
    // completion inline on the input tick. Unlike neovim, a spawn failure (a
    // missing tool) degrades to `code = -1` with the message on `stderr` instead
    // of raising, so it can never break `vim.lsp.enable` on a machine that lacks
    // the toolchain. stdout/stderr are returned as Lua byte strings (so non-UTF-8
    // output survives), independent of the `text` flag, which is accepted and
    // ignored.
    vim.set(
        "_system",
        lua.create_function(
            |lua,
             (cmd, cwd, env, _text): (
                Vec<String>,
                Option<String>,
                Option<Table>,
                Option<bool>,
            )| {
                let Some((program, args)) = cmd.split_first() else {
                    return Err(mlua::Error::external(
                        "vim.system: cmd must be a non-empty list",
                    ));
                };
                let mut command = std::process::Command::new(program);
                command.args(args).stdin(std::process::Stdio::null());
                if let Some(dir) = cwd {
                    command.current_dir(dir);
                }
                if let Some(env_tbl) = env {
                    for (k, v) in env_tbl.pairs::<String, String>().flatten() {
                        command.env(k, v);
                    }
                }
                let result = lua.create_table()?;
                // Spawn (capturing the real pid) then wait, rather than `output()`,
                // so the synchronous handle's `result.pid` is a real pid — parity
                // with the async path. The wait is short by construction (a
                // `root_dir` shell-out), so blocking here is acceptable.
                match command
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        result.set("pid", child.id())?;
                        match child.wait_with_output() {
                            Ok(output) => {
                                result.set("code", output.status.code().unwrap_or(-1))?;
                                result.set("stdout", lua.create_string(&output.stdout)?)?;
                                result.set("stderr", lua.create_string(&output.stderr)?)?;
                            }
                            Err(e) => {
                                result.set("code", -1)?;
                                result.set("stdout", "")?;
                                result.set(
                                    "stderr",
                                    format!("vim.system: wait failed for {program}: {e}"),
                                )?;
                            }
                        }
                    }
                    Err(e) => {
                        result.set("code", -1)?;
                        result.set("stdout", "")?;
                        result.set("stderr", format!("vim.system: failed to spawn {program}: {e}"))?;
                    }
                }
                Ok(result)
            },
        )?,
    )?;

    // `vim._json_decode(str)`: parse a JSON document into the equivalent Lua value
    // (objects -> string-keyed tables, arrays -> sequences, `null` -> nil). Backs
    // `vim.json.decode`; raises on malformed input, matching neovim. The config
    // path that reaches for it is rust_analyzer's `root_dir`, decoding the
    // `cargo metadata` output to read `workspace_root`.
    vim.set(
        "_json_decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            json_to_lua(lua, &value)
        })?,
    )?;

    // `vim._json_encode(value)`: serialize a Lua value to a JSON string, using the
    // same array-vs-object rule as [`lua_to_rmpv`]. Backs `vim.json.encode`.
    vim.set(
        "_json_encode",
        lua.create_function(|_, value: mlua::Value| {
            serde_json::to_string(&lua_to_json(&value)?).map_err(mlua::Error::external)
        })?,
    )?;

    // ----- vim.uv: the libuv-style host primitives configs reach for ----------
    // The `lsp/<server>.lua` configs probe the filesystem/home/cwd through
    // `vim.uv` (and its legacy alias `vim.loop`) while building defaults and
    // resolving roots. Only the handful actually used are provided.
    let uv = lua.create_table()?;
    // `vim.uv.fs_stat(path)`: a stat table (at least `type`/`size`), or nil when
    // `path` can't be stat'd. Follows symlinks (the `fs_stat`, not `fs_lstat`,
    // semantics); configs read `.type == 'file'`/`'directory'`.
    uv.set(
        "fs_stat",
        lua.create_function(|lua, path: String| match std::fs::metadata(&path) {
            Ok(md) => {
                let t = lua.create_table()?;
                t.set("type", if md.is_dir() { "directory" } else { "file" })?;
                t.set("size", md.len())?;
                Ok(mlua::Value::Table(t))
            }
            Err(_) => Ok(mlua::Value::Nil),
        })?,
    )?;
    // `vim.uv.os_homedir()`: the user's home directory.
    uv.set(
        "os_homedir",
        lua.create_function(|_, ()| {
            Ok(std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok()))
        })?,
    )?;
    // `vim.uv.cwd()`: the process working directory.
    uv.set(
        "cwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()))
        })?,
    )?;
    // `vim.uv.fs_realpath(path)`: the canonical path (symlinks resolved), or nil.
    uv.set(
        "fs_realpath",
        lua.create_function(|_, path: String| {
            Ok(std::fs::canonicalize(&path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned()))
        })?,
    )?;
    // `vim.uv.os_uname()`: a uname table. `lspconfig.util` reads `.version` and
    // matches it against "Windows" to detect the platform, so `version` carries a
    // Windows marker only on Windows.
    uv.set(
        "os_uname",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            t.set("sysname", std::env::consts::OS)?;
            t.set("machine", std::env::consts::ARCH)?;
            // INCOMPLETE: `release` is hardcoded empty (no real kernel release).
            // Only `version`/`sysname` are consulted by lspconfig today, so the
            // gap is dormant; a config that reads os_uname().release for an OS
            // version check gets "". A real impl would call the libc `uname(2)` /
            // platform API and fill release (and sysname's true value — `sysname`
            // here is Rust's "macos"/"linux" const, not uname's "Darwin"/"Linux").
            t.set("release", "")?;
            t.set(
                "version",
                if cfg!(windows) {
                    "Windows"
                } else {
                    std::env::consts::OS
                },
            )?;
            Ok(t)
        })?,
    )?;
    vim.set("uv", uv.clone())?;
    vim.set("loop", uv)?; // `vim.loop` is the pre-0.10 name for `vim.uv`.

    // ----- additional vim.fn (filesystem / process / PATH) --------------------
    // `vim.fn.executable(name)`: 1 if `name` is an executable on $PATH (or an
    // executable file path), else 0. Configs use it to prefer a project-local
    // `node_modules/.bin/<server>` over the global one.
    func.set(
        "executable",
        lua.create_function(|_, name: String| Ok(i64::from(find_executable(&name).is_some())))?,
    )?;
    // `vim.fn.exepath(name)`: the resolved path to `name` on $PATH, or "".
    func.set(
        "exepath",
        lua.create_function(|_, name: String| Ok(find_executable(&name).unwrap_or_default()))?,
    )?;
    // `vim.fn.getpid()`: this (editor) process's id.
    func.set(
        "getpid",
        lua.create_function(|_, ()| Ok(std::process::id() as i64))?,
    )?;
    // `vim.fn.resolve(path)`: `path` with symlinks resolved, or unchanged if it
    // can't be canonicalized.
    func.set(
        "resolve",
        lua.create_function(|_, path: String| {
            Ok(std::fs::canonicalize(&path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(path))
        })?,
    )?;
    // `vim.fn.filereadable(path)`: 1 if `path` is a readable regular file, else 0.
    func.set(
        "filereadable",
        lua.create_function(|_, path: String| {
            Ok(i64::from(std::path::Path::new(&path).is_file()))
        })?,
    )?;
    // `vim.fn.readblob(path)`: the file's raw bytes as a string; errors if
    // unreadable (callers `pcall` it).
    func.set(
        "readblob",
        lua.create_function(|lua, path: String| {
            lua.create_string(std::fs::read(&path).map_err(mlua::Error::external)?)
        })?,
    )?;
    // `vim.fn.glob(pattern[, nosuf, list])`: existing paths matching a shell-style
    // glob (`*`/`?` wildcards, per path component). Returns a list when `list` is
    // truthy (the form `lspconfig.util.root_pattern` uses), else the default
    // newline-joined string. `nosuf` is accepted and ignored.
    func.set(
        "glob",
        lua.create_function(
            |lua, (pattern, _nosuf, list): (String, Option<bool>, Option<bool>)| {
                let paths = glob_paths(&pattern);
                if list.unwrap_or(false) {
                    Ok(mlua::Value::Table(lua.create_sequence_from(paths)?))
                } else {
                    Ok(mlua::Value::String(lua.create_string(paths.join("\n"))?))
                }
            },
        )?,
    )?;

    Ok(())
}

/// Store (or clear) the panel's `on_select` callback in the Lua registry. `None`
/// stores `nil`, so [`crate::LuaRuntime::run_panel_select`] reads it back as "no
/// handler" — keeping a closed/replaced panel from firing a stale callback.
fn store_panel_callback(lua: &Lua, cb: Option<mlua::Function>) -> mlua::Result<()> {
    match cb {
        Some(f) => lua.set_named_registry_value(PANEL_ON_SELECT, f),
        None => lua.set_named_registry_value(PANEL_ON_SELECT, mlua::Value::Nil),
    }
}
