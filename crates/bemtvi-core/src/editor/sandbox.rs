//! The editor's leg of the bounded compute sandbox — see [`crate::sandbox`].

use super::BufferId;
use super::Editor;
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
