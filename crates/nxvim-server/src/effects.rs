//! Draining queued side effects to convergence: applying the Lua chunk's
//! highlights/commands/output/panel/LSP/loop/buffer ops, the Rust→Lua buffer
//! mirror, event-loop completions, and the `run_pending` fixpoint.

use crate::evloop::{LoopCommand, LoopEvent};
use crate::lsp::CODE_ACTION_PANEL_TITLE;
use crate::Server;
use crate::CURRENT_WIN;
use nxvim_core::highlight::HlDef;
use nxvim_core::{parse_color, BufferId};
use nxvim_lua::{BufOp, CallbackArgs, HlSet, LoopOp, PanelOp};
use rmpv::Value;
use std::collections::HashSet;

impl Server {
    /// Apply the side effects the last Lua chunk left in the runtime: highlight
    /// definitions fold into the core registry, queued ex-commands run against
    /// the editor, and the final captured `print` / `nvim_echo` line becomes the
    /// message.
    pub(crate) fn apply_lua_effects(&mut self) {
        for hl in self.lua.take_highlights() {
            self.editor.highlights.set(&hl.name, hl_def(&hl));
        }
        for cmd in self.lua.take_commands() {
            self.editor.command(&cmd);
        }
        // Each captured `print` / `nvim_echo` line becomes a message: the last
        // is shown on the message line, and every line lands in `:messages`.
        for line in self.lua.take_output() {
            self.editor.echo(line);
        }
        // Panel requests from `vim.panel.*` drive the core's panel state.
        for op in self.lua.take_panel_ops() {
            match op {
                PanelOp::Open {
                    title,
                    lines,
                    wants_select,
                    cursor,
                } => {
                    self.editor.open_panel(title, lines, wants_select, cursor);
                }
                PanelOp::SetLines(lines) => self.editor.set_panel_lines(lines),
                PanelOp::OnSelect(wants) => self.editor.set_panel_on_select(wants),
                PanelOp::SetCursor(line) => self.editor.set_panel_cursor(line),
                PanelOp::Close => self.editor.close_panel(),
            }
        }
        // Server-start requests from `vim.lsp.start` (the `vim.lsp.enable` FileType
        // dispatcher) bind a buffer to its language server and ensure it is spawned.
        for op in self.lua.take_lsp_ops() {
            self.apply_lsp_op(op);
        }
        // Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `vim.uv`
        // timers / async `vim.system`: a `Schedule` is serviced directly (queued
        // for the trailing `run_pending` drain); everything else is forwarded to
        // the background event-loop actor, whose completions arrive on the
        // `loop_events` `select!` arm.
        for op in self.lua.take_loop_ops() {
            self.apply_loop_op(op);
        }
        // Buffer mutations from `nvim_buf_set_lines` (Phase 6): applied to the live
        // editor after the chunk, so the rope catches up with the write-through the
        // Lua side already did against the `vim._bufs` mirror.
        for op in self.lua.take_buf_ops() {
            self.apply_buf_op(op);
        }
        // `vim.ui.input` prompts (Phase 8): open the editor's command line as a
        // labelled text prompt and remember which callback awaits the result. Only
        // one prompt can be open at a time (a single command line); if several were
        // queued, the last wins (its label/default is what shows) — a documented
        // single-prompt limitation, not a silent drop.
        for req in self.lua.take_ui_inputs() {
            self.editor.open_prompt(req.prompt, req.default);
            self.pending_ui_input = Some(req.cb_id);
        }
        // `vim.fn.confirm` button dialogs: open the command line as a single-key
        // confirm prompt and remember the callback that resumes the blocked
        // `vim.fn.confirm` coroutine. Shares the `pending_ui_input` slot and the
        // `prompt_results` channel with `vim.ui.input` (only one prompt is open at
        // a time); the chosen index arrives as a string the Lua side reads back.
        for req in self.lua.take_confirms() {
            self.editor
                .open_confirm(req.label, req.accelerators, req.default);
            self.pending_ui_input = Some(req.cb_id);
        }
    }

