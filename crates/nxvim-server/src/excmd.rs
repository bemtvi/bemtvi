//! Ex-command resolution: the `:`-command entry point, the fallback resolver for
//! commands the core didn't recognize (LSP commands, user commands, colorscheme),
//! and runtime-file lookup.

use crate::lsp::LspReqKind;
use crate::EditHost;
use std::path::PathBuf;

impl EditHost {
    pub(crate) fn run_command(&mut self, cmd: &str) {
        self.editor.command(cmd);
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Resolve an ex-command the core didn't recognize: load a colorscheme,
    /// dispatch a Lua user command if one is registered under that name, or
    /// report the standard unknown-command error. `cmd` is the trimmed line.
    pub(crate) fn resolve_command(&mut self, cmd: &str) {
        let name = cmd.split_whitespace().next().unwrap_or("");
        let args = cmd.get(name.len()..).unwrap_or("").trim_start();
        // A trailing `!` is the command's bang, not part of its name; split it off
        // so the abbreviation match below sees the bare name (`au!` → `au`).
        let (base, bang) = match name.strip_suffix('!') {
            Some(b) => (b, true),
            None => (name, false),
        };
        // The editor's authoritative current buffer, for resolving a buffer-local
        // user command (it shadows a global of the same name, and is unknown from
        // any other buffer).
        let cur_buf = self.editor.current_buffer_id().0;
        match name {
            "colorscheme" | "colo" => self.set_colorscheme(args.trim()),
            // `:so[urce] {file}` — run a script file (`.lua` executes through the
            // runtime; vimscript is fail-loud, not a silent skip).
            "so" | "sou" | "sour" | "sourc" | "source" => self.ex_source(args.trim()),
            // Phase-1 LSP observability: dump server/document state into the panel.
            "LspInfo" => {
                let lines = self.lsp_info_lines();
                self.editor.open_panel("LSP info", lines, false, 0);
            }
            // Phase-2: list the current buffer's diagnostics as a navigable
            // location list; `<CR>` on a row jumps to it (handled in the core).
            "LspDiagnostics" => match self.diagnostics_location_list() {
                Some((lines, targets)) => {
                    self.editor.open_panel("LSP diagnostics", lines, false, 0);
                    self.editor.set_panel_targets(targets);
                }
                None => self.editor.echo("No diagnostics"),
            },
            // Phase-3: go-to / references as ex-commands (the keymap-free path;
            // the reply jumps the cursor or opens a panel location list).
            "LspDefinition" => self.request_lsp(LspReqKind::Definition),
            "LspDeclaration" => self.request_lsp(LspReqKind::Declaration),
            "LspTypeDefinition" => self.request_lsp(LspReqKind::TypeDefinition),
            "LspImplementation" => self.request_lsp(LspReqKind::Implementation),
            "LspReferences" => self.request_lsp(LspReqKind::References),
            // Phase-4: hover docs into the panel, signature help on the message
            // line (the keymap-free path for `K` / `<C-k>`).
            "LspHover" => self.request_lsp(LspReqKind::Hover),
            "LspSignatureHelp" => self.request_lsp(LspReqKind::SignatureHelp),
            // Phase-6: buffer-mutating features. Format/code-action take no
            // argument; rename reads the new name the dispatcher split off — or,
            // with no name, prompts for it through `vim.lsp.buf.rename()`
            // (`vim.ui.input`, Phase 8) instead of erroring.
            "LspFormat" => self.request_lsp_format(),
            "LspRename" if args.trim().is_empty() => {
                if let Err(e) = self.lua.exec("vim.lsp.buf.rename()") {
                    self.editor
                        .echo(format!("E5108: Error in :LspRename prompt: {e}"));
                }
                self.apply_lua_effects();
            }
            "LspRename" => self.request_lsp_rename(args),
            "LspCodeAction" => self.request_lsp_code_action(),
            // `:au[tocmd]` / `:aug[roup]` / `:doau[tocmd]` (with abbreviations and
            // an optional `!`) drive the Lua autocmd registry. The core defers
            // them here; the prelude parses the argument line so the `:`-command
            // and the `nvim_*` API share one store.
            _ if is_autocmd(base) => match self.lua.ex_autocmd(bang, args) {
                Ok(out) => self.surface_autocmd_output("Autocommands", &out),
                Err(e) => self.editor.echo(format!("E5108: Error in :autocmd: {e}")),
            },
            _ if is_augroup(base) => match self.lua.ex_augroup(bang, args) {
                Ok(out) => self.surface_autocmd_output("Autocommands", &out),
                Err(e) => self.editor.echo(format!("E5108: Error in :augroup: {e}")),
            },
            // `:sil[ent][!] {cmd}` — the silent command modifier. Run `{cmd}` to
            // full convergence with its message-line output suppressed; the `!`
            // form is the one plugins lean on to ignore a command that errors or
            // doesn't exist (lazy.nvim: `silent! runtime plugin/rplugin.vim`). The
            // core defers the whole `silent …` string here, so this is the one
            // place it resolves. We snapshot the message line + history, run the
            // inner command (it may defer further — `run_command` drains that), then
            // restore: anything it echoed is dropped. nxvim doesn't yet distinguish
            // error- from normal-level output, so a bare `:silent` suppresses errors
            // too — a minor over-suppression versus neovim, where `:silent` keeps
            // errors and only `:silent!` swallows them.
            _ if is_silent(base) => {
                let inner = args.trim().to_string();
                if !inner.is_empty() {
                    let saved_msg = self.editor.message.clone();
                    let saved_len = self.editor.messages.len();
                    self.run_command(&inner);
                    self.editor.message = saved_msg;
                    self.editor.messages.truncate(saved_len);
                }
            }
            _ if is_doautocmd(base) => {
                match self.lua.ex_doautocmd(args) {
                    Ok(out) => self.surface_autocmd_output("Autocommands", &out),
                    Err(e) => self.editor.echo(format!("E5108: Error in :doautocmd: {e}")),
                }
                // A fired autocmd may have queued `vim.cmd(...)` / callbacks.
                self.apply_lua_effects();
            }
            // `:com[mand][!] [attrs] {Name} {repl}` — define a user command. The
            // prelude parses the attribute/name/replacement line and registers a
            // command whose `{repl}` runs as an ex-command on invocation (reusing
            // the same `vim._user_commands` store the `nvim_*` API uses), so the
            // many vimscript plugins that define their commands this way load. A
            // bare `:command` lists the defined commands (multi-line → panel).
            _ if is_command_def(base) => match self.lua.ex_command(bang, args, cur_buf) {
                Ok(out) if out.is_empty() => {}
                Ok(out) if out.contains('\n') => {
                    let lines = out.lines().map(str::to_string).collect();
                    self.editor.open_panel("User commands", lines, false, 0);
                }
                Ok(out) => self.editor.echo(out),
                Err(e) => self.editor.echo(format!("E5108: Error in :command: {e}")),
            },
            // `:TSInstall <lang>…` / `:TSUpdate <lang>…` — fetch + compile a
            // treesitter grammar into the data dir (see `nxvim_ts::install`). The
            // guard defers to a real nvim-treesitter plugin: if the user loaded one
            // and it registered `:TSInstall`, this arm is skipped and the
            // user-command arm below runs the plugin's instead (no silent shadow).
            "TSInstall" | "TSUpdate" if !self.lua.has_user_command(name, cur_buf) => {
                self.ts_install(args)
            }
            // `:TSInstallInfo` — list the parsers installed across the search path.
            // Same defer-to-plugin guard as the install commands.
            "TSInstallInfo" if !self.lua.has_user_command(name, cur_buf) => self.ts_install_info(),
            _ if self.lua.has_user_command(name, cur_buf) => {
                if let Err(e) = self.lua.run_user_command(name, args, cur_buf) {
                    self.editor
                        .echo(format!("E5108: Error executing command {name}: {e}"));
                }
                self.apply_lua_effects();
            }
            _ => self
                .editor
                .echo(format!("E492: Not an editor command: {name}")),
        }
    }

    /// `:TSInstall <lang>…` — fetch + compile each named grammar into the data dir
    /// off the editor thread. The work (network + a C compile) can take seconds, so
    /// each language runs on a `spawn_blocking` worker; its result returns on the
    /// `install_events` `select!` arm ([`EditHost::on_install_done`]). We echo a
    /// "installing…" line now so the user sees the command took.
    fn ts_install(&mut self, args: &str) {
        let langs: Vec<String> = args.split_whitespace().map(str::to_string).collect();
        if langs.is_empty() {
            self.editor
                .echo("TSInstall: usage: :TSInstall <language> [<language>…]");
            return;
        }
        self.editor
            .echo(format!("TSInstall: installing {}…", langs.join(", ")));
        for lang in langs {
            // Off-tick fetch+compile through the effect seam; the outcome returns on the
            // run loop's install arm ([`EditHost::on_install_done`]).
            self.fx.ts_install(lang);
        }
    }

    /// `:TSInstallInfo` — open a panel listing every parser installed across the
    /// data-dir search path (nxvim's own dir + a borrowed neovim `site/`), each
    /// with the queries it ships and the root it resolves from. Installed parsers
    /// only: the full installable catalog lives behind a network fetch we don't do
    /// for a read-only info command.
    fn ts_install_info(&mut self) {
        let parsers = nxvim_ts::installed_parsers();
        let mut lines = Vec::new();
        if parsers.is_empty() {
            lines.push("No treesitter parsers installed.".to_string());
            lines.push(String::new());
            lines.push("Install one with  :TSInstall <language>".to_string());
        } else {
            lines.push(format!("Installed treesitter parsers ({}):", parsers.len()));
            lines.push(String::new());
            for p in &parsers {
                let queries = if p.queries.is_empty() {
                    "(no queries)".to_string()
                } else {
                    p.queries.join(", ")
                };
                lines.push(format!("{:<14} {}", p.lang, queries));
                lines.push(format!("{:<14} {}", "", p.root.display()));
            }
        }
        self.editor.open_panel("TSInstall info", lines, false, 0);
    }

    /// Apply a finished `:TSInstall` job: on success, reload the grammar so every
    /// open buffer of that language re-highlights / re-indents against the new
    /// parser without a manual `:e`; on failure, echo the (loud) reason.
    pub(crate) fn on_install_done(&mut self, outcome: crate::InstallOutcome) {
        let (lang, result) = outcome;
        match result {
            Ok(report) => {
                self.editor.reload_ts_language(&report.lang);
                // `reload_ts_language` re-opens open buffers in the *engine*, but the
                // server's own highlight memo is keyed on (changedtick, viewport) —
                // neither changes on install. Drop it (like `TsOp::SetQuery` does) so
                // the next redraw re-queries the engine; otherwise a buffer opened
                // before the grammar existed stays blank until the next edit/scroll.
                self.syntax_states.clear();
                let short = &report.revision[..report.revision.len().min(9)];
                let queries = if report.queries.is_empty() {
                    "no queries".to_string()
                } else {
                    report.queries.join(", ")
                };
                self.editor.echo(format!(
                    "TSInstall: installed {} @ {short} [{}] (queries: {queries})",
                    report.lang, report.compiler
                ));
            }
            Err(e) => self.editor.echo(format!("TSInstall: {lang} failed: {e:#}")),
        }
    }

    /// Load a colorscheme by name: source `colors/<name>.lua` off the
    /// runtimepath (whose body populates the highlight registry via
    /// `nvim_set_hl`), record `g:colors_name`, and fire the `ColorScheme`
    /// autocmd. With no name, report the active colorscheme. The drain happens
    /// in the caller's `run_pending` fixpoint loop, so any `vim.cmd(...)` the
    /// theme queues is still resolved.
    pub(crate) fn set_colorscheme(&mut self, name: &str) {
        if name.is_empty() {
            return; // `:colorscheme` with no arg is a query we don't surface yet
        }
        let Some(path) = self.find_runtime_file(&format!("colors/{name}.lua")) else {
            self.editor
                .echo(format!("E185: Cannot find color scheme '{name}'"));
            return;
        };
        let src = match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                self.editor
                    .echo(format!("E185: Cannot read color scheme '{name}': {e}"));
                return;
            }
        };
        if let Err(e) = self.lua.exec(&src) {
            self.editor
                .echo(format!("E5108: Error loading colorscheme {name}: {e}"));
        }
        self.apply_lua_effects();
        let _ = self.lua.set_global_var("colors_name", name);
        if let Err(e) = self.lua.fire_autocmd("ColorScheme", name) {
            self.editor
                .echo(format!("E5108: Error in ColorScheme autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// `:source {file}` — run a script file. A `.lua` file executes through the
    /// runtime (its effects drain like a `:lua` chunk); a missing file is the
    /// standard `E484`. Vimscript (`.vim`) has no interpreter yet, so it fails loud
    /// rather than quietly skipping — a silent skip would make a colorscheme that
    /// never applied look loaded (the no-silent-stubs rule).
    fn ex_source(&mut self, arg: &str) {
        if arg.is_empty() {
            self.editor.echo("E471: Argument required");
            return;
        }
        let path = PathBuf::from(arg);
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => {
                self.editor.echo(format!("E484: Can't open file {arg}"));
                return;
            }
        };
        match path.extension().and_then(|e| e.to_str()) {
            Some("lua") => {
                if let Err(e) = self.lua.exec_named(&src, &format!("@{arg}")) {
                    self.editor
                        .echo(format!("E5113: Error while sourcing {arg}: {e}"));
                }
                self.apply_lua_effects();
            }
            _ => self.editor.echo(format!(
                "nxvim: :source of vimscript is not supported yet ({arg}); only .lua files can be sourced"
            )),
        }
    }

    /// Find a runtime file (e.g. `colors/catppuccin.lua`) by searching each
    /// runtimepath entry in order; the first existing match wins. `None` if no
    /// entry holds it.
    pub(crate) fn find_runtime_file(&self, relative: &str) -> Option<PathBuf> {
        self.lua.runtimepath().iter().find_map(|rt| {
            let candidate = rt.join(relative);
            candidate.is_file().then_some(candidate)
        })
    }

    /// Surface the text a `vim._ex_*` autocmd driver returned: empty is nothing,
    /// a multi-line listing opens a panel (like `:LspInfo`), and a single line is
    /// echoed (a message or an `E…` error).
    fn surface_autocmd_output(&mut self, title: &str, out: &str) {
        if out.is_empty() {
            return;
        }
        if out.contains('\n') {
            let lines = out.lines().map(str::to_string).collect();
            self.editor.open_panel(title, lines, false, 0);
        } else {
            self.editor.echo(out);
        }
    }
}

/// `:sil[ent]` and its abbreviations (`sil`, `sile`, `silen`, `silent`). The
/// minimal form is `sil` — shorter prefixes are ambiguous with other commands.
fn is_silent(base: &str) -> bool {
    matches!(base, "sil" | "sile" | "silen" | "silent")
}

/// `:au[tocmd]` and its abbreviations (`au`, `aut`, … `autocmd`). The minimal
/// form is `au` — `aug…` is `:augroup`, a different command.
fn is_autocmd(base: &str) -> bool {
    matches!(base, "au" | "aut" | "auto" | "autoc" | "autocm" | "autocmd")
}

/// `:aug[roup]` and its abbreviations (`aug`, `augr`, … `augroup`).
fn is_augroup(base: &str) -> bool {
    matches!(base, "aug" | "augr" | "augro" | "augrou" | "augroup")
}

/// `:doau[tocmd]` and its abbreviations (`doau`, `doaut`, … `doautocmd`).
fn is_doautocmd(base: &str) -> bool {
    matches!(
        base,
        "doau" | "doaut" | "doauto" | "doautoc" | "doautocm" | "doautocmd"
    )
}

/// `:com[mand]` and its abbreviations (`com`, `comm`, … `command`) — the
/// user-command *definition* command. The minimal form is `com`.
fn is_command_def(base: &str) -> bool {
    matches!(base, "com" | "comm" | "comma" | "comman" | "command")
}
