//! The editor's leg of the bounded compute sandbox — see [`crate::sandbox`].

use super::command::{PendingCommand, Stage};
use super::{BufferId, CmdlineKind, Editor, ExprTarget, RegKind};
use crate::mode::Mode;
use crate::sandbox::{SandboxEngine, SandboxError, SandboxFn};

/// The prefix that marks a `:s` replacement as a sandbox **expression**
/// rather than a literal template — vim's spelling, kept because the muscle
/// memory is universal even though what follows is Lua, not Vimscript.
pub(crate) const SUBST_EXPR_PREFIX: &str = r"\=";

/// The expression source inside a `\=` replacement, or `None` for a literal
/// template.
pub(crate) fn subst_expr_src(rep: &str) -> Option<&str> {
    rep.strip_prefix(SUBST_EXPR_PREFIX)
}

impl Editor {
    /// Install the bounded compute sandbox. The server constructs the concrete
    /// `bemtvi-sandbox` VM at startup and hands it over; a bare-core test leaves
    /// it `None` and every sandbox-backed surface fails loud when used.
    ///
    /// Mirrors [`Editor::set_syntax_engine`].
    pub fn set_sandbox_engine(&mut self, engine: Box<dyn SandboxEngine>) {
        self.sandbox = Some(engine);
    }

    /// Compile `src` as a sandbox expression, or report why it could not be.
    ///
    /// [`SandboxError::Unavailable`] when no engine is installed — never silently
    /// treated as "no expression", since that would apply an empty replacement to
    /// every match.
    pub(crate) fn sandbox_compile(
        &mut self,
        src: &str,
        params: &[&str],
    ) -> Result<SandboxFn, SandboxError> {
        match self.sandbox.as_mut() {
            Some(e) => e.compile_expr(src, params),
            None => Err(SandboxError::Unavailable),
        }
    }

    /// Drop a compiled chunk once its command is done.
    pub(crate) fn sandbox_release(&mut self, f: SandboxFn) {
        if let Some(e) = self.sandbox.as_mut() {
            e.release(f);
        }
    }
}

impl Editor {
    /// Run `body` with the sandbox engine **detached** from `self`.
    ///
    /// A substitute loop has to borrow the editor (to read and rewrite lines) and
    /// the engine (to evaluate the expression) at the same time, which a single
    /// `&mut self` cannot give. Lifting the engine out for the duration hands the
    /// body two independent borrows, and restoring it here — rather than at each
    /// of the caller's exits — makes it impossible to lose the engine by
    /// returning early on an error.
    pub(crate) fn with_sandbox<R>(
        &mut self,
        body: impl FnOnce(&mut Self, &mut Option<Box<dyn SandboxEngine>>) -> R,
    ) -> R {
        let mut engine = self.sandbox.take();
        let out = body(self, &mut engine);
        self.sandbox = engine;
        out
    }
}

impl Editor {
    /// Install (or clear, with `None`) the picker **re-ranker**: a sandbox
    /// expression over `label`, `query` and `score` returning a new sort key,
    /// higher first. Compiled here rather than on first use, so a bad expression
    /// is reported when it is configured instead of silently at the next picker.
    pub fn set_picker_scorer(&mut self, src: Option<String>) {
        if let Some(old) = self.picker_scorer.take() {
            self.sandbox_release(old);
        }
        let Some(src) = src else { return };
        match self.sandbox_compile(&src, &["label", "query", "score"]) {
            Ok(h) => self.picker_scorer = Some(h),
            Err(err) => self.echo(format!("btv.picker.scorer: {err}")),
        }
    }
}

impl Editor {
    /// Install (or clear, with `None`) the completion **re-ranker**: a sandbox
    /// expression over `label`, `query`, `score` and `kind` returning a new sort
    /// key, higher first. Compiled here rather than on first use, so a bad
    /// expression is reported when it is configured instead of at the next popup.
    pub fn set_complete_scorer(&mut self, src: Option<String>) {
        if let Some(old) = self.complete_scorer.take() {
            self.sandbox_release(old);
        }
        let Some(src) = src else { return };
        match self.sandbox_compile(&src, &["label", "query", "score", "kind"]) {
            Ok(h) => self.complete_scorer = Some(h),
            Err(err) => self.echo(format!("btv.complete.scorer: {err}")),
        }
    }
}

/// How much of a buffer the content sniffer sees. Enough for a shebang, an XML
/// declaration, a mode line or a distinctive first construct — and bounded, so
/// opening a huge file never hands a huge string across the VM boundary.
const SNIFF_BYTES: usize = 2048;