    /// Apply one [`BufOp`] to the live editor (Phase 6). Converts the neovim line
    /// range (0-based, `end`-exclusive, negatives from the end) to a byte range
    /// against the real buffer and replaces it as one undo step via
    /// [`Editor::apply_edits_to`], then flushes the buffer's pending LSP edits with
    /// [`Self::sync_lsp_buffer`] so a server attached to it sees the `didChange`
    /// (the must-not-omit step for a non-current buffer, which `sync_lsp` skips).
    pub(crate) fn apply_buf_op(&mut self, op: BufOp) {
        let BufOp::SetLines {
            bufnr,
            start,
            end,
            repl,
        } = op;
        let id = BufferId(bufnr);
        let Some(n) = self.editor.line_count_of(id) else {
            return; // unknown buffer — the Lua mirror guards this, but stay safe.
        };
        // Same normalization the Lua getter applies (kept in lockstep): negatives
        // count from the end, then clamp into [0, n]; `end` not below `start`.
        let norm = |i: i64| -> usize {
            let i = if i < 0 { n as i64 + i + 1 } else { i };
            i.clamp(0, n as i64) as usize
        };
        let start = norm(start);
        let end = norm(end).max(start);

        let buf = self
            .editor
            .buffer_of(id)
            .expect("line_count_of(id) was Some");
        let start_byte = buf.line_start(start);
        // Replacing through the last real line reaches the trailing phantom `\n`.
        // `line_start(n)` already equals `len_bytes()` for a real buffer (`n >= 1`),
        // but spell out the intent and guard the degenerate `n == 0` so the phantom
        // newline is never consumed.
        let end_byte = if end >= n && n > 0 {
            buf.len_bytes()
        } else {
            buf.line_start(end)
        };
        // Each replacement line needs its terminating `\n` (the removed span always
        // ends at a line boundary); `normalize()` re-adds the phantom trailing one.
        let repl_text = if repl.is_empty() {
            String::new()
        } else {
            let mut s = repl.join("\n");
            s.push('\n');
            s
        };
        self.editor
            .apply_edits_to(id, vec![(start_byte..end_byte, repl_text)]);
        self.sync_lsp_buffer(id);
    }

    /// Refresh the Rust→Lua buffer mirror (`vim._bufs` + `vim._cur_cursor` +
    /// current window) the buffer-read API resolves against (Phase 6). Pushed
    /// before any Lua entry that can read buffer/cursor state. The per-buffer line
    /// arrays are gated on `changedtick` — only a buffer that changed since its last
    /// mirror is re-serialized — so the common cursor-moved-no-edit path only
    /// refreshes the O(1) cursor/window fields.
    pub(crate) fn push_buf_mirror(&mut self) {
        let mut bufs: Vec<(u64, Option<Vec<String>>, String)> = Vec::new();
        for id in self.editor.buffer_ids() {
            let tick = self
                .editor
                .buffer_of(id)
                .map(|b| b.changedtick)
                .unwrap_or(0);
            let fresh = self.buf_mirror_ticks.get(&id) != Some(&tick);
            let lines = if fresh {
                self.buf_mirror_ticks.insert(id, tick);
                Some(self.editor.lines_of(id).unwrap_or_default())
            } else {
                None
            };
            let name = self.editor.buffer_name(id).unwrap_or_default();
            bufs.push((id.0, lines, name));
        }
        // Drop tick entries for buffers that no longer exist, so the map can't grow
        // unboundedly across a long session of opening and closing buffers.
        let live: HashSet<BufferId> = self.editor.buffer_ids().into_iter().collect();
        self.buf_mirror_ticks.retain(|id, _| live.contains(id));

        let cursor = (
            (self.editor.cursor.line + 1) as u64, // 1-based row, neovim convention
            self.editor.cursor.col as u64,        // 0-based column
        );
        let _ = self.lua.set_buf_mirror(&bufs, cursor, CURRENT_WIN);
    }

