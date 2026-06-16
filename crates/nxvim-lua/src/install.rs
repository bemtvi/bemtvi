//! Installing nxvim's `vim.*` bridge into a fresh Lua VM. Two passes, split only
//! by what they need to capture: [`install_vim`] wires the editor-touching
//! funnels that need just the [`Shared`] effect buffer (`vim.cmd`, `vim.api.*`,
//! `vim.panel.*`, the async-loop queue, the `vim.fn` basics, `print`), and
//! [`install_runtime_api`] adds the rest — the functions that also need the host
//! filesystem / environment / runtimepath (LSP queueing, the filesystem
//! `vim.fn.*`, the JSON / regex / process primitives). The pure-Lua
//! half of `vim.*` is layered on top from the `src/prelude/` modules by [`crate::LuaRuntime::new`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, Table, UserData, UserDataMethods, Variadic};

use crate::convert::{
    color_field, color_to_u32, env_pairs, flag_field, json_to_lua, lua_i64, lua_to_json,
    opt_table_to_json, stringify,
};
use crate::host::{create_dir_all_mode, get_runtime_file, glob_paths, parse_mode, stdpath};
use crate::ops::{
    BufOp, CompletePush, CompleteSetupReq, ConfirmReq, DecorMark, DecorPublish, DockOp, ExtmarkOp,
    FeedKeysOp, GlobalOptionOp, HlSet, LoopOp, LspOp, OptionValue, PanelOp, PickerOpenReq,
    PickerPush, PreviewPush, QfItem, QfSetOp, RegisterSetOp, SnippetAddReq, SnippetSetupReq,
    StatuslineKind, StatuslinePublishReq, StatuslineSetupReq, StatuslineTarget, TabOp,
    TerminalOpenReq, TsOp, UiFloatReq, UiInputReq, UiSelectReq, WindowOp,
};
use crate::runtime::{resolve_lua_fs, Shared};
use crate::vimregex;
use crate::BlockingSystem;

/// Lua registry key under which the panel's `on_select` callback is stored.
pub(crate) const PANEL_ON_SELECT: &str = "nxvim_panel_on_select";

/// `vim.regex(pat)` userdata: a vim pattern compiled by the real vim regexp engine
/// ([`nxvim_regex`]). Its `:match_str(text)` returns the match's `(start, end)`
/// byte offsets or `nil` — the shape neovim's regex object exposes, consumed by
/// `vim.treesitter.query`'s `#match?`. The reported span honours `\zs`/`\ze`.
struct LuaRegex {
    re: nxvim_regex::VimRegex,
}

impl UserData for LuaRegex {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("match_str", |_, this, text: mlua::String| {
            let text = text.to_str()?;
            // A compile error is already caught by `vim.regex`; a match-time error
            // (interrupt/timeout) is rare but raised rather than swallowed.
            let m = this
                .re
                .exec_line(&text, 0, false)
                .map_err(|e| mlua::Error::RuntimeError(format!("vim.regex match_str: {e}")))?;
            Ok(match m {
                Some(m) => (Some(m.start as i64), Some(m.end as i64)),
                None => (None, None),
            })
        });
    }
}