impl Editor {
    /// Install (or clear, with `None`) the content-based filetype sniffer.
    pub fn set_filetype_detect(&mut self, src: Option<String>) {
        if let Some(old) = self.filetype_fn.take() {
            self.sandbox_release(old);
        }
        // A new sniffer gets to re-answer for buffers the old one saw.
        self.filetype_sniffed.clear();
        let Some(src) = src else { return };
        match self.sandbox_compile(&src, &["name", "ext", "head"]) {
            Ok(h) => self.filetype_fn = Some(h),
            Err(err) => self.echo(format!("btv.filetype.detect: {err}")),
        }
    }

    /// Install (or clear, with `None`) the `'indentexpr'`.
    pub fn set_indent_expr(&mut self, src: Option<String>) {
        if let Some(old) = self.indent_fn.take() {
            self.sandbox_release(old);
        }
        let Some(src) = src else { return };
        match self.sandbox_compile(&src, &["prev", "line", "lnum", "sw", "previndent"]) {
            Ok(h) => self.indent_fn = Some(h),
            Err(err) => self.echo(format!("btv.indent.expr: {err}")),
        }
    }

    /// Run the content sniffer over any buffer it has not answered for yet.
    ///
    /// Once per buffer, not once per frame: the verdict is stored as the buffer's
    /// explicit filetype, so `buffer_filetype` — which is `&self` and on hot paths
    /// — never has to reach the sandbox at all.
    pub fn settle_filetype_detect(&mut self) {
        let Some(handle) = self.filetype_fn else {
            return;
        };

        let mut wanted: Vec<(BufferId, String, String, Option<std::path::PathBuf>, String)> =
            Vec::new();
        for id in self.buffer_ids() {
            let Some(buf) = self.buffer_of(id) else {
                continue;
            };
            let path = buf.path.clone();
            // Keyed by the *path*, not just the buffer: `:e` reuses an empty
            // unnamed buffer, so a verdict taken before the file was opened
            // into it would otherwise stick to the wrong content forever.
            if self.filetype_sniffed.get(&id).is_some_and(|p| *p == path) {
                continue;
            }
            let name = path
                .as_deref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let ext = path
                .as_deref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            // Joined, not terminated: an empty buffer must read as `""` so an
            // expression can test for "nothing to go on".
            let mut head = String::new();
            for l in 0..buf.line_count().min(16) {
                if !head.is_empty() {
                    head.push('\n');
                }
                head.push_str(&buf.line_cow(l));
                if head.len() >= SNIFF_BYTES {
                    break;
                }
            }
            head.truncate(SNIFF_BYTES);
            wanted.push((id, name, ext, path, head));
        }
        if wanted.is_empty() {
            return;
        }

        let failure = self.with_sandbox(|ed, sb| {
            for (id, name, ext, path, head) in &wanted {
                let got = match sb.as_mut() {
                    Some(engine) => engine.call_filetype(handle, name, ext, head),
                    None => Err(SandboxError::Unavailable),
                };
                match got {
                    Ok(verdict) => {
                        ed.filetype_sniffed.insert(*id, path.clone());
                        if let Some(ft) = verdict {
                            ed.set_filetype(*id, &ft);
                        }
                    }
                    Err(err) => return Some(err),
                }
            }
            None
        });

        if let Some(err) = failure {
            self.echo(format!(
                "btv.filetype.detect: {err} — content detection disabled"
            ));
            if let Some(h) = self.filetype_fn.take() {
                self.sandbox_release(h);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The expression register (`"=` / `<C-r>=`)
// ---------------------------------------------------------------------------

impl Editor {
    /// Open the **expression register** prompt: a [`CmdlineKind::Expr`] line whose
    /// `<CR>` evaluates the typed Lua in the sandbox and delivers the result to
    /// `target` (`<Esc>` cancels, computing nothing). Mirrors
    /// [`Editor::enter_helix_regex`] — one [`Mode::Command`] line, a kind that
    /// routes the submit.
    ///
    /// Reached from `<C-r>=` in Insert ([`ExprTarget::Insert`]) and from `"=` at a
    /// command boundary ([`ExprTarget::Register`]).
    pub(crate) fn enter_expr_register(&mut self, target: ExprTarget) {
        // Opened from an already-open command line: suspend it (text, cursor, kind
        // and all) so the result can be spliced back into it.
        if matches!(target, ExprTarget::Cmdline) {
            let saved = self.save_cmdline_state();
            self.expr_cmdline_stack.push(saved);
        }
        // Come back to the mode `"=` / `<C-r>=` was typed in: the insert flavour
        // for a computed insert, and Visual for `"=…<CR>p` over a selection (whose
        // selection stays painted while the line is open, as a `/` search's does).
        self.cmdline_return_mode = self.mode;
        self.cmdline_from_visual = self.mode.is_visual().then_some(self.mode);
        // The count and register of the command being built (`3"=…<CR>p`) — the
        // command line resets the parse state, so `submit_expr_register` puts them
        // back for the `p` that follows.
        // Stage::Start, not the `RegisterPending` the `"` left behind: restoring
        // that stage would make the *next* key read as a register name, silently
        // swallowing the `p`.
        self.expr_saved_pending = matches!(target, ExprTarget::Register).then(|| PendingCommand {
            register: Some('='),
            stage: Stage::Start,
            ..self.pending.clone()
        });
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Expr(target);
        // See `enter_command`: never inherit a stale `<C-r>` register-read — the
        // `=` that opened an insert-mode prompt was itself one.
        self.awaiting_register = false;
        self.hist_idx = None;
        self.message.clear();
        self.reset_pending();
    }

    /// Record a submitted expression in the `@=` history ring, skipping an empty
    /// line or a consecutive duplicate (mirroring [`Editor::remember_ex`]).
    pub(crate) fn remember_expr(&mut self, src: &str) {
        if src.is_empty() || self.expr_history.last().is_some_and(|last| last == src) {
            return;
        }
        self.expr_history.push(src.to_string());
    }

    /// Evaluate a submitted expression-register line and deliver the result.
    ///
    /// Evaluated **once, here** — not re-evaluated per read the way vim's `@=` is
    /// — because [`Editor::register_text`] is `&self` (it sits on the paste path)
    /// while the engine needs `&mut`. The consequence is stated in the plan doc:
    /// `"=lnum<CR>p` then `j.` pastes the first line number again, and in exchange
    /// the result is a stored, introspectable register value.
    ///
    /// Every failure — compile, runtime, deadline, a non-text result, no engine —
    /// echoes and delivers nothing, leaving the `=` register as it was.
    pub(crate) fn submit_expr_register(&mut self, target: ExprTarget, src: &str) {
        let saved = self.expr_saved_pending.take();
        // An empty line computes nothing (vim beeps; the abort is the whole signal),
        // and a failing one delivers nothing — but a *suspended command line* has to
        // come back either way, so the result is threaded as an `Option` rather than
        // returned early.
        let result = if src.trim().is_empty() {
            None
        } else {
            match self.eval_expr_register(src) {
                Ok(text) => Some(text),
                Err(err) => {
                    self.echo(format!("E:{err}"));
                    None
                }
            }
        };
        if let Some(text) = &result {
            // Vim's rule for a register set from a value: text ending in a newline
            // pastes linewise, anything else charwise. This is what lets
            // `"=…<CR>p` paste whole lines.
            let kind = if text.ends_with('\n') {
                RegKind::Line
            } else {
                RegKind::Char
            };
            self.registers.set_api('=', text.clone(), kind, false);
        }
        let text = result.unwrap_or_default();
        match target {
            // Insert at every cursor through the same primitive `<C-r>{register}`
            // uses, so multi-cursor insertion and the `".` accumulator behave
            // identically. The insert session's undo snapshot is still open, so
            // this groups into the surrounding insert.
            ExprTarget::Insert => self.insert_text_session(&text),
            // Put the count + `"=` back so the *following* `p` sees the register.
            ExprTarget::Register => {
                if let Some(saved) = saved {
                    self.pending = saved;
                }
            }
            // Resume the suspended line with the result spliced in at its cursor —
            // and resume it even when nothing was computed, so a typo at the nested
            // prompt costs the expression, never the line being typed.
            ExprTarget::Cmdline => self.resume_expr_cmdline(&text),
        }
    }

    /// Resume the command line a nested `<C-r>=` prompt was opened over, splicing
    /// `insert` in at its cursor (empty when the prompt computed nothing). Shared
    /// by the submit and cancel paths.
    pub(crate) fn resume_expr_cmdline(&mut self, insert: &str) {
        if let Some(outer) = self.expr_cmdline_stack.pop() {
            self.restore_cmdline_state(outer, insert);
        }
    }

    /// Compile and call one expression-register line, with the cursor's own line,
    /// 1-based line number and 1-based column in scope.
    fn eval_expr_register(&mut self, src: &str) -> Result<String, SandboxError> {
        let handle = self.sandbox_compile(src, &["line", "lnum", "col"])?;
        let line = self.buffer().line_cow(self.cursor.line).to_string();
        let lnum = self.cursor.line as i64 + 1;
        let col = self.cursor.col as i64 + 1;
        let got = self.with_sandbox(|_ed, sb| match sb.as_mut() {
            Some(engine) => engine.call_eval(handle, &line, lnum, col),
            None => Err(SandboxError::Unavailable),
        });
        // One compile per submit, released immediately: the chunk is never reused
        // (a later `p` pastes the stored text, it does not re-evaluate).
        self.sandbox_release(handle);
        got
    }
}