    /// Route one [`LoopOp`]: enqueue a `Schedule` for the `run_pending` drain, or
    /// forward a timer / process op to the event-loop actor (a fire-and-forget
    /// [`LoopCommand`], never awaited).
    pub(crate) fn apply_loop_op(&mut self, op: LoopOp) {
        match op {
            LoopOp::Schedule { id } => self.scheduled.push_back(id),
            LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            } => self.evloop.send(LoopCommand::TimerStart {
                id,
                delay: std::time::Duration::from_millis(delay_ms),
                repeat: std::time::Duration::from_millis(repeat_ms),
            }),
            LoopOp::TimerStop { id } => self.evloop.send(LoopCommand::TimerStop { id }),
            LoopOp::Spawn { id, cmd, cwd, env } => self.evloop.send(LoopCommand::Spawn {
                id,
                argv: cmd,
                cwd,
                env,
            }),
            LoopOp::Kill { id } => self.evloop.send(LoopCommand::Kill { id }),
        }
    }

    /// Handle one completion from the event-loop actor (a timer fired, a child
    /// reported its pid, or a child exited) by running its Lua callback on the
    /// server thread, then draining the effects it queued. The caller's
    /// `settle_events` drives the rest to convergence and repaints once per burst.
    pub(crate) fn on_loop_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::Timer { id, keep } => {
                if let Err(e) = self.lua.run_callback(id, keep, CallbackArgs::None) {
                    self.editor
                        .echo(format!("E5108: Error in timer callback: {e}"));
                }
                self.apply_lua_effects();
            }
            LoopEvent::ProcessSpawned { id, pid } => {
                // Record the real pid so the `vim.system` handle's `.pid` resolves
                // it (it can't be known synchronously on a single-threaded runtime).
                if let Err(e) = self.lua.set_process_pid(id, pid) {
                    self.editor
                        .echo(format!("E5108: Error recording process pid: {e}"));
                }
            }
            LoopEvent::ProcessExit {
                id,
                code,
                stdout,
                stderr,
            } => {
                let args = CallbackArgs::Process {
                    code,
                    stdout,
                    stderr,
                };
                if let Err(e) = self.lua.run_callback(id, false, args) {
                    self.editor
                        .echo(format!("E5108: Error in vim.system on_exit: {e}"));
                }
                self.apply_lua_effects();
            }
        }
    }

    /// The settle contract for an off-tick event arm: drive every queued effect to
    /// convergence (`run_pending`, which also drains `self.scheduled`) and repaint
    /// once. `dirty` forces a repaint even when no Lua callback ran (e.g. an LSP
    /// event that only updated cached state); a callback that queued work always
    /// repaints. Factored out so the syntax/LSP/loop arms share one tail and no
    /// off-tick callback's deferred `vim.cmd` is left undriven.
    pub(crate) fn settle_events(&mut self, dirty: bool) {
        let had_scheduled = !self.scheduled.is_empty();
        self.run_pending();
        if dirty || had_scheduled {
            self.redraw();
        }
    }

    /// Drive queued work to convergence: run the `:lua` chunks the editor
    /// queued, resolve every ex-command the core deferred (a Lua user command,
    /// else the unknown-command error), and repeat until nothing new is queued.
    /// Both queues feed each other — a user command can `vim.cmd(...)`, a `:lua`
    /// can define a command — so a single fixpoint loop covers them.
    pub(crate) fn run_pending(&mut self) {
        // Cap on fixpoint rounds before we conclude the queued work is
        // self-perpetuating — a command or `on_select` callback that re-queues
        // itself every round (e.g. a user command whose body re-runs the same
        // command). Without this the single-threaded server spins forever and
        // stops servicing input. Generous enough that any legitimate finite
        // chain converges first; mirrors neovim's `maxfuncdepth` recursion guard.
        const MAX_ROUNDS: usize = 100;
        let mut rounds = 0;
        // Refresh the buffer mirror before draining: everything that flows through
        // `run_pending` (user commands, scheduled / select callbacks, queued `:lua`)
        // can read buffer/cursor state. Intra-batch read-after-write stays
        // consistent via the `nvim_buf_set_lines` write-through, so once-at-entry is
        // enough (Phase 6).
        self.push_buf_mirror();
        loop {
            for chunk in std::mem::take(&mut self.editor.lua_queue) {
                // `exec_pumped` (not `exec`) so a `vim.fn.input` / `vim.fn.confirm`
                // in a `:lua` chunk can block on the command line and resume with
                // the answer instead of erroring "outside a coroutine".
                if let Err(e) = self.lua.exec_pumped(&chunk) {
                    self.editor.echo(format!("E5108: Error executing lua: {e}"));
                }
                self.apply_lua_effects();
            }
            for cmd in std::mem::take(&mut self.editor.deferred_commands) {
                self.resolve_command(&cmd);
            }
            // `<CR>` selections on a select-enabled panel: notify RPC clients and
            // fire the Lua `on_select` callback. The callback may itself queue
            // commands / lua / panel ops, so this is inside the fixpoint loop.
            for (index, line) in std::mem::take(&mut self.editor.panel_selects) {
                // The `:LspCodeAction` list (Phase 6) is a select-enabled panel:
                // a `<CR>` on row `index` applies that action's edit, keyed to the
                // currently-open code-action panel by title so a select on some
                // *other* select panel can't misroute here.
                if self.editor.panel_title() == Some(CODE_ACTION_PANEL_TITLE) {
                    self.apply_code_action(index);
                    continue;
                }
                // Navigable LSP location lists (diagnostics, references) jump in
                // the core itself when their target line is selected, so they
                // never reach here — only scripted/RPC select panels do.
                self.rpc.notify(
                    "nxvim_panel_select",
                    vec![Value::Map(vec![
                        (Value::from("index"), Value::from(index as u64 + 1)),
                        (Value::from("line"), Value::from(line.as_str())),
                    ])],
                );
                if let Err(e) = self.lua.run_panel_select(index, &line) {
                    self.editor
                        .echo(format!("E5108: Error in panel on_select: {e}"));
                }
                self.apply_lua_effects();
            }
            // `vim.ui.input` results (Phase 8): a submitted (`Some`) or cancelled
            // (`None`) prompt fires the waiting callback off the same tick. The
            // callback may itself open another prompt / queue lua, so this is
            // inside the fixpoint. The pending id is taken (one prompt at a time).
            for result in std::mem::take(&mut self.editor.prompt_results) {
                if let Some(id) = self.pending_ui_input.take() {
                    if let Err(e) = self.lua.run_ui_input(id, result) {
                        self.editor
                            .echo(format!("E5108: Error in vim.ui.input callback: {e}"));
                    }
                    self.apply_lua_effects();
                }
            }
            // Scheduled callbacks (`vim.schedule`) run after the work that queued
            // them converges, but still within this fixpoint — a scheduled fn may
            // itself `vim.schedule` / `vim.cmd`, which re-enters the loop. One
            // throwing callback is isolated (echoed as E5108) and never aborts the
            // drain or stops a later scheduled callback from running.
            for id in std::mem::take(&mut self.scheduled) {
                if let Err(e) = self.lua.run_callback(id, false, CallbackArgs::None) {
                    self.editor
                        .echo(format!("E5108: Error in scheduled callback: {e}"));
                }
                self.apply_lua_effects();
            }
            if self.editor.lua_queue.is_empty()
                && self.editor.deferred_commands.is_empty()
                && self.editor.panel_selects.is_empty()
                && self.editor.prompt_results.is_empty()
                && self.scheduled.is_empty()
            {
                break;
            }
            rounds += 1;
            if rounds >= MAX_ROUNDS {
                // Drop the still-growing work and report it, rather than loop
                // forever. The editor stays responsive to the next message.
                self.editor.lua_queue.clear();
                self.editor.deferred_commands.clear();
                self.editor.panel_selects.clear();
                self.editor.prompt_results.clear();
                self.scheduled.clear();
                self.editor
                    .echo("E132: command recursion limit exceeded".to_string());
                break;
            }
        }
    }
}

/// Translate a Lua-side `nvim_set_hl` definition into the core registry's
/// `HlDef`, parsing the color strings (`#rrggbb` / named / `NONE`) here at the
/// boundary so `nxvim-lua` need not know about the color type.
fn hl_def(hl: &HlSet) -> HlDef {
    let color = |c: &Option<String>| c.as_deref().and_then(parse_color);
    HlDef {
        fg: color(&hl.fg),
        bg: color(&hl.bg),
        sp: color(&hl.sp),
        bold: hl.bold,
        italic: hl.italic,
        underline: hl.underline,
        undercurl: hl.undercurl,
        strikethrough: hl.strikethrough,
        reverse: hl.reverse,
        link: hl.link.clone(),
    }
}
