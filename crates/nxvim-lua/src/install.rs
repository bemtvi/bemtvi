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
use unicode_width::UnicodeWidthStr;

use crate::convert::{
    color_field, color_to_u32, env_pairs, flag_field, json_to_lua, lua_i64, lua_to_json,
    opt_table_to_json, stringify,
};
use crate::host::{get_runtime_file, stdpath};
use crate::ops::{
    BufOp, CompletePush, CompleteSetupReq, ConfirmReq, DecorMark, DecorPublish, DiagnosticData,
    DockOp, ExtmarkOp, FeedKeysOp, FsJob, GlobalOptionOp, HlSet, LayerOp, LoopOp, LspOp,
    OptionValue, PanelOp, PickerOpenReq, PickerPush, PreviewPush, QfItem, QfSetOp, RegisterSetOp,
    SnippetAddReq, SnippetSetupReq, StatuslineKind, StatuslinePublishReq, StatuslineSetupReq,
    StatuslineTarget, TabOp, TerminalOpenReq, TsOp, UiFloatReq, UiInputReq, UiSelectReq, ViewOp,
    VirtChunkData, VirtDecorData, WindowOp,
};
use crate::runtime::{OutputLine, Shared};
use crate::vimregex;

/// `vim.regex(pat)` userdata: a vim pattern compiled by the real vim regexp engine
/// ([`nxvim_regex`]). Its `:match_str(text)` returns the match's `(start, end)`
/// byte offsets or `nil` — the shape neovim's regex object exposes. The reported
/// span honours `\zs`/`\ze`.
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

/// Parse one neovim virtual-text chunk `{ text, hl_group? }` into a
/// [`VirtChunkData`]. `hl_group` may be a string or absent; a list-of-groups
/// (neovim's stacked form) is rejected loud rather than silently dropped, matching
/// the `hl_group` handling in `nvim_buf_set_extmark`.
/// Read the `margin` field of a geometry config table into the `[top, right,
/// bottom, left]` cells the wire carries. The prelude's `nx._geom` normalizer
/// always emits margin as a 4-element array (a number / `{v,h}` / `{t,r,b,l}` /
/// `{top=,…}` are all expanded there), so an absent or malformed value is treated
/// as no margin.
/// The argument tuple of the `nx._picker_open` bridge: `(dynamic, width, height,
/// align, margin, prompt_bottom, preview, query, title)` — width/height/align are
/// raw specs the server parses, `margin` is a `[top, right, bottom, left]` array,
/// `query` is the initial prompt text (empty for the historical empty-prompt
/// open), and `title` is the optional box title. Aliased to keep clippy's
/// complex-type lint quiet on the closure signature.
type PickerOpenArgs = (
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<u64>>,
    Option<bool>,
    Option<bool>,
    Option<String>,
    Option<String>,
    Option<bool>,
);

fn read_margin(cfg: &Table) -> mlua::Result<[u64; 4]> {
    let m: Option<Vec<u64>> = cfg.get("margin")?;
    Ok(match m {
        Some(v) if v.len() == 4 => [v[0], v[1], v[2], v[3]],
        _ => [0; 4],
    })
}

fn virt_chunk_from_table(t: &Table) -> mlua::Result<VirtChunkData> {
    let text: String = t.get(1)?;
    let hl_group = match t.get::<mlua::Value>(2)? {
        mlua::Value::Nil => None,
        mlua::Value::String(s) => Some(s.to_str()?.to_owned()),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "virt_text chunk hl_group must be a string (group lists not supported yet)".into(),
            ))
        }
    };
    Ok(VirtChunkData { text, hl_group })
}

/// Parse a chunk list (`{ {text, hl}, … }`) into `Vec<VirtChunkData>`.
fn virt_chunks_from_table(list: &Table) -> mlua::Result<Vec<VirtChunkData>> {
    let mut out = Vec::new();
    for chunk in list.clone().sequence_values::<Table>() {
        out.push(virt_chunk_from_table(&chunk?)?);
    }
    Ok(out)
}