pub(crate) fn install_vim(lua: &Lua, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    let vim = lua.create_table()?;

    // `nx` is the canonical editor namespace (ADR 0002); the `vim.*` whitelist is
    // aliases onto it. The Lua prelude builds most of `nx.*`, but the surfaces
    // that need a Rust closure (here: `nx.cmd`) are seeded on the `nx` table now,
    // with `vim.*` forwarding to the same value.
    let nx = lua.create_table()?;

    // `nx.cmd` (alias: `vim.cmd` / `nvim_command`): the ex-command funnel. Only the
    // canonical `nx.*` natives are seeded from Rust; the muscle-memory `vim.api.nvim_*`
    // names are aliased onto them in the Lua prelude (ADR 0002), so there is no
    // second registration path here.
    let sh = shared.clone();
    let cmd = lua.create_function(move |_, cmd: String| {
        sh.borrow_mut().commands.push(cmd);
        Ok(())
    })?;
    nx.set("cmd", cmd.clone())?;
    vim.set("cmd", cmd)?;

    vim.set("version", "nxvim 0.1.0")?;

    // An empty `vim.api` namespace; the prelude fills it with `nvim_*` aliases onto
    // the `nx.*` natives (the canonical surface) seeded here and across the prelude.
    let api = lua.create_table()?;
    // `nx.echo` (alias: `nvim_echo`): push text onto the message line.
    let sh = shared.clone();
    let echo = lua.create_function(move |_, msg: String| {
        sh.borrow_mut().output.push(msg);
        Ok(())
    })?;
    nx.set("echo", echo)?;
    // `nvim_set_hl(ns, name, opts)`: capture the group definition for the server
    // to fold into the core registry, keyed by namespace. `ns == 0` is the global
    // table a colorscheme populates; a non-zero `ns` is kept in its own table so
    // it never clobbers the global definition. The full opts shape — colors, the
    // boolean attrs, and `link` — is read here so a colorscheme's hundreds of
    // calls all land.
    // INCOMPLETE: `nvim_win_set_hl_ns` (selecting a namespace per window at
    // render time) is not modelled yet, so a non-zero namespace is stored and
    // readable (`nvim_get_hl(ns, …)`) but the renderer always resolves against
    // the global table. Storing per-namespace is the prerequisite for that.
    // `nx.hl.define` (alias: `nvim_set_hl`): capture the group definition.
    let sh = shared.clone();
    let set_hl =
        lua.create_function(move |lua, (ns, name, opts): (i64, String, Option<Table>)| {
            let mut def = HlSet {
                ns: ns.max(0) as u32,
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
            // Write through to the `nx._hl_defs` mirror *now*, so a same-turn
            // `nx.hl.get` / `hlexists` sees this group. The core fold only
            // refreshes the mirror between turns (gated on the registry
            // generation), so without this an `init.lua` doing
            // `colorscheme(...)` then `require('lualine').setup{}` in one chunk
            // reads a stale, empty `Normal` and errors. Mirrors the write-through
            // `nx.o` / `setreg` already do for the same reason.
            write_hl_mirror_row(lua, &def)?;
            sh.borrow_mut().highlights.push(def);
            Ok(())
        })?;
    // `nx.hl` is the canonical highlight namespace (alias: `nvim_set_hl`); the Lua
    // prelude adds `nx.hl.get` to this same table later.
    let nx_hl = lua.create_table()?;
    nx_hl.set("define", set_hl)?;
    nx.set("hl", nx_hl)?;
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

    // `nx.dock`: nxvim's permanent edge panels (VSCode-style side/bottom docks).
    // Each call queues a [`DockOp`] the server drains into the core after the
    // chunk — same "Lua queues, core mutates" flow as `vim.panel.*`. A dock holds
    // an ordinary editable window; `<C-w><C-w>` crosses focus between the main area
    // and the docks. `side` is `"left"`/`"right"`/`"top"`/`"bottom"`.
    let dock = lua.create_table()?;
    // `open{ side, size?, buf? }` — open (or resize/refocus) and focus the dock.
    // `size` is columns (left/right) or rows (top/bottom); `buf` is an existing
    // buffer handle to show (default: a fresh scratch buffer).
    let sh = shared.clone();
    dock.set(
        "open",
        lua.create_function(move |_, opts: mlua::Table| {
            let side: String = opts.get("side")?;
            let size: Option<u64> = opts.get("size")?;
            let buf: Option<u64> = opts.get("buf")?;
            sh.borrow_mut()
                .dock_ops
                .push(DockOp::Open { side, size, buf });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    dock.set(
        "close",
        lua.create_function(move |_, side: String| {
            sh.borrow_mut().dock_ops.push(DockOp::Close { side });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    dock.set(
        "focus",
        lua.create_function(move |_, side: String| {
            sh.borrow_mut().dock_ops.push(DockOp::Focus { side });
            Ok(())
        })?,
    )?;
    // `toggle`/`hide`/`show` — collapse a dock from view while keeping its content
    // parked (VSCode-style), the counterpart of `close` (which drops the content).
    let sh = shared.clone();
    dock.set(
        "toggle",
        lua.create_function(move |_, side: String| {
            sh.borrow_mut().dock_ops.push(DockOp::Toggle { side });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    dock.set(
        "hide",
        lua.create_function(move |_, side: String| {
            sh.borrow_mut().dock_ops.push(DockOp::Hide { side });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    dock.set(
        "show",
        lua.create_function(move |_, side: String| {
            sh.borrow_mut().dock_ops.push(DockOp::Show { side });
            Ok(())
        })?,
    )?;
    // `nx._dock_set_opt(side, name, value)`: queue a [`DockOp::SetOption`] for the
    // dock scope. The prelude's `nx.dock.opt(side)` proxy (and the inline keys of
    // `nx.dock.open{...}`) call this after write-through to `nx._dock_opts`. A
    // number rides as `Number` (`showtabline`/`size`), a string as `String`
    // (`title`/`winhighlight`); the core validates the name. Other types are
    // ignored.
    let sh = shared.clone();
    dock.set(
        "_set_opt",
        lua.create_function(
            move |_, (side, name, value): (String, String, mlua::Value)| {
                let value = match value {
                    mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                    mlua::Value::Integer(n) => Some(OptionValue::Number(lua_i64(n))),
                    mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                    mlua::Value::String(s) => {
                        s.to_str().ok().map(|s| OptionValue::String(s.to_string()))
                    }
                    _ => None,
                };
                if let Some(value) = value {
                    sh.borrow_mut()
                        .dock_ops
                        .push(DockOp::SetOption { side, name, value });
                }
                Ok(())
            },
        )?,
    )?;
    nx.set("dock", dock)?;

    // `nx.terminal`: open a terminal job programmatically — the API twin of the
    // `:terminal` ex-command, same "Lua queues, core mutates" flow as `nx.dock`.
    // `open{ cmd?, cwd? }` queues a [`TerminalOpenReq`] the server drains into
    // `Editor::open_terminal` after the chunk. `cmd` is a string (whitespace-split
    // into argv, no shell — like `:terminal`) or a list (argv verbatim, so an
    // argument may contain spaces); omitted ⇒ the default shell. `cwd` defaults to
    // the editor's working directory.
    let terminal = lua.create_table()?;
    let sh = shared.clone();
    terminal.set(
        "open",
        lua.create_function(move |_, opts: Option<mlua::Table>| {
            let (argv, cwd) = match opts {
                None => (Vec::new(), None),
                Some(opts) => {
                    let cwd: Option<String> = opts.get("cwd")?;
                    let cmd: mlua::Value = opts.get("cmd")?;
                    let argv = match cmd {
                        mlua::Value::Nil => Vec::new(),
                        mlua::Value::String(s) => {
                            s.to_str()?.split_whitespace().map(str::to_string).collect()
                        }
                        mlua::Value::Table(t) => t
                            .sequence_values::<String>()
                            .collect::<mlua::Result<Vec<_>>>()?,
                        other => {
                            return Err(mlua::Error::runtime(format!(
                                "nx.terminal.open: `cmd` must be a string or a list, got {}",
                                other.type_name()
                            )))
                        }
                    };
                    (argv, cwd)
                }
            };
            sh.borrow_mut()
                .terminal_ops
                .push(TerminalOpenReq { argv, cwd });
            Ok(())
        })?,
    )?;
    nx.set("terminal", terminal)?;

    // ----- the async runtime bridge (the "event loop") -----------------------
    // Lua queues a [`LoopOp`] carrying a callback id; the server drains it in
    // `apply_lua_effects` and either services it directly (`Schedule`) or forwards
    // it to the background event-loop actor (timers, processes). Same "Lua queues,
    // the server drives" flow as `vim.cmd` / panel / lsp ops — the callback itself
    // stays in the Lua registry (`nx._cb_fns[id]`) and runs on the server thread.

    // `nx._schedule(id)`: defer callback `id` to the end of the current
    // convergence (the strict, non-nested `vim.schedule`).
    let sh = shared.clone();
    nx.set(
        "_schedule",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::Schedule { id });
            Ok(())
        })?,
    )?;
    // `nx._timer_start(id, delay_ms, repeat_ms)`: arm a timer firing callback
    // `id` after `delay_ms`, then every `repeat_ms` (`0` ⇒ one-shot).
    let sh = shared.clone();
    nx.set(
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
    // `nx._timer_stop(id)`: cancel the timer armed under `id`.
    let sh = shared.clone();
    nx.set(
        "_timer_stop",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::TimerStop { id });
            Ok(())
        })?,
    )?;
    // `nx._system_async(id, cmd, cwd, env)`: spawn `cmd` (an argv list) in the
    // event-loop actor and run callback `id` with `{ code, stdout, stderr }` when
    // it exits — the off-tick `vim.system`. Returns the child's OS pid immediately
    // (the actor sends it back over a oneshot the bridge blocks on *briefly* — only
    // until the spawn itself completes, not the run), so the `vim.system` handle
    // carries a real pid while the wait stays async. A spawn failure surfaces as a
    // `nil` pid (the `on_exit` still fires later with `code = -1`).
    let sh = shared.clone();
    nx.set(
        "_system_async",
        lua.create_function(
            move |_,
                  (id, cmd, cwd, env, stdin): (
                u64,
                Vec<String>,
                Option<String>,
                Option<Table>,
                Option<mlua::String>,
            )| {
                let env = env_pairs(env)?;
                let stdin = stdin.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                sh.borrow_mut().loop_ops.push(LoopOp::Spawn {
                    id,
                    cmd,
                    cwd,
                    env,
                    stdin,
                    stream: false,
                });
                Ok(())
            },
        )?,
    )?;
    // `nx._spawn_stream(id, cmd, cwd, env)`: spawn `cmd` (argv list) in the
    // event-loop actor and **stream** its stdout — each newline-delimited batch
    // fires the persistent stdout callback `nx._run_stdout(id, lines)`, and the
    // exit fires the one-shot `nx._run_cb(id, false, {code,stdout="",stderr})`.
    // Backs `nx.run_stream`'s streamed stdout/`on_exit` (the picker's streaming sources).
    // No stdin (a search/list job feeds none).
    let sh = shared.clone();
    nx.set(
        "_spawn_stream",
        lua.create_function(
            move |_, (id, cmd, cwd, env): (u64, Vec<String>, Option<String>, Option<Table>)| {
                let env = env_pairs(env)?;
                sh.borrow_mut().loop_ops.push(LoopOp::Spawn {
                    id,
                    cmd,
                    cwd,
                    env,
                    stdin: Vec::new(),
                    stream: true,
                });
                Ok(())
            },
        )?,
    )?;
    // `nx._system_kill(id, signal)`: terminate the async child running under
    // `id`. `signal` is accepted (neovim's `handle:kill(signal)`) but ignored —
    // the actor terminates the child unconditionally (see [`LoopOp::Kill`]).
    let sh = shared.clone();
    nx.set(
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
    // `vim.fn.getftime(path)`: the file's mtime in whole seconds, or -1. Routed
    // through the fs seam so it sees the *remote* project in a daemon session.
    let sh = shared.clone();
    func.set(
        "getftime",
        lua.create_function(move |_, path: String| {
            Ok(resolve_lua_fs(&sh)
                .stat(&path)
                .ok()
                .and_then(|st| st.mtime)
                .map(|(sec, _)| sec)
                .unwrap_or(-1))
        })?,
    )?;
    let sh = shared.clone();
    func.set(
        "isdirectory",
        lua.create_function(move |_, path: String| {
            let is_dir = matches!(
                resolve_lua_fs(&sh).stat(&path),
                Ok(st) if st.kind == crate::FileKind::Dir
            );
            Ok(i64::from(is_dir))
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

    lua.globals().set("nx", nx)?;
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

/// The argument tuple of `nx._lsp_start`: the original five
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
/// (runtimepath `lsp/` discovery), `vim.fn.getcwd`, the `nx._read_file` /
/// `nx._readdir` filesystem primitives the pure-Lua `vim.fs` builds on, and
/// `nx._lsp_start` (the queue `vim.lsp.start` pushes onto). Separated from
/// [`install_vim`] because these capture the runtimepath, known only here.
pub(crate) fn install_runtime_api(
    lua: &Lua,
    shared: &Rc<RefCell<Shared>>,
    runtimepath: &[PathBuf],
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let func: Table = vim.get("fn")?;
    let nx: Table = lua.globals().get("nx")?;

    // `nx.runtime_file(name, all)` (alias: `nvim_get_runtime_file`, set in the Lua
    // prelude): full paths of files matching `name` (a runtimepath-relative path,
    // the final component optionally globbed with `*`) across the runtimepath.
    // `all=false` returns the first match only. The `lsp/<server>.lua`
    // config-discovery primitive.
    let rtp = runtimepath.to_vec();
    let runtime_file = lua.create_function(move |lua, (name, all): (String, Option<bool>)| {
        let hits = get_runtime_file(&rtp, &name, all.unwrap_or(false));
        lua.create_sequence_from(hits)
    })?;
    nx.set("runtime_file", runtime_file)?;

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

    // `nx._read_file(path)`: the file's contents, or nil if unreadable. Backs the
    // pure-Lua loader that sources an `lsp/<name>.lua` config (via `loadstring`),
    // sidestepping any `loadfile` sandbox question.
    nx.set(
        "_read_file",
        lua.create_function(|_, path: String| Ok(std::fs::read_to_string(&path).ok()))?,
    )?;

    // `nx._readdir(path)`: the entry names directly under `path` (no `.`/`..`),
    // or an empty list if it can't be read. Backs `vim.fs.find`/predicate markers,
    // which walk the *project* tree, so it routes through the fs seam (remote in a
    // daemon session).
    let sh = shared.clone();
    nx.set(
        "_readdir",
        lua.create_function(move |lua, path: String| {
            let names: Vec<String> = resolve_lua_fs(&sh)
                .scandir(&path)
                .map(|entries| entries.into_iter().map(|e| e.name).collect())
                .unwrap_or_default();
            lua.create_sequence_from(names)
        })?,
    )?;

    // `nx._lsp_start(name, cmd, root, filetype, bufnr, init_options, settings,
    // capabilities)`: queue an [`LspOp::Start`] for the server to drain. The
    // Lua-facing `vim.lsp.start` wrapper (prelude) resolves the config and root,
    // then calls this. The trailing three are the config's `init_options` /
    // `settings` / `capabilities` tables (each `nil` when unset); they convert
    // through the same `lua_to_json` bridge `vim.json.encode` uses, so the server
    // forwards them at `initialize` exactly as the config wrote them (Phase 2).
    let sh = shared.clone();
    nx.set(
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

    // `nx._feedkeys(keys, remap, insert)`: queue a [`FeedKeysOp`] for the server
    // to drain into its typeahead after the chunk. The Lua-facing
    // `nvim_feedkeys` (prelude) parses the mode flags into `remap`/`insert`.
    let sh = shared.clone();
    nx.set(
        "_feedkeys",
        lua.create_function(move |_, (keys, remap, insert): (String, bool, bool)| {
            sh.borrow_mut().feedkeys.push(FeedKeysOp {
                keys,
                remap,
                insert,
            });
            Ok(())
        })?,
    )?;

    // The extmark funnels (`nx._extmark_set` / `_extmark_del` / `_extmark_clear`):
    // queue an [`ExtmarkOp`] for the server to apply to the target buffer's
    // `ExtmarkStore` after the chunk. The Lua-facing `nvim_buf_set_extmark` family
    // (prelude) has resolved `bufnr`, allocated the id, and updated its
    // `nx._extmarks` mirror (write-through); the server converts the 0-based
    // `(row, col)` positions to byte offsets against the live rope.
    // `(bufnr, ns, id, row, col, end_row, end_col, hl_group, priority)` — the
    // positional payload the prelude's `nvim_buf_set_extmark` forwards.
    type ExtmarkSetArgs = (
        u64,
        u32,
        u64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
        u32,
    );
    let sh = shared.clone();
    nx.set(
        "_extmark_set",
        lua.create_function(
            move |_, (bufnr, ns, id, row, col, end_row, end_col, hl_group, priority): ExtmarkSetArgs| {
                sh.borrow_mut().extmark_ops.push(ExtmarkOp::Set {
                    bufnr,
                    ns,
                    id,
                    row,
                    col,
                    end_row,
                    end_col,
                    hl_group,
                    priority,
                });
                Ok(())
            },
        )?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_extmark_del",
        lua.create_function(move |_, (bufnr, ns, id): (u64, u32, u64)| {
            sh.borrow_mut()
                .extmark_ops
                .push(ExtmarkOp::Del { bufnr, ns, id });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_extmark_clear",
        lua.create_function(
            move |_, (bufnr, ns, line_start, line_end): (u64, u32, i64, i64)| {
                sh.borrow_mut().extmark_ops.push(ExtmarkOp::Clear {
                    bufnr,
                    ns,
                    line_start,
                    line_end,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._buf_set_option(bufnr, name, value)`: queue a [`BufOp::SetOption`] for
    // the server to apply to the live editor's buffer (Phase 6). The prelude
    // (`vim.bo` / `nvim_set_option_value`) has canonicalized `name` and updated
    // its option mirror (write-through); a number value rides as `Number`, a
    // boolean as `Bool`, and the one string buffer option (`regexsyntax`) as
    // `String`. Other Lua types are ignored (the option set is typed:
    // tabstop/shiftwidth are numbers, expandtab a boolean).
    let sh = shared.clone();
    nx.set(
        "_buf_set_option",
        lua.create_function(move |_, (bufnr, name, value): (u64, String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(lua_i64(n))),
                mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                mlua::Value::String(s) => {
                    s.to_str().ok().map(|s| OptionValue::String(s.to_string()))
                }
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

    // `nx._set_global_option(name, value)`: queue a [`GlobalOptionOp`] for the
    // server to apply to the editor's global options. The prelude (`vim.o`) has
    // canonicalized `name` and written through its `nx._go_mirror`; the wired
    // global options are all boolean, but a number rides as `Number` for symmetry
    // with the buffer/window bridges. Other Lua types are ignored.
    let sh = shared.clone();
    nx.set(
        "_set_global_option",
        lua.create_function(move |_, (name, value): (String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(lua_i64(n))),
                mlua::Value::Number(n) => Some(OptionValue::Number(n as i64)),
                // `statusline` and other string globals (the prelude forwards
                // only the canonical wired set).
                mlua::Value::String(s) => {
                    s.to_str().ok().map(|s| OptionValue::String(s.to_string()))
                }
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

    // `nx._win_op(...)`: the window-mutation bridges (Phase 5). Each queues a
    // [`WindowOp`] the server drains into the live editor after the chunk; the
    // Lua-facing `vim.api.nvim_win_*` wrappers (prelude) have already updated the
    // `nx._wins` mirror (write-through) where a read-after-write needs it.
    let sh = shared.clone();
    nx.set(
        "_set_current_win",
        lua.create_function(move |_, win: u64| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetCurrent { win });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_buf",
        lua.create_function(move |_, (win, buf): (u64, u64)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetBuf { win, buf });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_cursor",
        lua.create_function(move |_, (win, line, col): (u64, usize, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetCursor { win, line, col });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_topline",
        lua.create_function(move |_, (win, top): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetTopline { win, top });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_width",
        lua.create_function(move |_, (win, width): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetWidth { win, width });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_height",
        lua.create_function(move |_, (win, height): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetHeight { win, height });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_win_set_option",
        lua.create_function(move |_, (win, name, value): (u64, String, mlua::Value)| {
            let value = match value {
                mlua::Value::Boolean(b) => Some(OptionValue::Bool(b)),
                mlua::Value::Integer(n) => Some(OptionValue::Number(lua_i64(n))),
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
    nx.set(
        "_win_close",
        lua.create_function(move |_, (win, force): (u64, bool)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::Close { win, force });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
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
    // `nx._open_float(cfg)`: queue the float form of `nvim_open_win`. The prelude
    // builds `cfg` (a validated table of primitive fields) and calls this; the
    // server drains the op into `Editor::open_float_window`. The split form keeps
    // its own `_open_win` bridge above.
    let sh = shared.clone();
    nx.set(
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
    // `nx._win_set_config(win, cfg)`: queue the partial reconfigure of
    // `nvim_win_set_config`. `cfg` carries only the keys the caller passed (the
    // prelude already validated the enumerated strings); a missing key stays
    // `None` so the core leaves it unchanged. `relative = ""` rides through as the
    // re-tile form.
    // `nx._set_current_tab(tab)`: queue the tab switch of
    // `nvim_set_current_tabpage` (Phase 3). The only tab mutation in the API —
    // the `nvim_tabpage_*` reads resolve from the `nx._tabs` mirror. The prelude
    // has already resolved `0` to the current tab and updated the mirror
    // (write-through) so a read-after-set in the same chunk agrees.
    let sh = shared.clone();
    nx.set(
        "_set_current_tab",
        lua.create_function(move |_, tab: u64| {
            sh.borrow_mut().tab_ops.push(TabOp::SetCurrent { tab });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
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

    // `nx._lsp_buf(kind)`: queue a position-family `vim.lsp.buf.*` request
    // ([`LspOp::BufRequest`]) or one of the edit ops (`Format`/`CodeAction`),
    // selected by the `LspReqKind::as_u16` the prelude passes. The single Rust
    // entry the bare `vim.lsp.buf` functions route through (rename has its own,
    // below, since it carries an argument).
    let sh = shared.clone();
    nx.set(
        "_lsp_buf",
        lua.create_function(move |_, kind: u16| {
            sh.borrow_mut().lsp_ops.push(LspOp::BufRequest { kind });
            Ok(())
        })?,
    )?;

    // `nx._lsp_buf_format()`: queue [`LspOp::Format`]. Kept distinct from
    // `_lsp_buf` because formatting has no `{uri, position}` shape (it routes to
    // `request_lsp_format`, not `request_lsp`).
    let sh = shared.clone();
    nx.set(
        "_lsp_buf_format",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::Format);
            Ok(())
        })?,
    )?;

    // `nx._lsp_buf_code_action()`: queue [`LspOp::CodeAction`].
    let sh = shared.clone();
    nx.set(
        "_lsp_buf_code_action",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::CodeAction);
            Ok(())
        })?,
    )?;

    // `nx._lsp_buf_rename(name)`: queue [`LspOp::Rename`]. The prelude requires
    // the argument (echoing `E471` on nil), so a name always arrives here.
    let sh = shared.clone();
    nx.set(
        "_lsp_buf_rename",
        lua.create_function(move |_, new_name: String| {
            sh.borrow_mut().lsp_ops.push(LspOp::Rename { new_name });
            Ok(())
        })?,
    )?;

    // `nx._diagnostic_goto(forward, severity)`: queue [`LspOp::DiagnosticGoto`]
    // — the cursor move `vim.diagnostic.goto_next`/`goto_prev` drive.
    let sh = shared.clone();
    nx.set(
        "_diagnostic_goto",
        lua.create_function(move |_, (forward, severity): (bool, Option<u16>)| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticGoto {
                forward,
                severity: severity.map(|s| s as u8),
            });
            Ok(())
        })?,
    )?;

    // `nx._diagnostic_setloclist()`: queue [`LspOp::DiagnosticSetloclist`] — open
    // the diagnostics location-list panel.
    let sh = shared.clone();
    nx.set(
        "_diagnostic_setloclist",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticSetloclist);
            Ok(())
        })?,
    )?;

    // `nx._diagnostic_open_float()`: queue [`LspOp::DiagnosticOpenFloat`] — open
    // the float listing the cursor line's diagnostics in full.
    let sh = shared.clone();
    nx.set(
        "_diagnostic_open_float",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticOpenFloat);
            Ok(())
        })?,
    )?;

    // `nx._diagnostic_config(underline, virtual_text, virt_prefix, signs, sign_text)`:
    // queue [`LspOp::DiagnosticConfig`] — the prelude resolves the merged
    // `underline` / `virtual_text` / `signs` to bools (and the virt-text `prefix` /
    // the per-severity sign glyphs to strings) and pushes them so the server gates
    // the squiggle, inline-message, and gutter-sign rendering. `sign_text` is the
    // four `[error, warn, info, hint]` glyphs in order; anything else is a prelude
    // bug, so reject it loudly rather than silently rendering the wrong column.
    let sh = shared.clone();
    nx.set(
        "_diagnostic_config",
        lua.create_function(
            move |_,
                  (underline, virtual_text, virt_prefix, signs, sign_text): (
                bool,
                bool,
                String,
                bool,
                Vec<String>,
            )| {
                let sign_text: [String; 4] = sign_text.try_into().map_err(|v: Vec<String>| {
                    mlua::Error::RuntimeError(format!(
                        "nx._diagnostic_config: sign_text must be 4 glyphs, got {}",
                        v.len()
                    ))
                })?;
                sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticConfig {
                    underline,
                    virtual_text,
                    virt_prefix,
                    signs,
                    sign_text,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._lsp_client_request(client_id, method, params, cb_id)`: queue a generic
    // `client:request` ([`LspOp::ClientRequest`]). The handler is already stored in
    // `nx._cb_fns[cb_id]` by the Lua wrapper; the server forwards the request and
    // runs the callback with `(err, result)` when the reply lands (Phase 5).
    // `params` is any Lua value (a table / nil), converted through the same
    // `lua_to_json` bridge `vim.json.encode` uses.
    let sh = shared.clone();
    nx.set(
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

    // `nx._lsp_client_notify(client_id, method, params)`: queue a generic
    // fire-and-forget `client:notify` ([`LspOp::ClientNotify`]).
    let sh = shared.clone();
    nx.set(
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

    // `nx._lsp_apply_workspace_edit(edit)`: queue [`LspOp::ApplyWorkspaceEdit`]
    // (Phase 7). `edit` is the LSP-shape WorkspaceEdit table, converted to JSON
    // through the same `lua_to_json` bridge `client:request` params use; the server
    // deserializes, normalizes, and applies it across the open buffers it names.
    let sh = shared.clone();
    nx.set(
        "_lsp_apply_workspace_edit",
        lua.create_function(move |_, edit: mlua::Value| {
            sh.borrow_mut().lsp_ops.push(LspOp::ApplyWorkspaceEdit {
                edit: lua_to_json(&edit)?,
            });
            Ok(())
        })?,
    )?;

    // `nx._lsp_show_document(uri, line, character, encoding)`: queue
    // [`LspOp::ShowDocument`] (Phase 7) — the server builds an LSP location and
    // reuses the native single-location goto (open + cursor jump).
    let sh = shared.clone();
    nx.set(
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

    // `nx._lsp_semantic_enable(bufnr, enabled)`: queue [`LspOp::SemanticTokensEnable`]
    // (Phase 3) — `vim.lsp.semantic_tokens.start`/`stop` flip the per-buffer
    // projection (`bufnr` already resolved from `0`/`nil` → current in Lua).
    let sh = shared.clone();
    nx.set(
        "_lsp_semantic_enable",
        lua.create_function(move |_, (bufnr, enabled): (u64, bool)| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::SemanticTokensEnable { bufnr, enabled });
            Ok(())
        })?,
    )?;

    // `nx._lsp_semantic_refresh(bufnr)`: queue [`LspOp::SemanticTokensRefresh`]
    // (Phase 3) — `vim.lsp.semantic_tokens.force_refresh` drops the delta cursor and
    // re-requests the whole token set.
    let sh = shared.clone();
    nx.set(
        "_lsp_semantic_refresh",
        lua.create_function(move |_, bufnr: u64| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::SemanticTokensRefresh { bufnr });
            Ok(())
        })?,
    )?;

    // `nx._lsp_semantic_config(enabled)`: queue [`LspOp::SemanticTokensConfig`]
    // (Phase 3) — `vim.lsp.semantic_tokens.enable` is nxvim's editor-wide gate.
    let sh = shared.clone();
    nx.set(
        "_lsp_semantic_config",
        lua.create_function(move |_, enabled: bool| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::SemanticTokensConfig { enabled });
            Ok(())
        })?,
    )?;

    // `nx._lsp_inlay_hint_enable(bufnr, enabled)`: queue [`LspOp::InlayHintEnable`]
    // — `vim.lsp.inlay_hint.enable(enable, { bufnr })` flips the per-buffer inlay-
    // hint projection (off by default; `bufnr` already resolved from `0`/`nil` →
    // current in Lua).
    let sh = shared.clone();
    nx.set(
        "_lsp_inlay_hint_enable",
        lua.create_function(move |_, (bufnr, enabled): (u64, bool)| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::InlayHintEnable { bufnr, enabled });
            Ok(())
        })?,
    )?;

    // `nx._ui_input(prompt, default, cb_id)`: queue a `vim.ui.input` prompt
    // ([`UiInputReq`]). The server opens the editor's command line labelled
    // `prompt` (prefilled with `default`) and fires `nx._cb_fns[cb_id]` with the
    // typed text — or `nil` on cancel — when the user submits (Phase 8).
    let sh = shared.clone();
    nx.set(
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

    // `nx._ui_select(items, prompt, cb_id)`: queue a `nx.ui.select` request
    // ([`UiSelectReq`]). The server opens the floating selectable-list widget
    // showing `items` (the already-rendered display labels) titled `prompt`, and
    // fires `nx._cb_fns[cb_id]` with the chosen 1-based index — or `nil` on
    // cancel — when the user confirms. The Lua wrapper maps the index back to the
    // original item before calling the user's `on_choice`.
    let sh = shared.clone();
    nx.set(
        "_ui_select",
        lua.create_function(
            move |_, (items, prompt, cb_id): (Vec<String>, String, u64)| {
                sh.borrow_mut().ui_selects.push(UiSelectReq {
                    items,
                    prompt,
                    cb_id,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._ui_float(id, lines, title, border, editor)`: queue a `nx.ui.float`
    // open/update request ([`UiFloatReq`]). The server opens the list-less content
    // float rendering `lines` with the given border / placement. `id == 0` is the
    // transient default (no callback, dismissed by the next key); a non-zero `id`
    // is a persistent handle's float (survives keystrokes; an `:update` re-queues
    // with the same id), closed via `_ui_float_close`.
    let sh = shared.clone();
    nx.set(
        "_ui_float",
        lua.create_function(
            move |_,
                  (id, lines, title, border, editor): (
                u64,
                Vec<String>,
                Option<String>,
                String,
                bool,
            )| {
                sh.borrow_mut().ui_floats.push(UiFloatReq {
                    id,
                    close: false,
                    lines,
                    title,
                    border,
                    editor,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._ui_float_close(id)`: queue a close for the persistent content float
    // keyed by handle `id`. Ordered in the same `ui_floats` queue as opens, so an
    // open-then-close within one chunk is honoured. A close for a float already
    // replaced no-ops server-side (`close_content_float_id`).
    let sh = shared.clone();
    nx.set(
        "_ui_float_close",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().ui_floats.push(UiFloatReq {
                id,
                close: true,
                lines: Vec::new(),
                title: None,
                border: String::new(),
                editor: false,
            });
            Ok(())
        })?,
    )?;

    // `nx._picker_open(dynamic)`: queue a `nx.picker.open` request
    // ([`PickerOpenReq`]). The server opens the centered fuzzy-finder widget and
    // kicks the active source's initial run; the source's candidates / `confirm` /
    // `on_cancel` stay Lua-side (`nx._picker`).
    let sh = shared.clone();
    nx.set(
        "_picker_open",
        lua.create_function(
            move |_,
                  (dynamic, width, height, prompt_bottom, preview): (
                bool,
                Option<String>,
                Option<String>,
                Option<bool>,
                Option<bool>,
            )| {
                sh.borrow_mut().picker_opens.push(PickerOpenReq {
                    dynamic,
                    width: width.unwrap_or_default(),
                    height: height.unwrap_or_default(),
                    prompt_bottom: prompt_bottom.unwrap_or(false),
                    preview: preview.unwrap_or(false),
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._complete_setup(auto, min_chars, next, prev, confirm, abort, has_async)`:
    // queue a native completion-engine configuration ([`CompleteSetupReq`]). Each key
    // argument is a list of vim notation strings (`{ "<C-n>", "<Tab>" }`); an empty
    // list keeps that action's built-in default. `has_async` is true when at least one
    // configured source is a Lua `complete` function (`nx.complete.source{}`), so the
    // engine dispatches it off the input path. The Lua wrapper (`prelude/complete.lua`)
    // validates the source list before calling this.
    let sh = shared.clone();
    type CompleteSetupArgs = (
        bool,
        usize,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        Vec<String>,
        bool,
        bool,
        i32,
        i32,
        bool,
        String,
        bool,
        i32,
    );
    nx.set(
        "_complete_setup",
        lua.create_function(
            move |_,
                  (
                auto,
                min_chars,
                next,
                prev,
                confirm,
                abort,
                has_async,
                lsp,
                buffer_priority,
                lsp_priority,
                docs,
                trigger_chars,
                snippets,
                snippets_priority,
            ): CompleteSetupArgs| {
                sh.borrow_mut().complete_setups.push(CompleteSetupReq {
                    auto,
                    min_chars,
                    next,
                    prev,
                    confirm,
                    abort,
                    has_async,
                    lsp,
                    buffer_priority,
                    lsp_priority,
                    docs,
                    trigger_chars,
                    snippets,
                    snippets_priority,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._complete_trigger()`: queue a manual completion open
    // (`nx.complete.trigger()` / a mapped key). The server runs
    // `Editor::complete_manual_trigger` after the chunk — it ignores `auto` /
    // `min_chars`, so an explicit request always offers what's there.
    let sh = shared.clone();
    nx.set(
        "_complete_trigger",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().complete_triggers.push(());
            Ok(())
        })?,
    )?;
    // `nx._complete_push(gen, labels, inserts, docs)`: queue a BATCH of streamed async
    // completion candidates ([`CompletePush`]) — parallel `labels` (display) /
    // `inserts` (applied on accept) / `docs` (inline docs-sidebar text, `""` ⇒ none)
    // arrays, stamped with the trigger `gen`eration. The server drops a batch whose
    // `gen` is behind the live prefix and appends the rest to the open completion
    // popup. Phase 4-B; `docs` added 4-E.
    let sh = shared.clone();
    nx.set(
        "_complete_push",
        lua.create_function(
            move |_,
                  (gen, labels, inserts, docs, resolves): (
                u64,
                Vec<String>,
                Vec<String>,
                Vec<String>,
                Vec<u64>,
            )| {
                let mut sh = sh.borrow_mut();
                sh.complete_pushes.reserve(labels.len());
                for (i, (label, insert)) in labels.into_iter().zip(inserts).enumerate() {
                    // `docs` / `resolves` are parallel to `labels`: an empty doc string
                    // ⇒ no inline docs; a `0` resolve id ⇒ no lazy-docs handle.
                    let doc = docs.get(i).filter(|d| !d.is_empty()).cloned();
                    let resolve = resolves.get(i).copied().filter(|&r| r != 0);
                    sh.complete_pushes.push(CompletePush {
                        gen,
                        label,
                        insert,
                        doc,
                        resolve,
                    });
                }
                Ok(())
            },
        )?,
    )?;
    // `nx._complete_finish(gen)`: every async source for generation `gen` has
    // finished (the Lua wrapper reduces their `done()`s to one call) — the server
    // closes the popup if the prefix matched nothing across all sources. Phase 4-B.
    let sh = shared.clone();
    nx.set(
        "_complete_finish",
        lua.create_function(move |_, gen: u64| {
            sh.borrow_mut().complete_finishes.push(gen);
            Ok(())
        })?,
    )?;
    // `nx._complete_resolve_done(id, doc)`: a plugin source's `resolve` callback
    // responded with the lazy docs for resolve-handle `id` — queue them for the
    // server to fold into its docs-sidebar cache and repaint. `""` ⇒ resolved but
    // docless (the server stamps it so the row is never re-resolved). Phase 4-E.
    let sh = shared.clone();
    nx.set(
        "_complete_resolve_done",
        lua.create_function(move |_, (id, doc): (u64, String)| {
            sh.borrow_mut().complete_resolve_dones.push((id, doc));
            Ok(())
        })?,
    )?;

    // `nx._statusline_setup(win, kind, left, right)`: queue a `nx.statusline.setup{}`
    // / `reset()` request ([`StatuslineSetupReq`]). `win` is `nil` for the global
    // layout or a window id for a window-local override; `kind` is `"segments"`
    // (with the validated `left` / `right` name lists), `"format"` (use the
    // `%`-format), or `"inherit"` (drop the override). The Lua wrapper validated
    // every segment name against the built-ins / registered segments.
    let sh = shared.clone();
    nx.set(
        "_statusline_setup",
        lua.create_function(
            move |_, (win, kind, left, right): (Option<u64>, String, Vec<String>, Vec<String>)| {
                let target = match win {
                    Some(w) => StatuslineTarget::Window(w),
                    None => StatuslineTarget::Global,
                };
                let kind = match kind.as_str() {
                    "format" => StatuslineKind::Format,
                    "inherit" => StatuslineKind::Inherit,
                    _ => StatuslineKind::Segments { left, right },
                };
                sh.borrow_mut()
                    .statusline_setups
                    .push(StatuslineSetupReq { target, kind });
                Ok(())
            },
        )?,
    )?;
    // `nx._statusline_publish(win, name, texts, groups)`: queue a custom segment's
    // resolved cells for one window ([`StatuslinePublishReq`]) — parallel `texts` /
    // `groups` arrays (an empty group ⇒ the base `StatusLine` highlight). The server
    // caches them by `(win, name)` and paints them until the next re-render.
    let sh = shared.clone();
    nx.set(
        "_statusline_publish",
        lua.create_function(
            move |_, (win, name, texts, groups): (u64, String, Vec<String>, Vec<String>)| {
                let cells = texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let group = groups.get(i).filter(|g| !g.is_empty()).cloned();
                        (text, group)
                    })
                    .collect();
                sh.borrow_mut()
                    .statusline_publishes
                    .push(StatuslinePublishReq { win, name, cells });
                Ok(())
            },
        )?,
    )?;
    // `nx._statusline_invalidate(name)`: mark a custom segment dirty. The server
    // re-renders it (per window) after the current input settles, with a fresh
    // window mirror — so an invalidate fired from an autocmd that ran before the
    // window/focus transition still renders against the post-transition state.
    let sh = shared.clone();
    nx.set(
        "_statusline_invalidate",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().statusline_invalidates.push(name);
            Ok(())
        })?,
    )?;

    // `nx._snippet_setup(next, prev)`: queue the tabstop-jump key configuration
    // ([`SnippetSetupReq`]) for the native snippet engine. Each is a list of vim
    // notation strings; empty keeps the built-in default (`<Tab>` / `<S-Tab>`).
    let sh = shared.clone();
    nx.set(
        "_snippet_setup",
        lua.create_function(move |_, (next, prev): (Vec<String>, Vec<String>)| {
            sh.borrow_mut()
                .snippet_setups
                .push(SnippetSetupReq { next, prev });
            Ok(())
        })?,
    )?;
    // `nx._snippet_add(ft, triggers, bodies)`: register string-body snippets for a
    // filetype ([`SnippetAddReq`]); the server stores them for the `snippets`
    // completion source. The Lua wrapper validates shapes and rejects function bodies.
    let sh = shared.clone();
    nx.set(
        "_snippet_add",
        lua.create_function(
            move |_, (filetype, triggers, bodies): (String, Vec<String>, Vec<String>)| {
                sh.borrow_mut().snippet_adds.push(SnippetAddReq {
                    filetype,
                    triggers,
                    bodies,
                });
                Ok(())
            },
        )?,
    )?;
    // `nx._snippet_expand(body)`: queue a snippet body to expand at the cursor; the
    // server parses and expands it via `Editor::expand_snippet` after the chunk.
    let sh = shared.clone();
    nx.set(
        "_snippet_expand",
        lua.create_function(move |_, body: String| {
            sh.borrow_mut().snippet_expands.push(body);
            Ok(())
        })?,
    )?;
    // `nx._picker_push(gen, labels, keys, paths, rows, cols)`: queue a BATCH of
    // streamed picker candidates ([`PickerPush`]) — parallel arrays, stamped with
    // the run `gen`eration. Batching keeps a 100k-result stream to ~one bridge
    // crossing per source chunk instead of one per item. `paths` (and, for the
    // `"location"` kind, `rows` / `cols` — all 0-based) are present only when the
    // picker carries a preview pane; an empty `paths[i]` means that row has no
    // target. The server drops a batch whose `gen` is behind the live query and
    // feeds the rest into the open picker.
    let sh = shared.clone();
    // `(gen, labels, keys, paths, rows, cols)` — the parallel-array push batch; the
    // last three are `Some` only for a preview-carrying picker.
    type PushArgs = (
        u64,
        Vec<String>,
        Vec<usize>,
        Option<Vec<String>>,
        Option<Vec<usize>>,
        Option<Vec<usize>>,
    );
    nx.set(
        "_picker_push",
        lua.create_function(move |_, (gen, labels, keys, paths, rows, cols): PushArgs| {
            let mut sh = sh.borrow_mut();
            sh.picker_pushes.reserve(labels.len());
            for (i, (label, key)) in labels.into_iter().zip(keys).enumerate() {
                let preview = paths.as_ref().and_then(|ps| {
                    let path = ps.get(i)?;
                    if path.is_empty() {
                        return None;
                    }
                    let loc = match (rows.as_ref(), cols.as_ref()) {
                        (Some(rs), Some(cs)) => Some((*rs.get(i)?, *cs.get(i)?)),
                        _ => None,
                    };
                    Some(PreviewPush {
                        path: path.clone(),
                        loc,
                    })
                });
                sh.picker_pushes.push(PickerPush {
                    gen,
                    label,
                    key,
                    preview,
                });
            }
            Ok(())
        })?,
    )?;
    // `nx._picker_finish(gen)`: the source's `done()` for generation `gen` — the
    // server settles the picker (a query that matched nothing clears its stale
    // rows; one that matched already swapped them in via `_picker_push`).
    let sh = shared.clone();
    nx.set(
        "_picker_finish",
        lua.create_function(move |_, gen: u64| {
            sh.borrow_mut().picker_finishes.push(gen);
            Ok(())
        })?,
    )?;
    // `nx._picker_action(name)`: a `picker`-bucket keymap fired the named picker
    // action (next / prev / confirm / cancel / preview scroll / query edit); the
    // server applies it to the open picker via `Editor::apply_picker_action`. The
    // default `picker` maps in `prelude/picker.lua` call this; a user override does
    // too (or runs anything else). Queued so it drains in the same convergence as the
    // keystroke that fired it.
    let sh = shared.clone();
    nx.set(
        "_picker_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().picker_actions.push(name);
            Ok(())
        })?,
    )?;
    // `nx._select_action(name)`: a `select`-bucket keymap fired the named action
    // (next / prev / first / last / confirm / cancel) on the open `nx.ui.select`
    // list; the server applies it via `Editor::apply_select_action`. The select-mode
    // sibling of `nx._picker_action`.
    let sh = shared.clone();
    nx.set(
        "_select_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().select_actions.push(name);
            Ok(())
        })?,
    )?;
    // `nx._panel_action(name)`: a `panel`-bucket keymap fired the named action (next /
    // prev / first / last / half scroll / confirm / close) on the open message /
    // quickfix panel; the server applies it via `Editor::apply_panel_action`. A
    // panel sibling of `nx._picker_action`.
    let sh = shared.clone();
    nx.set(
        "_panel_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().panel_actions.push(name);
            Ok(())
        })?,
    )?;
    // `nx._explorer_action(name)`: an `explorer`-bucket keymap fired the named action
    // (open / up / next / prev / first / last / half + page scroll) on the file
    // explorer listing; the server applies it via `Editor::apply_explorer_action`.
    let sh = shared.clone();
    nx.set(
        "_explorer_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().explorer_actions.push(name);
            Ok(())
        })?,
    )?;
    // `nx._cmdline_action(name)`: a `cmdline`-bucket (`'c'`) keymap fired the named
    // action (cancel / submit / backspace / delete / cursor motion / history /
    // insert_register) on the open command line; the server applies it via
    // `Editor::apply_cmdline_action`. The command-line sibling of `nx._picker_action`.
    let sh = shared.clone();
    nx.set(
        "_cmdline_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().cmdline_actions.push(name);
            Ok(())
        })?,
    )?;

    // `nx._decor_register()`: a `nx.decor.provider` was registered — flip the live
    // gate so the server starts dispatching viewport-change signals to providers.
    // While it stays unset the server skips the whole off-tick decor path (never even
    // slices the visible lines), so the common no-provider config pays nothing on
    // scroll. Phase 2 of `nx.decor`.
    let sh = shared.clone();
    nx.set(
        "_decor_register",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().decor_active = true;
            Ok(())
        })?,
    )?;

    // `nx._key_pending_register()`: an `nx.on_key_pending` listener was registered —
    // flip the live gate so the server starts computing + pushing the pending-key
    // signal on each input batch. While it stays unset the server never walks the
    // trie for continuations or re-enters Lua, so the common no-which-key config pays
    // nothing per keystroke. The pending-key (which-key / showcmd) oracle.
    let sh = shared.clone();
    nx.set(
        "_key_pending_register",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().key_pending_active = true;
            Ok(())
        })?,
    )?;

    // `nx._decor_publish(ns, gen, win, buf, rows, cols, end_rows, end_cols, hls,
    // priorities)`: queue the marks a provider published for one window's viewport
    // ([`DecorPublish`]) — parallel arrays in **buffer** 0-based coordinates, stamped
    // with the viewport `gen`. Sentinels in the optional arrays mark an unset field:
    // `end_row`/`end_col`/`priority` of `-1` ⇒ absent, `hl` of `""` ⇒ none. The server
    // gen-gates the batch (drops it if the window scrolled past since the dispatch),
    // then clears the provider's `ns` on `buf` and re-sets these marks into the extmark
    // layer (a republish replaces the prior viewport's marks wholesale). Phase 3 of
    // `nx.decor`.
    type DecorPublishArgs = (
        u32,
        u64,
        u64,
        u64,
        Vec<i64>,
        Vec<i64>,
        Vec<i64>,
        Vec<i64>,
        Vec<String>,
        Vec<i64>,
    );
    let sh = shared.clone();
    nx.set(
        "_decor_publish",
        lua.create_function(
            move |_,
                  (ns, gen, win, buf, rows, cols, end_rows, end_cols, hls, priorities): DecorPublishArgs| {
                let marks = rows
                    .iter()
                    .enumerate()
                    .map(|(i, &row)| DecorMark {
                        row,
                        col: cols.get(i).copied().unwrap_or(0),
                        end_row: end_rows.get(i).copied().filter(|&r| r >= 0),
                        end_col: end_cols.get(i).copied().filter(|&c| c >= 0),
                        hl: hls.get(i).filter(|h| !h.is_empty()).cloned(),
                        priority: priorities.get(i).copied().filter(|&p| p >= 0).map(|p| p as u32),
                    })
                    .collect();
                sh.borrow_mut().decor_publishes.push(DecorPublish {
                    ns,
                    gen,
                    win,
                    buf,
                    marks,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._confirm(label, accelerators, default, cb_id)`: queue a `vim.fn.confirm`
    // button dialog ([`ConfirmReq`]). The server opens the command line as a
    // single-key confirm prompt showing `label`; a keypress matching one of
    // `accelerators` (or `<CR>` → `default`, `<Esc>` → 0) resolves it, firing
    // `nx._cb_fns[cb_id]` with the chosen 1-based index to resume the blocked
    // `vim.fn.confirm` call.
    let sh = shared.clone();
    nx.set(
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

    // `nx._ui_opener()`: the OS file/URL opener argv prefix `vim.ui.open` spawns
    // (via the async `vim.system`), chosen by platform — `open` on macOS,
    // `xdg-open` elsewhere (Phase 8). The path is appended by the Lua wrapper.
    nx.set(
        "_ui_opener",
        lua.create_function(|_, ()| {
            Ok(match std::env::consts::OS {
                "macos" => vec!["open".to_string()],
                "windows" => vec!["explorer".to_string()],
                _ => vec!["xdg-open".to_string()],
            })
        })?,
    )?;

    // `nx._substitute(input, pat, sub, flags)`: the engine behind
    // `vim.fn.substitute` — a real vim-regex substitution (vim's magic dialect +
    // replacement syntax, NOT nxvim's standard-regex `/` search). An invalid or
    // unsupported pattern raises (fail loud), never a fake identity result.
    nx.set(
        "_substitute",
        lua.create_function(
            |_, (input, pat, sub, flags): (String, String, String, String)| {
                vimregex::substitute(&input, &pat, &sub, &flags).map_err(mlua::Error::RuntimeError)
            },
        )?,
    )?;

    // `nx._set_reg(name, text, linewise, append)`: queue a [`RegisterSetOp`] for
    // the server to apply to the editor's register file after the chunk — the
    // write half of `vim.fn.setreg`. The Lua wrapper has already rejected
    // read-only specials, resolved an uppercase name / `a` flag into `append`,
    // and written through the `nx._registers` mirror for read-after-write within
    // the chunk; this only records the deferred write.
    let sh = shared.clone();
    nx.set(
        "_set_reg",
        lua.create_function(
            move |_, (name, text, linewise, append): (String, String, bool, bool)| {
                let name = name.chars().next().unwrap_or('"');
                sh.borrow_mut().reg_ops.push(RegisterSetOp {
                    name,
                    text,
                    linewise,
                    append,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._set_qflist(items, lines, efm, action, title)`: queue a [`QfSetOp`] for
    // the server to apply to the editor's quickfix list after the chunk — the
    // write half of `vim.fn.setqflist`. `items` is an array of entry dicts (the
    // structured form) or nil; `lines` is an array of strings parsed against `efm`
    // (the `{lines=…}` form) or nil. The Lua wrapper normalizes the public
    // `setqflist(list, action, what)` signature down to these positionals.
    let sh = shared.clone();
    nx.set(
        "_set_qflist",
        lua.create_function(
            move |_, (items, lines, efm, action, title, loclist_win): QfSetArgs| {
                let items = match items {
                    Some(tbl) => {
                        let mut out = Vec::with_capacity(tbl.raw_len());
                        for pair in tbl.sequence_values::<mlua::Table>() {
                            out.push(qf_item_from_table(&pair?)?);
                        }
                        Some(out)
                    }
                    None => None,
                };
                let lines = match lines {
                    Some(tbl) => {
                        let mut out = Vec::with_capacity(tbl.raw_len());
                        for s in tbl.sequence_values::<String>() {
                            out.push(s?);
                        }
                        Some(out)
                    }
                    None => None,
                };
                // Default action is `' '` (new list); a non-empty arg's first char.
                let action = action.and_then(|a| a.chars().next()).unwrap_or(' ');
                sh.borrow_mut().qf_ops.push(QfSetOp {
                    items,
                    lines,
                    efm,
                    action,
                    title,
                    open: false,
                    goto_first: false,
                    loclist_win,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._qf_populate(lines, efm, title, open, jump)`: the `:make`/`:grep`
    // completion path. Like `nx._set_qflist` with a raw-lines payload, but it also
    // carries the post-populate window/jump behavior vim's `:make` adds — open the
    // quickfix window iff there are entries (`open`), then jump to the first valid
    // one (`jump`, suppressed by a `!`). The async producer's `on_exit` calls it
    // with the child's combined output already split into lines.
    let sh = shared.clone();
    nx.set(
        "_qf_populate",
        lua.create_function(
            move |_,
                  (lines, efm, title, open, jump, loclist_win): (
                Vec<String>,
                String,
                String,
                bool,
                bool,
                Option<u64>,
            )| {
                sh.borrow_mut().qf_ops.push(QfSetOp {
                    items: None,
                    lines: Some(lines),
                    efm: Some(efm),
                    action: ' ',
                    title: Some(title),
                    open,
                    goto_first: jump,
                    loclist_win,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._nx_set_ts_query(lang, name, text|nil)`: the native query setter behind
    // `nx.treesitter.set_query`. Queues a [`TsOp::SetQuery`] the server pushes
    // straight to the engine — no Lua merge/resolution. `nil` text drops the override.
    let sh = shared.clone();
    nx.set(
        "_nx_set_ts_query",
        lua.create_function(
            move |_, (lang, name, text): (String, String, Option<String>)| {
                sh.borrow_mut()
                    .ts_ops
                    .push(TsOp::SetQuery { lang, name, text });
                Ok(())
            },
        )?,
    )?;

    // `vim.regex(pat)`: compile a vim pattern into a regex object exposing
    // `:match_str(text)` -> (start, end) byte offsets or nil. Backs `query.lua`'s
    // `#match?` predicate (it compiles each `\v`-prefixed pattern through here).
    // Same vim-magic dialect as `vim.fn.substitute`; an invalid pattern raises.
    vim.set(
        "regex",
        lua.create_function(|_, pat: String| {
            Ok(LuaRegex {
                re: vimregex::compile(&pat).map_err(mlua::Error::RuntimeError)?,
            })
        })?,
    )?;

    // `nx._system(cmd, cwd, env, text)`: spawn `cmd` (an argv list — no shell),
    // block until it exits, and return `{ code, stdout, stderr, pid }`. The pure-Lua
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
    //
    // The actual spawn goes through the injected [`BlockingSystem`] seam
    // (`Shared::blocking_system`): the default [`StdBlockingSystem`] spawns locally
    // (today's behavior), while a daemon session injects a blocking bridge so the
    // shell-out runs on the remote where the project files are (edit-host split,
    // Phase 3, Open Decision #5's residual blocking-bridge note). The bridge parks
    // this thread on the daemon's reply — the same as the local spawn blocks on its
    // `wait` — so the call stays synchronous either way.
    let sh = shared.clone();
    nx.set(
        "_system",
        lua.create_function(
            move |lua,
                  (cmd, cwd, env, _text): (
                Vec<String>,
                Option<String>,
                Option<Table>,
                Option<bool>,
            )| {
                let spec = crate::SystemSpec {
                    cmd,
                    cwd,
                    env: crate::convert::env_pairs(env)?,
                };
                // Take the injected backend out of `Shared` (cloning the `Rc`) and drop
                // the borrow *before* running — the run blocks the thread (locally on
                // `wait`, remotely on the daemon reply), and we must not hold a `RefCell`
                // borrow across it.
                let backend = sh.borrow().blocking_system.clone();
                let out = match backend {
                    Some(backend) => backend.run(spec),
                    None => crate::StdBlockingSystem.run(spec),
                };
                let result = lua.create_table()?;
                if let Some(pid) = out.pid {
                    result.set("pid", pid)?;
                }
                result.set("code", out.code)?;
                result.set("stdout", lua.create_string(&out.stdout)?)?;
                result.set("stderr", lua.create_string(&out.stderr)?)?;
                Ok(result)
            },
        )?,
    )?;

    // `nx._json_decode(str)`: parse a JSON document into the equivalent Lua value
    // (objects -> string-keyed tables, arrays -> sequences, `null` -> nil). Backs
    // `vim.json.decode`; raises on malformed input, matching neovim. The config
    // path that reaches for it is rust_analyzer's `root_dir`, decoding the
    // `cargo metadata` output to read `workspace_root`.
    nx.set(
        "_json_decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            json_to_lua(lua, &value)
        })?,
    )?;

    // `nx._json_encode(value)`: serialize a Lua value to a JSON string, using the
    // same array-vs-object rule as [`lua_to_rmpv`]. Backs `vim.json.encode`.
    nx.set(
        "_json_encode",
        lua.create_function(|_, value: mlua::Value| {
            serde_json::to_string(&lua_to_json(&value)?).map_err(mlua::Error::external)
        })?,
    )?;

    // ----- additional vim.fn (filesystem / process / PATH) --------------------
    // `vim.fn.executable(name)`: 1 if `name` is an executable on $PATH (or an
    // executable file path), else 0. Configs use it to prefer a project-local
    // `node_modules/.bin/<server>` over the global one.
    let sh = shared.clone();
    func.set(
        "executable",
        lua.create_function(move |_, name: String| {
            Ok(i64::from(resolve_lua_fs(&sh).which(&name).is_some()))
        })?,
    )?;
    // `vim.fn.exepath(name)`: the resolved path to `name` on $PATH, or "".
    let sh = shared.clone();
    func.set(
        "exepath",
        lua.create_function(move |_, name: String| {
            Ok(resolve_lua_fs(&sh).which(&name).unwrap_or_default())
        })?,
    )?;
    // `vim.fn.getpid()`: this (editor) process's id.
    func.set(
        "getpid",
        lua.create_function(|_, ()| Ok(std::process::id() as i64))?,
    )?;
    // `vim.fn.resolve(path)`: `path` with symlinks resolved, or unchanged if it
    // can't be canonicalized.
    let sh = shared.clone();
    func.set(
        "resolve",
        lua.create_function(move |_, path: String| {
            Ok(resolve_lua_fs(&sh).realpath(&path).unwrap_or(path))
        })?,
    )?;
    // `vim.fn.filereadable(path)`: 1 if `path` is a regular file, else 0.
    let sh = shared.clone();
    func.set(
        "filereadable",
        lua.create_function(move |_, path: String| {
            let is_file = matches!(
                resolve_lua_fs(&sh).stat(&path),
                Ok(st) if st.kind == crate::FileKind::File
            );
            Ok(i64::from(is_file))
        })?,
    )?;
    // `vim.fn.readblob(path)`: the file's raw bytes as a string; errors if
    // unreadable (callers `pcall` it).
    let sh = shared.clone();
    func.set(
        "readblob",
        lua.create_function(move |lua, path: String| {
            lua.create_string(
                resolve_lua_fs(&sh)
                    .read_file(&path)
                    .map_err(mlua::Error::external)?,
            )
        })?,
    )?;
    // `vim.fn.glob(pattern[, nosuf, list])`: existing paths matching a shell-style
    // glob (`*`/`?` wildcards, per path component). Returns a list when `list` is
    // truthy (the form `lspconfig.util.root_pattern` uses), else the default
    // newline-joined string. `nosuf` is accepted and ignored.
    let sh = shared.clone();
    func.set(
        "glob",
        lua.create_function(
            move |lua, (pattern, _nosuf, list): (String, Option<bool>, Option<bool>)| {
                let paths = glob_paths(&pattern, resolve_lua_fs(&sh).as_ref());
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

/// The positional arguments `nx._set_qflist` receives: `(items, lines, efm,
/// action, title, loclist_win)`, where `items`/`lines` are Lua arrays, the next
/// three are strings, and `loclist_win` is the location-list target window
/// (`nil`/absent for the quickfix list, `0` for the current window — see
/// [`QfSetOp::loclist_win`]).
type QfSetArgs = (
    Option<mlua::Table>,
    Option<mlua::Table>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
);

/// Convert one `setqflist` entry dict into a [`QfItem`]. Absent keys take their
/// zero value; `valid` defaults to "has a line number" (vim's rule) when omitted.
fn qf_item_from_table(t: &Table) -> mlua::Result<QfItem> {
    let str_or =
        |k: &str| -> mlua::Result<String> { Ok(t.get::<Option<String>>(k)?.unwrap_or_default()) };
    let int_or =
        |k: &str, d: i64| -> mlua::Result<i64> { Ok(t.get::<Option<i64>>(k)?.unwrap_or(d)) };
    let lnum = int_or("lnum", 0)?;
    let valid = match t.get::<Option<bool>>("valid")? {
        Some(v) => v,
        None => lnum > 0,
    };
    Ok(QfItem {
        filename: t.get::<Option<String>>("filename")?,
        bufnr: int_or("bufnr", 0)? as i32,
        module: str_or("module")?,
        lnum: lnum.max(0) as usize,
        end_lnum: int_or("end_lnum", 0)?.max(0) as usize,
        col: int_or("col", 0)?.max(0) as usize,
        end_col: int_or("end_col", 0)?.max(0) as usize,
        vcol: t.get::<Option<bool>>("vcol")?.unwrap_or(false),
        nr: int_or("nr", -1)? as i32,
        pattern: str_or("pattern")?,
        text: str_or("text")?,
        typ: str_or("type")?,
        valid,
    })
}

/// The Lua mirror table backing `nvim_get_hl(ns, …)` for namespace `ns`:
/// `nx._hl_defs` for the global namespace (`0`), or `nx._hl_defs_ns[ns]` for a
/// non-zero one. Both the outer `_hl_defs_ns` map and the per-namespace inner
/// table are created on first use. Keeping namespaces in separate tables (rather
/// than one flat table) is what stops a non-zero-namespace write from clobbering
/// the global definition a colorscheme set.
fn hl_mirror_table(lua: &Lua, nx: &Table, ns: u32) -> mlua::Result<Table> {
    if ns == 0 {
        return match nx.get::<Option<Table>>("_hl_defs")? {
            Some(t) => Ok(t),
            None => {
                let t = lua.create_table()?;
                nx.set("_hl_defs", &t)?;
                Ok(t)
            }
        };
    }
    // Non-zero namespace: `nx._hl_defs_ns[ns]`, keyed by the numeric namespace
    // id (matching the server's `set_hl_mirror_ns` push and the prelude reader).
    let by_ns: Table = match nx.get::<Option<Table>>("_hl_defs_ns")? {
        Some(t) => t,
        None => {
            let t = lua.create_table()?;
            nx.set("_hl_defs_ns", &t)?;
            t
        }
    };
    match by_ns.get::<Option<Table>>(ns)? {
        Some(t) => Ok(t),
        None => {
            let t = lua.create_table()?;
            by_ns.set(ns, &t)?;
            Ok(t)
        }
    }
}

/// Write (or clear) the namespace mirror row for a highlight group
/// `nvim_set_hl` just defined, so a *same-turn* `nvim_get_hl` / `hlexists` reads
/// it. The row goes into the table for `hl.ns` ([`hl_mirror_table`]), and must
/// match byte-for-byte the one the server's between-turn push derives
/// ([`crate::runtime::HlDefMirror`] → `set_hl_mirror` / `set_hl_mirror_ns`):
/// colors as `0xRRGGBB` ints, boolean attrs present only when `true`, and a
/// *blank* def (no colors after parsing, no attrs, no link — what neovim treats
/// as a clear) *removing* the key, matching
/// `nxvim_core::highlight::Highlights::set_ns`, which drops a cleared group from
/// the target table. Attrs are mirrored even alongside a `link` (parity with the
/// server fold, which copies every field unconditionally).
fn write_hl_mirror_row(lua: &Lua, hl: &HlSet) -> mlua::Result<()> {
    let fg = hl.fg.as_deref().and_then(color_to_u32);
    let bg = hl.bg.as_deref().and_then(color_to_u32);
    let sp = hl.sp.as_deref().and_then(color_to_u32);
    let blank = fg.is_none()
        && bg.is_none()
        && sp.is_none()
        && !hl.bold
        && !hl.italic
        && !hl.underline
        && !hl.undercurl
        && !hl.strikethrough
        && !hl.reverse
        && hl.link.is_none();

    let nx: Table = lua.globals().get("nx")?;
    let defs = hl_mirror_table(lua, &nx, hl.ns)?;
    if blank {
        defs.set(hl.name.as_str(), mlua::Value::Nil)?;
        return Ok(());
    }
    let row = lua.create_table()?;
    if let Some(c) = fg {
        row.set("fg", c)?;
    }
    if let Some(c) = bg {
        row.set("bg", c)?;
    }
    if let Some(c) = sp {
        row.set("sp", c)?;
    }
    if hl.bold {
        row.set("bold", true)?;
    }
    if hl.italic {
        row.set("italic", true)?;
    }
    if hl.underline {
        row.set("underline", true)?;
    }
    if hl.undercurl {
        row.set("undercurl", true)?;
    }
    if hl.strikethrough {
        row.set("strikethrough", true)?;
    }
    if hl.reverse {
        row.set("reverse", true)?;
    }
    if let Some(l) = &hl.link {
        row.set("link", l.as_str())?;
    }
    defs.set(hl.name.as_str(), row)?;
    Ok(())
}