/// Lower a `nvim_buf_set_extmark` `decoration` table (the virtual-text payload the
/// prelude collected) into a [`VirtDecorData`]. Returns `None` when it carries
/// neither `virt_text` nor `virt_lines` — a decoration of only not-yet-rendered
/// keys (signs, conceal, …) stores nothing renderable, so no op payload is needed.
fn virt_decor_from_table(t: &Table) -> mlua::Result<Option<VirtDecorData>> {
    let virt_text = match t.get::<Option<Table>>("virt_text")? {
        Some(list) => virt_chunks_from_table(&list)?,
        None => Vec::new(),
    };
    let virt_lines = match t.get::<Option<Table>>("virt_lines")? {
        Some(lines) => {
            let mut out = Vec::new();
            for line in lines.sequence_values::<Table>() {
                out.push(virt_chunks_from_table(&line?)?);
            }
            out
        }
        None => Vec::new(),
    };
    // A gutter sign: a 1–2 cell glyph drawn in the sign column on the mark's line.
    // `sign_text` alone (no virt_text/lines) is a renderable decoration, so it must
    // not hit the "nothing renderable" early-return below.
    let sign_text = t.get::<Option<String>>("sign_text")?;
    let sign_hl_group = t.get::<Option<String>>("sign_hl_group")?;
    // `line_fill = { text = <str>, hl_group = <str?> }` — an nx-native whole-line
    // fill (the text repeated across the line). Likewise renderable on its own.
    let line_fill = match t.get::<Option<Table>>("line_fill")? {
        Some(f) => Some(VirtChunkData {
            text: f.get::<String>("text")?,
            hl_group: f.get::<Option<String>>("hl_group")?,
        }),
        None => None,
    };
    if virt_text.is_empty() && virt_lines.is_empty() && sign_text.is_none() && line_fill.is_none() {
        return Ok(None);
    }
    // Reject an unknown `virt_text_pos` / `hl_mode` loud here (at the scripting
    // boundary, where the error names the bad value) so the server can match the
    // string against a closed set without a silent fallback.
    let virt_text_pos = t.get::<Option<String>>("virt_text_pos")?;
    if let Some(p) = &virt_text_pos {
        if !matches!(p.as_str(), "eol" | "inline" | "overlay" | "right_align") {
            return Err(mlua::Error::RuntimeError(format!(
                "nvim_buf_set_extmark: virt_text_pos '{p}' is not one of eol|inline|overlay|right_align"
            )));
        }
    }
    let hl_mode = t.get::<Option<String>>("hl_mode")?;
    if let Some(m) = &hl_mode {
        if !matches!(m.as_str(), "replace" | "combine" | "blend") {
            return Err(mlua::Error::RuntimeError(format!(
                "nvim_buf_set_extmark: hl_mode '{m}' is not one of replace|combine|blend"
            )));
        }
    }
    Ok(Some(VirtDecorData {
        virt_text,
        virt_text_pos,
        virt_text_win_col: t.get::<Option<i64>>("virt_text_win_col")?,
        virt_text_hide: t.get::<Option<bool>>("virt_text_hide")?.unwrap_or(false),
        hl_mode,
        virt_lines,
        virt_lines_above: t.get::<Option<bool>>("virt_lines_above")?.unwrap_or(false),
        sign_text,
        sign_hl_group,
        line_fill,
    }))
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
        sh.borrow_mut().output.push(OutputLine {
            text: msg,
            error: false,
        });
        Ok(())
    })?;
    nx.set("echo", echo)?;
    // `nx._echo_err(msg)`: the error sibling of `nx.echo` — backs `nx.err_write` /
    // `nx.err_writeln` (and the `nvim_err_write*` aliases) so an error message is
    // routed through the core's `echo_err` and painted red, rather than blending in
    // via `print`.
    let sh = shared.clone();
    let echo_err = lua.create_function(move |_, msg: String| {
        sh.borrow_mut().output.push(OutputLine {
            text: msg,
            error: true,
        });
        Ok(())
    })?;
    nx.set("_echo_err", echo_err)?;
    // `nx._strwidth(s)` (exposed by the prelude as `nx.str.width`): the display
    // width of `s` in terminal cells — wide (CJK / emoji) graphemes count as two,
    // combining marks as zero — via the same `unicode-width` table the core
    // renderer measures with (`nxvim_core::unicode::display_width`), so Lua layout
    // code and the rendered frame agree. Like that helper it does *not* expand
    // tabs; align/layout helpers feed it tab-free line content.
    nx.set(
        "_strwidth",
        lua.create_function(|_, s: String| Ok(UnicodeWidthStr::width(s.as_str())))?,
    )?;
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

    // `nx.open(path, { where })` — open a file/dir in the editing area, queuing a
    // [`LayerOp::Open`]. `where = "main"` crosses to the Main layer first (so an open
    // fired from a dock keymap lands in the main editor, not the sidebar); the
    // default opens in the current window like `:edit`.
    let sh = shared.clone();
    nx.set(
        "open",
        lua.create_function(move |_, (path, opts): (String, Option<mlua::Table>)| {
            let where_main = match opts {
                Some(o) => o.get::<Option<String>>("where")?.as_deref() == Some("main"),
                None => false,
            };
            sh.borrow_mut()
                .layer_ops
                .push(LayerOp::Open { path, where_main });
            Ok(())
        })?,
    )?;
    // `nx.layer` — focus the main editor area or a dock by name, queuing a
    // [`LayerOp::Focus`]. `nx.layer.main()` is the shorthand for `focus("main")`.
    let layer = lua.create_table()?;
    let sh = shared.clone();
    layer.set(
        "focus",
        lua.create_function(move |_, target: String| {
            sh.borrow_mut().layer_ops.push(LayerOp::Focus { target });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    layer.set(
        "main",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().layer_ops.push(LayerOp::Focus {
                target: "main".to_string(),
            });
            Ok(())
        })?,
    )?;
    nx.set("layer", layer)?;

    // `nx.view` raw bridges — each queues a [`ViewOp`] the server drains into the
    // core's view registry. The handle object (`nx.view.create` returning a table
    // with `:set_lines` / `:mount` / `:on_select` / …) is authored in the prelude
    // over these primitives; the Lua-side state (per-line userdata, the `on_select`
    // callback) lives in that handle, so only these content / mount / lifecycle
    // signals cross the bridge. `id` is the Lua-allocated handle id.
    let view = lua.create_table()?;
    let sh = shared.clone();
    view.set(
        "_create",
        lua.create_function(move |_, (id, name, filetype): (u64, String, String)| {
            sh.borrow_mut()
                .view_ops
                .push(ViewOp::Create { id, name, filetype });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_set_lines",
        lua.create_function(move |_, (id, lines): (u64, Vec<String>)| {
            sh.borrow_mut()
                .view_ops
                .push(ViewOp::SetLines { id, lines });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_set_cursor",
        lua.create_function(move |_, (id, line): (u64, u64)| {
            sh.borrow_mut()
                .view_ops
                .push(ViewOp::SetCursor { id, line });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_mount_dock",
        lua.create_function(move |_, (id, side, size): (u64, String, Option<u64>)| {
            sh.borrow_mut()
                .view_ops
                .push(ViewOp::MountDock { id, side, size });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_mount_split",
        lua.create_function(move |_, (id, vertical): (u64, bool)| {
            sh.borrow_mut()
                .view_ops
                .push(ViewOp::MountSplit { id, vertical });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_mount_tab",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().view_ops.push(ViewOp::MountTab { id });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_mount_float",
        lua.create_function(move |_, (id, cfg): (u64, Table)| {
            sh.borrow_mut().view_ops.push(ViewOp::MountFloat {
                id,
                relative: cfg.get("relative")?,
                win: cfg.get::<Option<u64>>("win")?.unwrap_or(0),
                anchor: cfg.get("anchor")?,
                row: cfg.get("row")?,
                col: cfg.get("col")?,
                width: cfg.get("width")?,
                height: cfg.get("height")?,
                align: cfg.get::<Option<String>>("align")?,
                margin: read_margin(&cfg)?,
                zindex: cfg.get("zindex")?,
                focusable: cfg.get("focusable")?,
                border: cfg.get("border")?,
                title: cfg.get::<Option<String>>("title")?,
                grab: cfg.get("grab")?,
            });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_unmount",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().view_ops.push(ViewOp::Unmount { id });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_focus",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().view_ops.push(ViewOp::Focus { id });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    view.set(
        "_destroy",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().view_ops.push(ViewOp::Destroy { id });
            Ok(())
        })?,
    )?;
    nx.set("view", view)?;

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

    // `nx.panel`: the transient, focus-locked bottom overlay over an ordinary
    // `nomodifiable` buffer (the successor to the retired bespoke panel). The surface is
    // deliberately tiny — `open{ name?, lines, filetype?, height? }` and `close()` —
    // because all interaction rides ordinary buffer mechanisms: motions navigate, and
    // selection / dismissal are buffer-local maps a `FileType` autocmd installs (the
    // `:ls` / `qf` model). `name` (default `[Panel]`) makes the panel unique — re-opening
    // the same name replaces its content, and it shows under `:lspanels`. `filetype`
    // defaults to `nxpanel` (whose ftplugin maps `q`/`<Esc>` to close); a plugin passing
    // its own filetype wires its own keys. Same "Lua queues, core mutates" flow as
    // `nx.view` / `nx.terminal`.
    let panel = lua.create_table()?;
    let sh = shared.clone();
    panel.set(
        "open",
        lua.create_function(move |_, opts: mlua::Table| {
            let lines: Vec<String> = match opts.get::<mlua::Value>("lines")? {
                mlua::Value::Table(t) => {
                    t.sequence_values::<String>().collect::<mlua::Result<_>>()?
                }
                mlua::Value::Nil => {
                    return Err(mlua::Error::runtime(
                        "nx.panel.open: `lines` (a list of strings) is required",
                    ))
                }
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "nx.panel.open: `lines` must be a list of strings, got {}",
                        other.type_name()
                    )))
                }
            };
            let name: Option<String> = opts.get("name")?;
            let filetype: Option<String> = opts.get("filetype")?;
            let height: Option<String> = opts.get("height")?;
            let margin = read_margin(&opts)?;
            sh.borrow_mut().panel_ops.push(PanelOp::Open {
                name,
                lines,
                filetype,
                height,
                margin,
            });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    panel.set(
        "close",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().panel_ops.push(PanelOp::Close);
            Ok(())
        })?,
    )?;
    nx.set("panel", panel)?;

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
    // `nx._proc_open(id, cmd, cwd, env)`: spawn `cmd` (argv list) as a **duplex**
    // child in the event-loop actor — stdin stays open for `nx._proc_write`, and
    // stdout/stderr stream back as raw byte chunks via `nx._proc_recv(id, data,
    // is_stderr)` until the exit fires `nx._proc_exit(id, code)`. Backs
    // `nx.process.open` (the DAP / framed-protocol transport). Kill with
    // `nx._proc_kill` (the shared `LoopOp::Kill`).
    let sh = shared.clone();
    nx.set(
        "_proc_open",
        lua.create_function(
            move |_, (id, cmd, cwd, env): (u64, Vec<String>, Option<String>, Option<Table>)| {
                let env = env_pairs(env)?;
                sh.borrow_mut()
                    .loop_ops
                    .push(LoopOp::ProcOpen { id, cmd, cwd, env });
                Ok(())
            },
        )?,
    )?;
    // `nx._proc_write(id, data)`: feed `data` (a binary-safe Lua string) to the
    // running duplex child's stdin. A no-op if it already exited.
    let sh = shared.clone();
    nx.set(
        "_proc_write",
        lua.create_function(move |_, (id, data): (u64, mlua::String)| {
            let data = data.as_bytes().to_vec();
            sh.borrow_mut()
                .loop_ops
                .push(LoopOp::ProcWrite { id, data });
            Ok(())
        })?,
    )?;
    // `nx._proc_kill(id)`: terminate the duplex child under `id`. A dedicated op (not
    // the one-shot `LoopOp::Kill`) so a wasm session routes it to the `dproc_*` leg.
    let sh = shared.clone();
    nx.set(
        "_proc_kill",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::ProcClose { id });
            Ok(())
        })?,
    )?;
    // `nx._sock_connect(id, host, port)`: open a TCP client connection in the actor;
    // on success `nx._sock_connected(id)` fires, then incoming bytes stream back via
    // `nx._sock_data(id, data)` until `nx._sock_closed(id, err)`. Backs
    // `nx.socket.connect` (a DAP `type="server"` adapter transport).
    let sh = shared.clone();
    nx.set(
        "_sock_connect",
        lua.create_function(move |_, (id, host, port): (u64, String, u16)| {
            sh.borrow_mut()
                .loop_ops
                .push(LoopOp::SockConnect { id, host, port });
            Ok(())
        })?,
    )?;
    // `nx._sock_write(id, data)`: send `data` (a binary-safe Lua string) over the
    // connection. A no-op once it closed.
    let sh = shared.clone();
    nx.set(
        "_sock_write",
        lua.create_function(move |_, (id, data): (u64, mlua::String)| {
            let data = data.as_bytes().to_vec();
            sh.borrow_mut()
                .loop_ops
                .push(LoopOp::SockWrite { id, data });
            Ok(())
        })?,
    )?;
    // `nx._sock_close(id)`: shut the connection under `id`.
    let sh = shared.clone();
    nx.set(
        "_sock_close",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::SockClose { id });
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
            sh.borrow_mut().output.push(OutputLine {
                text: parts.join("\t"),
                error: false,
            });
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
    runtimepath: &Rc<RefCell<Vec<PathBuf>>>,
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let func: Table = vim.get("fn")?;
    let nx: Table = lua.globals().get("nx")?;

    // `nx.runtime_file(name, all)` (alias: `nvim_get_runtime_file`, set in the Lua
    // prelude): full paths of files matching `name` (a runtimepath-relative path,
    // the final component optionally globbed with `*`) across the runtimepath.
    // `all=false` returns the first match only. The `lsp/<server>.lua`
    // config-discovery primitive. Reads the LIVE runtimepath (cloned `Rc`) so a
    // plugin the package manager installed mid-session contributes its `lsp/` /
    // `queries/` / `colors/` immediately.
    let rtp = runtimepath.clone();
    let runtime_file = lua.create_function(move |lua, (name, all): (String, Option<bool>)| {
        let hits = get_runtime_file(&rtp.borrow(), &name, all.unwrap_or(false));
        lua.create_sequence_from(hits)
    })?;
    nx.set("runtime_file", runtime_file)?;

    // `nx._add_rtp(dir)`: append `dir` to the live runtimepath and prepend its
    // `lua/` patterns to `package.path`, so the directory's modules are
    // `require`-able and its `colors/` / `queries/` / `lsp/` resolve through
    // `nvim_get_runtime_file` — all without a restart. The package manager
    // (`nx.plugins`) calls this the instant a plugin is on disk, before it sources
    // the plugin's `plugin/` scripts or runs its `config`. Idempotent: a dir
    // already on the path is a no-op (no duplicate `package.path` entries).
    let rtp_add = runtimepath.clone();
    nx.set(
        "_add_rtp",
        lua.create_function(move |lua, dir: String| {
            let path = PathBuf::from(&dir);
            let mut paths = rtp_add.borrow_mut();
            if paths.contains(&path) {
                return Ok(false);
            }
            paths.push(path.clone());
            drop(paths);
            crate::host::seed_one_package_path(lua, &path)?;
            Ok(true)
        })?,
    )?;

    // `nx._runtime_paths()`: the live runtimepath as a list of absolute directory
    // strings (longest-prefix order is the caller's job). Backs `nx.shada.plugin`'s
    // namespace attribution — it maps the calling chunk's source file to the rtp
    // entry (plugin root) that contains it. Reads the same live `Rc` `_add_rtp`
    // mutates, so a mid-session-installed plugin is attributable immediately.
    let rtp_list = runtimepath.clone();
    nx.set(
        "_runtime_paths",
        lua.create_function(move |lua, ()| {
            let paths: Vec<String> = rtp_list
                .borrow()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            lua.create_sequence_from(paths)
        })?,
    )?;

    // `vim.fn.getcwd()`: the editor's effective working directory (the root fallback
    // and the base for relative->absolute path math in `vim.fs`/`fnamemodify`). The
    // server keeps `nx._cwd` equal to the effective dir on every change; read that when
    // set so a daemon session reports the *daemon's* cwd (which need not exist on the
    // local disk), falling back to the local process cwd before the first publish / for
    // a bare session. See `docs/plans/2026-06-23-remote-cwd.md`.
    func.set(
        "getcwd",
        lua.create_function(|lua, ()| {
            if let Ok(nx) = lua.globals().get::<mlua::Table>("nx") {
                if let Ok(Some(cwd)) = nx.get::<Option<String>>("_cwd") {
                    if !cwd.is_empty() {
                        return Ok(cwd);
                    }
                }
            }
            Ok(std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default())
        })?,
    )?;

    // ----- nx.shada: per-launch shada namespace + session capture (public API) -----
    //
    // A PUBLIC surface any plugin can build on (the bundled workspace plugin is just one
    // consumer). nxvim itself never chooses a namespace or reads a project file; a
    // launcher passes `--shada-namespace <id>` and these expose / drive the result.
    let shada_tbl = lua.create_table()?;

    // `nx.shada.namespace()` -> the shada namespace this launch is scoped to (the
    // `--shada-namespace` value), isolating its marks / registers / session under
    // `ns/<id>/`, or `nil` for the global store. READ-ONLY: stamped into the environment
    // by the binary before any Lua runs, so it reports the namespace THIS process was
    // launched with (which a project file read from Lua can't, since the store is fixed
    // first). A plugin compares it against its own config to detect a mismatch.
    shada_tbl.set(
        "namespace",
        lua.create_function(|_, ()| {
            Ok(match std::env::var("NXVIM_SHADA_NAMESPACE") {
                Ok(s) if !s.is_empty() => Some(s),
                _ => None,
            })
        })?,
    )?;

    // `nx.shada.save_layout(enable)` -> opt this session into CAPTURING the window/tab
    // layout (the exact split tree, open files, cursors, docks) into the shada,
    // persisted on exit and restored when the editor is launched with
    // `--restore-session`. Default OFF — a plugin turns it on once it knows the launch is
    // the namespace it wanted. Only meaningful with a namespace (the global store never
    // persists layout). How split / dock sizes are stored (proportional vs absolute) is
    // governed by the native `nx.o.relative_splits` / `nx.o.relative_docks` options,
    // read off the editor at capture — so any wrapper that opts in honors them.
    {
        let sh = shared.clone();
        shada_tbl.set(
            "save_layout",
            lua.create_function(move |_, enable: bool| {
                sh.borrow_mut().session_save_layout = enable;
                Ok(())
            })?,
        )?;
    }
    nx.set("shada", shada_tbl)?;

    // ----- nx.shada.plugin: opt-in, isolated per-plugin shada storage -------------
    //
    // The native half of the `nx.shada.plugin()` handle (the ergonomic method wrapper
    // and the namespace *attribution* live in `prelude/stdlib.lua`). A plugin's data
    // lives in its own `namespace -> (key -> value)` slice of `Shared::plugin_shada`,
    // keyed apart from the core registers / marks / history — so a plugin can reach
    // only its own namespace, never the editor's own shada state. The namespace is
    // *assigned* from where the calling code lives (its runtimepath / plugin dir), not
    // chosen by the plugin, so one plugin can't claim another's slice. Values are
    // stored as JSON (the same `lua_to_json` codec `nx.json.encode` uses), so `set`
    // accepts any JSON-able Lua value and `get` hands a fresh copy back. The server
    // seeds this map at shada load and harvests it at flush, so the data rides the
    // ordinary shada cadence.

    // `nx._shada_plugin_set(ns, key, value)`: JSON-encode `value` and store it under
    // `(ns, key)`. Overwrites any existing value for the key.
    {
        let sh = shared.clone();
        nx.set(
            "_shada_plugin_set",
            lua.create_function(move |_, (ns, key, value): (String, String, mlua::Value)| {
                let json = lua_to_json(&value)?;
                let encoded = serde_json::to_string(&json).map_err(mlua::Error::external)?;
                sh.borrow_mut()
                    .plugin_shada
                    .entry(ns)
                    .or_default()
                    .insert(key, encoded);
                Ok(())
            })?,
        )?;
    }

    // `nx._shada_plugin_get(ns, key)` -> the decoded value, or nil when unset.
    {
        let sh = shared.clone();
        nx.set(
            "_shada_plugin_get",
            lua.create_function(move |lua, (ns, key): (String, String)| {
                let encoded = sh
                    .borrow()
                    .plugin_shada
                    .get(&ns)
                    .and_then(|m| m.get(&key))
                    .cloned();
                match encoded {
                    Some(s) => {
                        let value: serde_json::Value =
                            serde_json::from_str(&s).map_err(mlua::Error::external)?;
                        json_to_lua(lua, &value)
                    }
                    None => Ok(mlua::Value::Nil),
                }
            })?,
        )?;
    }

    // `nx._shada_plugin_delete(ns, key)`: drop one key from the namespace.
    {
        let sh = shared.clone();
        nx.set(
            "_shada_plugin_delete",
            lua.create_function(move |_, (ns, key): (String, String)| {
                if let Some(m) = sh.borrow_mut().plugin_shada.get_mut(&ns) {
                    m.remove(&key);
                }
                Ok(())
            })?,
        )?;
    }

    // `nx._shada_plugin_keys(ns)` -> the namespace's keys, sorted (the `BTreeMap`
    // iterates in order).
    {
        let sh = shared.clone();
        nx.set(
            "_shada_plugin_keys",
            lua.create_function(move |lua, ns: String| {
                let keys: Vec<String> = sh
                    .borrow()
                    .plugin_shada
                    .get(&ns)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                lua.create_sequence_from(keys)
            })?,
        )?;
    }

    // `nx._shada_plugin_clear(ns)`: drop every key in the namespace.
    {
        let sh = shared.clone();
        nx.set(
            "_shada_plugin_clear",
            lua.create_function(move |_, ns: String| {
                sh.borrow_mut().plugin_shada.remove(&ns);
                Ok(())
            })?,
        )?;
    }

    // `nx.uuid()` -> a random v4 UUID in canonical `8-4-4-4-12` hex. A public utility
    // (the workspace plugin mints session namespaces with it).
    nx.set(
        "uuid",
        lua.create_function(|_, ()| {
            let mut b = [0u8; 16];
            // `getrandom::Error` only implements `std::error::Error` with getrandom's
            // `std` feature (dropped in the wasm-eligible no-default-features build), so
            // format via its always-present `Display` rather than `Error::external`.
            getrandom::fill(&mut b)
                .map_err(|e| mlua::Error::RuntimeError(format!("nx.uuid: {e}")))?;
            b[6] = (b[6] & 0x0f) | 0x40; // version 4
            b[8] = (b[8] & 0x3f) | 0x80; // variant 1 (RFC 4122)
            Ok(format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
                 {:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0],
                b[1],
                b[2],
                b[3],
                b[4],
                b[5],
                b[6],
                b[7],
                b[8],
                b[9],
                b[10],
                b[11],
                b[12],
                b[13],
                b[14],
                b[15]
            ))
        })?,
    )?;

    // `nx.argv()` -> the positional file arguments this process was launched with (a
    // list of strings; empty when none). A launcher / wrapper reads them to forward to a
    // relaunched editor. Carried through `NXVIM_ARGV` (newline-joined) so the binary
    // stays the single source of truth.
    nx.set(
        "argv",
        lua.create_function(|lua, ()| {
            let joined = std::env::var("NXVIM_ARGV").unwrap_or_default();
            let items: Vec<&str> = if joined.is_empty() {
                Vec::new()
            } else {
                joined.split('\n').collect()
            };
            lua.create_sequence_from(items)
        })?,
    )?;

    // `nx.reexec(args)` -> replace THIS process with a fresh `nxvim args…` (the current
    // executable). A launcher uses it to relaunch the editor with chosen flags (e.g.
    // `--shada-namespace` + `--restore-session`). On Unix this `execv`s (never returns
    // on success); elsewhere it spawns + exits with the child's status. Raises if the
    // exec / spawn itself fails.
    nx.set(
        "reexec",
        lua.create_function(|_, args: Vec<String>| -> mlua::Result<()> {
            let exe = std::env::current_exe()
                .map_err(|e| mlua::Error::RuntimeError(format!("nx.reexec: current_exe: {e}")))?;
            let mut cmd = std::process::Command::new(exe);
            cmd.args(&args);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // execv: replaces the image; only returns on failure.
                let err = cmd.exec();
                Err(mlua::Error::RuntimeError(format!("nx.reexec: {err}")))
            }
            #[cfg(not(unix))]
            {
                let status = cmd
                    .status()
                    .map_err(|e| mlua::Error::RuntimeError(format!("nx.reexec: {e}")))?;
                std::process::exit(status.code().unwrap_or(1));
            }
        })?,
    )?;

    // `nx.now_ms()`: wall-clock milliseconds since the Unix epoch, as a Lua number
    // (a float, so it stays width-safe on the wasm `i32` integer build). A real
    // time read for timing / scheduling math — the plugin test runner stamps each
    // test's duration with it, since `os.clock` measures CPU time (≈0 across an
    // awaited tick) rather than wall time. Public: useful to any plugin author.
    nx.set(
        "now_ms",
        lua.create_function(|_, ()| {
            Ok(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64() * 1000.0)
                .unwrap_or(0.0))
        })?,
    )?;

    // `nx._read_file(path)`: the file's contents, or nil if unreadable. Backs the
    // pure-Lua loader that sources an `lsp/<name>.lua` config (via `loadstring`),
    // sidestepping any `loadfile` sandbox question.
    nx.set(
        "_read_file",
        lua.create_function(|_, path: String| Ok(std::fs::read_to_string(&path).ok()))?,
    )?;

    // ===== nx.fs (one-shot ops) ===========================================
    // The promise-always Lua filesystem surface (docs/plans/2026-06-16-nx-fs-api.md +
    // the off-tick plan). `nx._fs_op(job, cb_id)` queues the op OFF the editor tick:
    // it pushes a [`LoopOp::Fs`] carrying the typed [`FsJob`] and the promise's
    // callback id, and returns immediately (the promise the `nx.fs.*` wrapper built
    // is still pending). The event-loop actor runs the op on its blocking pool against
    // its `LuaFs` clone (native) — or the daemon `luafs` leg runs it (wasm, Phase 2) —
    // and the result returns inbound as a `CallbackArgs::FsResult`, which fires
    // `nx._run_cb(cb_id, false, err, value)` to settle the promise. `job` is a table
    // `{ op = "<name>", … }`; an unknown op or a missing field fails loud here at the
    // scripting boundary rather than queuing a malformed op.
    let sh = shared.clone();
    nx.set(
        "_fs_op",
        lua.create_function(move |_, (job, cb_id): (Table, u64)| {
            let fs_job = fs_job_from_table(&job)?;
            sh.borrow_mut().loop_ops.push(LoopOp::Fs {
                id: cb_id,
                job: fs_job,
            });
            Ok(())
        })?,
    )?;
    // `nx._fs_watch(id, path, recursive)` / `nx._fs_unwatch(id)`: arm / cancel a
    // native filesystem watch feeding the Lua watch stream `id`. Changes fire back
    // as `nx._run_fs_watch(id, ev, err)` (prelude/fs.lua) until stopped. Queued like
    // the other loop ops; the event-loop actor coalesces bursts (10 ms).
    let sh = shared.clone();
    nx.set(
        "_fs_watch",
        lua.create_function(
            move |_, (id, path, recursive): (u64, String, Option<bool>)| {
                sh.borrow_mut().loop_ops.push(LoopOp::FsWatch {
                    id,
                    path,
                    recursive: recursive.unwrap_or(false),
                });
                Ok(())
            },
        )?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_fs_unwatch",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::FsUnwatch { id });
            Ok(())
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

    // `nx._test_clipboard_seed(text, linewise)`: queue a clipboard seed for the
    // server to write into the editor's clipboard provider (the plugin-test seam
    // behind `nx.test.clipboard.seed`). Reachable only via the gated `nx.test`.
    let sh = shared.clone();
    nx.set(
        "_test_clipboard_seed",
        lua.create_function(move |_, (text, linewise): (String, bool)| {
            sh.borrow_mut().clipboard_seeds.push((text, linewise));
            Ok(())
        })?,
    )?;

    // `nx._test_tempdir()`: create and return a fresh, unique temp directory — the
    // plugin-test seam behind `nx.test.tempdir()`, for a suite that exercises a
    // plugin's file I/O without colliding with other runs. Native (the test runner
    // is native); reachable only via the gated `nx.test`.
    nx.set(
        "_test_tempdir",
        lua.create_function(|_, ()| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("nxvim-test-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&dir)
                .map_err(|e| mlua::Error::external(format!("nx.test.tempdir: {e}")))?;
            Ok(dir.to_string_lossy().into_owned())
        })?,
    )?;

    // The extmark funnels (`nx._extmark_set` / `_extmark_del` / `_extmark_clear`):
    // queue an [`ExtmarkOp`] for the server to apply to the target buffer's
    // `ExtmarkStore` after the chunk. The Lua-facing `nvim_buf_set_extmark` family
    // (prelude) has resolved `bufnr`, allocated the id, and updated its
    // `nx._extmarks` mirror (write-through); the server converts the 0-based
    // `(row, col)` positions to byte offsets against the live rope.
    // `(bufnr, ns, id, row, col, end_row, end_col, hl_group, priority, decoration)`
    // — the positional payload the prelude's `nvim_buf_set_extmark` forwards.
    // `decoration` is the accepted-but-previously-unrendered virtual-text table
    // (`virt_text` / `virt_lines` / …), lowered here into a [`VirtDecorData`].
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
        Option<Table>,
    );
    let sh = shared.clone();
    nx.set(
        "_extmark_set",
        lua.create_function(
            move |_, (bufnr, ns, id, row, col, end_row, end_col, hl_group, priority, decoration): ExtmarkSetArgs| {
                let decor = match decoration {
                    Some(t) => virt_decor_from_table(&t)?.map(Box::new),
                    None => None,
                };
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
                    decor,
                });
                Ok(())
            },
        )?,
    )?;
    // `nx._buf_search(lines, pattern, opts)` — native buffer text search over the
    // mirror lines (no `shared` ops; a pure read). The Lua wrapper `nx.buf.search`
    // passes the mirror's line array; see crate::search.
    nx.set(
        "_buf_search",
        lua.create_function(move |lua, (lines, pattern, opts): (Table, String, Table)| {
            crate::search::buf_search(lua, lines, pattern, opts)
        })?,
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
    // boolean as `Bool`, and the string buffer options (`regexsyntax` /
    // `fileencoding` / `filetype` / `commentstring`) as `String`. Other Lua types
    // are ignored (the option set is typed:
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

    // `nx._buf_set_lines(bufnr, start, end, lines)`: queue a [`BufOp::SetLines`] for the
    // server to apply via `Editor::api_set_lines` (the lone buffer-text mutation). The
    // prelude (`nx.buf.set_lines` / `nvim_buf_set_lines`) has already validated the shape
    // and resolved `start`/`end` to 0-based, end-exclusive, clamped indices; here we only
    // marshal the `lines` table into a `Vec<String>` (non-string entries error loud, as
    // neovim does for a non-text replacement line).
    let sh = shared.clone();
    nx.set(
        "_buf_set_lines",
        lua.create_function(
            move |_, (bufnr, start, end, lines): (u64, i64, i64, Vec<String>)| {
                sh.borrow_mut().buf_ops.push(BufOp::SetLines {
                    bufnr,
                    start: start.max(0) as usize,
                    end: end.max(0) as usize,
                    lines,
                });
                Ok(())
            },
        )?,
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
        "_jump_to",
        lua.create_function(
            move |_,
                  (path, line, col, mode, to_main): (
                String,
                usize,
                usize,
                Option<String>,
                Option<bool>,
            )| {
                let target = crate::ops::OpenTarget::from_mode(mode.as_deref());
                sh.borrow_mut().window_ops.push(WindowOp::Jump {
                    path,
                    line,
                    col,
                    target,
                    to_main: to_main.unwrap_or(false),
                });
                Ok(())
            },
        )?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_open",
        lua.create_function(move |_, (path, to_main): (String, Option<bool>)| {
            sh.borrow_mut().window_ops.push(WindowOp::OpenSwitchbuf {
                path,
                to_main: to_main.unwrap_or(false),
            });
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    nx.set(
        "_buf_switch",
        lua.create_function(
            move |_, (buf, mode, to_main): (u64, Option<String>, Option<bool>)| {
                let target = crate::ops::OpenTarget::from_mode(mode.as_deref());
                sh.borrow_mut().window_ops.push(WindowOp::BufSwitch {
                    buf,
                    target,
                    to_main: to_main.unwrap_or(false),
                });
                Ok(())
            },
        )?,
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
        "_win_set_leftcol",
        lua.create_function(move |_, (win, leftcol): (u64, usize)| {
            sh.borrow_mut()
                .window_ops
                .push(WindowOp::SetLeftcol { win, leftcol });
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
                mlua::Value::String(s) => {
                    s.to_str().ok().map(|s| OptionValue::String(s.to_string()))
                }
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
                align: cfg.get::<Option<String>>("align")?,
                margin: read_margin(&cfg)?,
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
            let margin: Option<Vec<u64>> = cfg.get("margin")?;
            sh.borrow_mut().window_ops.push(WindowOp::SetConfig {
                win,
                relative: cfg.get::<Option<String>>("relative")?,
                parent: cfg.get::<Option<u64>>("win")?.unwrap_or(0),
                anchor: cfg.get::<Option<String>>("anchor")?,
                row: cfg.get::<Option<i64>>("row")?,
                col: cfg.get::<Option<i64>>("col")?,
                width: cfg.get::<Option<String>>("width")?,
                height: cfg.get::<Option<String>>("height")?,
                align: cfg.get::<Option<String>>("align")?,
                margin: margin
                    .filter(|m| m.len() == 4)
                    .map(|m| [m[0], m[1], m[2], m[3]]),
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

    // `nx._signature_autotrigger(enable)`: queue [`LspOp::SignatureAutoTrigger`] —
    // opt into auto-showing signature help as you type a call (the server's advertised
    // trigger chars drive it). The prelude's `nx.lsp.signature_help_autotrigger` wraps it.
    let sh = shared.clone();
    nx.set(
        "_signature_autotrigger",
        lua.create_function(move |_, enable: bool| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::SignatureAutoTrigger { enable });
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

    // `nx._set_client_diagnostics(bufnr, list)`: queue [`LspOp::SetClientDiagnostics`]
    // — replace `bufnr`'s server-side render store with the prelude-flattened
    // client-set diagnostics (every namespace merged into one list). The prelude
    // normalizes each entry to the `{lnum,col,end_lnum,end_col,severity,message,
    // source}` shape before pushing, so the fields are always present; positions
    // are native byte columns. An empty `list` clears the buffer's store.
    let sh = shared.clone();
    nx.set(
        "_set_client_diagnostics",
        lua.create_function(move |_, (bufnr, list): (u64, Vec<Table>)| {
            let diags = list
                .into_iter()
                .map(|d| {
                    let lnum: i64 = d.get("lnum")?;
                    let col: i64 = d.get("col")?;
                    Ok(DiagnosticData {
                        lnum,
                        col,
                        end_lnum: d.get("end_lnum").unwrap_or(lnum),
                        end_col: d.get("end_col").unwrap_or(col),
                        severity: d.get("severity").unwrap_or(1),
                        message: d.get("message").unwrap_or_default(),
                        source: d.get("source").ok(),
                    })
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::SetClientDiagnostics { bufnr, diags });
            Ok(())
        })?,
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

    // `nx._lsp_workspace_symbol(query)`: queue [`LspOp::WorkspaceSymbol`] —
    // `nx.lsp.workspace_symbol(query)` requests `workspace/symbol` for the fuzzy
    // query and opens the matching symbols in `nx.picker`.
    let sh = shared.clone();
    nx.set(
        "_lsp_workspace_symbol",
        lua.create_function(move |_, query: String| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::WorkspaceSymbol { query });
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
                  (id, lines, title, border, relative): (
                u64,
                Vec<Table>,
                Option<String>,
                String,
                String,
            )| {
                // Each `lines` entry is a chunk list (`{ {text, hl}, … }`) — the Lua
                // wrapper normalized plain-string lines to a single unstyled chunk —
                // so a styled `nx.ui.float` caller (which-key) crosses the same path
                // as extmark virt_text.
                let lines = lines
                    .iter()
                    .map(virt_chunks_from_table)
                    .collect::<mlua::Result<Vec<_>>>()?;
                sh.borrow_mut().ui_floats.push(UiFloatReq {
                    id,
                    close: false,
                    lines,
                    title,
                    border,
                    relative,
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
                relative: String::new(),
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
        lua.create_function(move |_, args: PickerOpenArgs| {
            let (
                dynamic,
                width,
                height,
                align,
                margin,
                prompt_bottom,
                preview,
                query,
                title,
                multiselect,
            ) = args;
            sh.borrow_mut().picker_opens.push(PickerOpenReq {
                dynamic,
                width: width.unwrap_or_default(),
                height: height.unwrap_or_default(),
                align: align.unwrap_or_default(),
                margin: margin
                    .filter(|m| m.len() == 4)
                    .map(|m| [m[0], m[1], m[2], m[3]])
                    .unwrap_or([0; 4]),
                prompt_bottom: prompt_bottom.unwrap_or(false),
                preview: preview.unwrap_or(false),
                query: query.unwrap_or_default(),
                title: title.filter(|t| !t.is_empty()),
                // Default-on: only an explicit `multiselect = false` disables marking.
                multiselect: multiselect.unwrap_or(true),
            });
            Ok(())
        })?,
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
    // `nx._cmdline_complete_setup(docs)`: enable the command-line completion engine
    // (the float-list widget's fifth orchestration). `docs` toggles the docs/params
    // preview pane (Phase 3). The candidate source itself is the Lua function
    // `nx._cmdline_complete_run`, which the server calls synchronously per `<Tab>`.
    let sh = shared.clone();
    nx.set(
        "_cmdline_complete_setup",
        lua.create_function(move |_, docs: bool| {
            sh.borrow_mut().cmdline_complete_setups.push(docs);
            Ok(())
        })?,
    )?;
    // `nx._cmdline_set_arg(path)`: the file-picker confirm pasting `path` into the
    // open command line's argument token (the picker was launched over a still-open
    // `:e <Tab>`; this fills the line without running it). The server drains it into
    // `Editor::cmdline_replace_arg`.
    let sh = shared.clone();
    nx.set(
        "_cmdline_set_arg",
        lua.create_function(move |_, path: String| {
            sh.borrow_mut().cmdline_set_args.push(path);
            Ok(())
        })?,
    )?;
    // `nx._options_catalog`: the documented option catalog (an array of
    // `{ name, abbrev, kind, scope, doc }` tables). Populated by the server at
    // startup from core's single source of truth
    // (`nxvim_core::options::options_catalog()`) via
    // [`crate::LuaRuntime::set_options_catalog`] — nxvim-lua stays decoupled from
    // core, so the data is injected rather than read directly. The bundled
    // `nx.cmdline_complete` source reads it to offer option names (with their docs)
    // after `:set`, so the completion list can never drift from the names `:set`
    // actually accepts. An empty table until the server populates it.
    nx.set("_options_catalog", lua.create_table()?)?;

    // `nx._statusline_setup(win, kind, left, right, separator)`: queue a
    // `nx.statusline.setup{}` / `reset()` request ([`StatuslineSetupReq`]). `win` is
    // `nil` for the global layout or a window id for a window-local override; `kind`
    // is `"segments"` (with the validated `left` / `right` name lists), `"format"`
    // (use the `%`-format), or `"inherit"` (drop the override). `separator` is the
    // connector between/around segments (`nil` ⇒ a single space; `""` to disable it
    // for a powerline statusline). The Lua wrapper validated every segment name
    // against the built-ins / registered segments.
    let sh = shared.clone();
    nx.set(
        "_statusline_setup",
        lua.create_function(
            move |_,
                  (win, kind, left, right, separator): (
                Option<u64>,
                String,
                Vec<String>,
                Vec<String>,
                Option<String>,
            )| {
                let target = match win {
                    Some(w) => StatuslineTarget::Window(w),
                    None => StatuslineTarget::Global,
                };
                let kind = match kind.as_str() {
                    "format" => StatuslineKind::Format,
                    "inherit" => StatuslineKind::Inherit,
                    // The connector defaults to a single space (the plain-bar look);
                    // a powerline statusline passes `""` to disable it.
                    _ => StatuslineKind::Segments {
                        left,
                        right,
                        separator: separator.unwrap_or_else(|| " ".to_string()),
                    },
                };
                sh.borrow_mut()
                    .statusline_setups
                    .push(StatuslineSetupReq { target, kind });
                Ok(())
            },
        )?,
    )?;
    // `nx._statusline_publish(win, name, texts, groups, clicks)`: queue a custom
    // segment's resolved cells for one window ([`StatuslinePublishReq`]) — parallel
    // `texts` / `groups` / `clicks` arrays (an empty group ⇒ the base `StatusLine`
    // highlight; an empty click ⇒ a non-clickable cell, else a `v:lua.…` handler
    // reference). The server caches them by `(win, name)` and paints them until the
    // next re-render.
    let sh = shared.clone();
    nx.set(
        "_statusline_publish",
        lua.create_function(
            move |_,
                  (win, name, texts, groups, clicks): (
                u64,
                String,
                Vec<String>,
                Vec<String>,
                Vec<String>,
            )| {
                let cells = texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let group = groups.get(i).filter(|g| !g.is_empty()).cloned();
                        let on_click = clicks.get(i).filter(|c| !c.is_empty()).cloned();
                        (text, group, on_click)
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
    // `nx._view_action(name)`: a view buffer-local keymap fired the named action
    // (`confirm`) on the focused `nx.view` buffer; the server applies it via
    // `Editor::apply_view_action`. (Navigation is ordinary normal-mode motion now.)
    let sh = shared.clone();
    nx.set(
        "_view_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().view_actions.push(name);
            Ok(())
        })?,
    )?;
    // `nx._qf_action(name)`: a `FileType qf` buffer-local keymap fired the named
    // action (`jump`) on the focused quickfix / location-list display buffer; the
    // server applies it via `Editor::apply_qf_action`. This carries vim's
    // buffer-local quickfix `<CR>` (formerly a hard-coded `input()` branch).
    let sh = shared.clone();
    nx.set(
        "_qf_action",
        lua.create_function(move |_, name: String| {
            sh.borrow_mut().qf_actions.push(name);
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
                    send: false,
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
                    send: false,
                });
                Ok(())
            },
        )?,
    )?;

    // `nx._list_send(items, title, action, to_qf)`: queue a [`QfSetOp`] routed through
    // [`Editor::list_send`] — the "send/add these results to a list" actions behind
    // `nx.qf.{send,add}_to_{loc,qf}list` (the picker's quickfix-style sinks). Honors
    // `'qfdock'`: a dock tab (nxvim) or a split (vim/telescope). `action` is `" "`
    // (send / new) or `"a"` (add / append); `to_qf` targets the global quickfix list
    // (encoded as `loclist_win = None`) vs a location list (`Some(0)`). `items` is an
    // array of entry dicts.
    let sh = shared.clone();
    nx.set(
        "_list_send",
        lua.create_function(
            move |_,
                  (items, title, action, to_qf): (
                mlua::Table,
                Option<String>,
                Option<String>,
                bool,
            )| {
                let mut out = Vec::with_capacity(items.raw_len());
                for pair in items.sequence_values::<mlua::Table>() {
                    out.push(qf_item_from_table(&pair?)?);
                }
                let action = action.and_then(|a| a.chars().next()).unwrap_or(' ');
                sh.borrow_mut().qf_ops.push(QfSetOp {
                    items: Some(out),
                    lines: None,
                    efm: None,
                    action,
                    title,
                    open: false,
                    goto_first: false,
                    loclist_win: if to_qf { None } else { Some(0) },
                    send: true,
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
    // `:match_str(text)` -> (start, end) byte offsets or nil. Same vim-magic
    // dialect as `vim.fn.substitute`; an invalid pattern raises.
    vim.set(
        "regex",
        lua.create_function(|_, pat: String| {
            Ok(LuaRegex {
                re: vimregex::compile(&pat).map_err(mlua::Error::RuntimeError)?,
            })
        })?,
    )?;

    // `nx._regex(pat, opts?)`: compile a pattern into the regex object backing
    // `nx.regex` (the documented Lua wrapper in the prelude). A compiled
    // `NxRegex` userdata exposing `:find`/`:match`/`:gmatch`/`:gsub`/`:test`.
    // Defaults to the Rust `regex` crate (`engine = "pcre"`); `engine = "vim"` /
    // `plain` select the other engines.
    nx.set(
        "_regex",
        lua.create_function(|_, (pat, opts): (String, Option<Table>)| {
            crate::search::NxRegex::compile(&pat, opts.as_ref())
        })?,
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

    // `nx._json_encode(value[, opts])`: serialize a Lua value to a JSON string, using
    // the same array-vs-object rule as [`lua_to_rmpv`]. `opts.pretty` (default false)
    // emits a 2-space-indented, multi-line document instead of the compact one-liner —
    // for human-readable / diff-friendly files, so a plugin needn't hand-roll its own
    // pretty printer. Backs `nx.json.encode` / `vim.json.encode`.
    nx.set(
        "_json_encode",
        lua.create_function(|_, (value, opts): (mlua::Value, Option<mlua::Table>)| {
            let pretty = match &opts {
                Some(t) => t.get::<Option<bool>>("pretty")?.unwrap_or(false),
                None => false,
            };
            let json = lua_to_json(&value)?;
            let out = if pretty {
                serde_json::to_string_pretty(&json)
            } else {
                serde_json::to_string(&json)
            };
            out.map_err(mlua::Error::external)
        })?,
    )?;

    // ----- additional vim.fn (process / PATH) ---------------------------------
    // `vim.fn.getpid()`: this (editor) process's id.
    func.set(
        "getpid",
        lua.create_function(|_, ()| Ok(std::process::id() as i64))?,
    )?;

    Ok(())
}

// ===== nx.fs helpers ========================================================
// The `nx._fs_op` bridge above parses its job table into a typed [`FsJob`] here, and
// the runtime marshals the op's [`FsValue`] result back through `fs_stat_table`. The
// op itself runs OFF the tick (the event-loop actor / daemon leg) via
// [`crate::run_fs_job`] — the per-op semantics live there now, not inline.

/// Parse a `nx._fs_op` job table `{ op = "<name>", … }` into a typed [`FsJob`].
/// An unknown `op`, or a field the op requires that is missing / the wrong type,
/// fails loud here (the scripting boundary) rather than queuing a malformed op.
fn fs_job_from_table(job: &Table) -> mlua::Result<FsJob> {
    let op: String = job.get("op")?;
    let bytes = |key: &str| -> mlua::Result<Vec<u8>> {
        let s: mlua::String = job.get(key)?;
        Ok(s.as_bytes().to_vec())
    };
    let recursive =
        || -> mlua::Result<bool> { Ok(job.get::<Option<bool>>("recursive")?.unwrap_or(false)) };
    // `mode` is the Unix permission bits for `mkdir`; default 0o755 (matching
    // neovim's `mkdir` default) when the caller omits it.
    let mode = || -> mlua::Result<u32> { Ok(job.get::<Option<u32>>("mode")?.unwrap_or(0o755)) };
    Ok(match op.as_str() {
        "stat" => FsJob::Stat {
            path: job.get("path")?,
        },
        "lstat" => FsJob::Lstat {
            path: job.get("path")?,
        },
        "exists" => FsJob::Exists {
            path: job.get("path")?,
        },
        "readdir" => FsJob::Readdir {
            path: job.get("path")?,
        },
        "read" => FsJob::Read {
            path: job.get("path")?,
        },
        "read_text" => FsJob::ReadText {
            path: job.get("path")?,
            encoding: job
                .get::<Option<String>>("encoding")?
                .unwrap_or_else(|| "utf-8".to_string()),
        },
        "write" => FsJob::Write {
            path: job.get("path")?,
            data: bytes("data")?,
        },
        "append" => FsJob::Append {
            path: job.get("path")?,
            data: bytes("data")?,
        },
        "mkdir" => FsJob::Mkdir {
            path: job.get("path")?,
            recursive: recursive()?,
            mode: mode()?,
        },
        "rename" => FsJob::Rename {
            from: job.get("from")?,
            to: job.get("to")?,
        },
        "remove" => FsJob::Remove {
            path: job.get("path")?,
            recursive: recursive()?,
        },
        "copy" => FsJob::Copy {
            src: job.get("src")?,
            dst: job.get("dst")?,
            recursive: recursive()?,
        },
        "realpath" => FsJob::Realpath {
            path: job.get("path")?,
        },
        other => {
            return Err(mlua::Error::runtime(format!(
                "nx._fs_op: unknown fs op '{other}'"
            )))
        }
    })
}

/// Project a [`crate::LuaStat`] into the Lua table `nx.fs.stat` resolves with.
/// Times are fractional seconds since the epoch; `ino`/`uid`/… are omitted (not
/// what a file tree needs, and `0` off unix). No `ctime` field — `LuaStat` carries
/// only modify/access times. Called from [`crate::runtime`] when marshalling an
/// [`FsValue::Stat`](crate::FsValue) back to Lua.
pub(crate) fn fs_stat_table(lua: &Lua, st: &crate::LuaStat) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("type", st.kind.as_str())?;
    t.set("size", st.size)?;
    t.set("mode", st.mode)?;
    if let Some((s, n)) = st.mtime {
        t.set("mtime", s as f64 + f64::from(n) / 1e9)?;
    }
    if let Some((s, n)) = st.atime {
        t.set("atime", s as f64 + f64::from(n) / 1e9)?;
    }
    Ok(t)
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
