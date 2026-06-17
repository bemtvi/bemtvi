//! Ex-command resolution: the `:`-command entry point, the fallback resolver for
//! commands the core didn't recognize (LSP commands, user commands, colorscheme),
//! and runtime-file lookup.

use crate::cwd::CdScope;
#[cfg(feature = "native")]
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
            // `:make[!] [args]` / `:grep[!] [args]` — run `'makeprg'` / `'grepprg'`
            // (with `$*` replaced by the args, else appended), capture the combined
            // output, parse it against `'errorformat'` / `'grepformat'`, open the
            // quickfix window if there are errors, and jump to the first one (a `!`
            // suppresses the jump). Async, via the job machinery (`nx._qf_make`).
            _ if matches!(base, "mak" | "make") => self.ex_make(args, bang, false, false),
            _ if matches!(base, "gr" | "gre" | "grep") => self.ex_make(args, bang, true, false),
            // The location-list twins (`:lmake[!]` / `:lgrep[!]`): identical, but the
            // parsed output populates the focused window's location list.
            _ if matches!(base, "lmak" | "lmake") => self.ex_make(args, bang, false, true),
            _ if matches!(base, "lgr" | "lgre" | "lgrep") => self.ex_make(args, bang, true, true),
            // `:so[urce] {file}` — run a script file (`.lua` executes through the
            // runtime; vimscript is fail-loud, not a silent skip).
            "so" | "sou" | "sour" | "sourc" | "source" => self.ex_source(args.trim()),
            // `:cd[!]` / `:chdir[!] [dir]` — change the **global** working directory.
            // `:tcd` / `:lcd` are the tab- and window-local variants (vim's scope
            // override order: window > tab > global). No argument goes to `$HOME`
            // (Unix `:cd`), `-` returns to that scope's previous directory, and
            // `~`/`~/…` expands to home; a relative path resolves against the current
            // cwd. The process cwd tracks the current window's effective dir (what
            // `vim.fn.getcwd` reads), so these mutate it directly.
            _ if matches!(base, "cd" | "chd" | "chdi" | "chdir") => {
                self.ex_chdir(CdScope::Global, args.trim())
            }
            _ if matches!(base, "tc" | "tcd" | "tch" | "tchd" | "tchdi" | "tchdir") => {
                self.ex_chdir(CdScope::Tabpage, args.trim())
            }
            _ if matches!(base, "lc" | "lcd" | "lch" | "lchd" | "lchdi" | "lchdir") => {
                self.ex_chdir(CdScope::Window, args.trim())
            }
            // `:pw[d]` — print the working directory on the message line.
            _ if matches!(base, "pw" | "pwd") => self.ex_pwd(),
            // Phase-1 LSP observability: dump server/document state into a listing.
            #[cfg(feature = "native")]
            "LspInfo" => {
                let lines = self.lsp_info_lines();
                self.editor.open_scratch_listing("[LSP info]", lines, 0);
            }
            // Phase-2: list the current buffer's diagnostics as a navigable
            // location list; `<CR>` on a row jumps to it (handled in the core).
            #[cfg(feature = "native")]
            "LspDiagnostics" => match self.diagnostics_location_list() {
                Some(entries) => self.editor.open_location_list(entries, "LSP diagnostics"),
                None => self.editor.echo("No diagnostics"),
            },
            // Phase-3: go-to / references as ex-commands (the keymap-free path;
            // the reply jumps the cursor or opens a panel location list).
            #[cfg(feature = "native")]
            "LspDefinition" => self.request_lsp(LspReqKind::Definition),
            #[cfg(feature = "native")]
            "LspDeclaration" => self.request_lsp(LspReqKind::Declaration),
            #[cfg(feature = "native")]
            "LspTypeDefinition" => self.request_lsp(LspReqKind::TypeDefinition),
            #[cfg(feature = "native")]
            "LspImplementation" => self.request_lsp(LspReqKind::Implementation),
            #[cfg(feature = "native")]
            "LspReferences" => self.request_lsp(LspReqKind::References),
            // Phase-4: hover docs into the panel, signature help on the message
            // line (the keymap-free path for `K` / `<C-k>`).
            #[cfg(feature = "native")]
            "LspHover" => self.request_lsp(LspReqKind::Hover),
            #[cfg(feature = "native")]
            "LspSignatureHelp" => self.request_lsp(LspReqKind::SignatureHelp),
            // Phase-6: buffer-mutating features. Format/code-action take no
            // argument; rename reads the new name the dispatcher split off — or,
            // with no name, prompts for it through `vim.lsp.buf.rename()`
            // (`vim.ui.input`, Phase 8) instead of erroring.
            #[cfg(feature = "native")]
            "LspFormat" => self.request_lsp_format(),
            #[cfg(feature = "native")]
            "LspRename" if args.trim().is_empty() => {
                if let Err(e) = self.lua.exec("vim.lsp.buf.rename()") {
                    self.editor
                        .echo(format!("E5108: Error in :LspRename prompt: {e}"));
                }
                self.apply_lua_effects();
            }
            #[cfg(feature = "native")]
            "LspRename" => self.request_lsp_rename(args),
            #[cfg(feature = "native")]
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
            // doesn't exist (e.g. a plugin manager: `silent! runtime plugin/rplugin.vim`). The
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
            // the same `nx._user_commands` store the `nvim_*` API uses), so the
            // many vimscript plugins that define their commands this way load. A
            // bare `:command` lists the defined commands (multi-line → panel).
            _ if is_command_def(base) => match self.lua.ex_command(bang, args, cur_buf) {
                Ok(out) if out.is_empty() => {}
                Ok(out) if out.contains('\n') => {
                    let lines = out.lines().map(str::to_string).collect();
                    self.editor
                        .open_scratch_listing("[User commands]", lines, 0);
                }
                Ok(out) => self.editor.echo(out),
                Err(e) => self.editor.echo(format!("E5108: Error in :command: {e}")),
            },
            // `:TSInstall <lang>…` / `:TSUpdate <lang>…` — fetch + compile a
            // treesitter grammar into the data dir (see `nxvim_ts::install`). The
            // guard defers to a real nvim-treesitter plugin: if the user loaded one
            // and it registered `:TSInstall`, this arm is skipped and the
            // user-command arm below runs the plugin's instead (no silent shadow).
            #[cfg(feature = "native")]
            "TSInstall" | "TSUpdate" if !self.lua.has_user_command(name, cur_buf) => {
                self.ts_install(args)
            }
            // The browser build can't compile/`dlopen` a native grammar, but it
            // highlights JS-side (web-tree-sitter), so `:TSInstall` fetches a *prebuilt*
            // grammar `.wasm` + queries at runtime through the `ts_install` effect — the
            // JS host does the fetch/cache/register (see edithost's `WasmEffects`). Same
            // defer-to-plugin guard: a real nvim-treesitter `:TSInstall` shadows this.
            #[cfg(not(feature = "native"))]
            "TSInstall" | "TSUpdate" if !self.lua.has_user_command(name, cur_buf) => {
                self.ts_install(args)
            }
            // `:TSInstallInfo` — list the parsers installed across the search path.
            // Same defer-to-plugin guard as the install commands.
            #[cfg(feature = "native")]
            "TSInstallInfo" if !self.lua.has_user_command(name, cur_buf) => self.ts_install_info(),
            // `:TSInstallInfo` on the browser build — list the grammars available to the
            // JS highlighter (the offline bundle + whatever `:TSInstall` cached in OPFS),
            // the wasm analogue of native's on-disk parser scan.
            #[cfg(not(feature = "native"))]
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

    /// `:cd` (global) / `:tcd` (tab-local) / `:lcd` (window-local) `[dir]` — change
    /// the working directory at `scope`. The process cwd tracks the current window's
    /// effective dir — `vim.fn.getcwd` reads it and every relative path resolves
    /// against it — so this mutates it directly and records the new dir at `scope` in
    /// [`DirState`]. No argument goes to `$HOME` (Unix `:cd` semantics), `-` returns
    /// to that scope's previous directory (E186 if there is none yet), and `~` / `~/…`
    /// expands to home; anything else resolves relative to the current cwd. On
    /// success `DirChanged` fires with the scope's pattern. A failure (missing /
    /// inaccessible directory) is reported, not swallowed.
    fn ex_chdir(&mut self, scope: CdScope, arg: &str) {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let target = match arg {
            "" => match home_dir() {
                Some(h) => h,
                None => {
                    self.editor
                        .echo("E5000: Cannot determine home directory ($HOME unset)");
                    return;
                }
            },
            "-" => match self.dirs.prev(scope, win, tab) {
                Some(p) => p.to_path_buf(),
                None => {
                    self.editor.echo("E186: No previous directory");
                    return;
                }
            },
            _ => expand_cd_arg(arg),
        };
        if let Err(e) = std::env::set_current_dir(&target) {
            self.editor.echo(format!(
                "E344: Can't change directory to \"{}\": {e}",
                target.display()
            ));
            return;
        }
        // Re-read the cwd so the stored / announced dir is the canonical absolute
        // path (relative `:cd ../x` and symlinked targets resolve here).
        let cwd = std::env::current_dir().unwrap_or(target);
        self.dirs.set(scope, win, tab, cwd.clone());
        // Announce the change so `DirChanged` handlers (project / session plugins) run.
        if let Err(e) = self
            .lua
            .fire_dir_changed(scope.pattern(), &cwd.display().to_string())
        {
            self.editor
                .echo(format!("E5108: Error in DirChanged autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// `:pwd` — print the working directory (the current window's effective dir, i.e.
    /// the process cwd) on the message line.
    fn ex_pwd(&mut self) {
        match std::env::current_dir() {
            Ok(p) => self.editor.echo(p.display().to_string()),
            Err(e) => self.editor.echo(format!("E187: Unknown directory: {e}")),
        }
    }

    /// `:make[!]` / `:grep[!]` — run `'makeprg'` / `'grepprg'` and route the output
    /// into the quickfix list. `'makeprg'` (or `'grepprg'`) is the shell command;
    /// `$*` is replaced by `args` (else `args` is appended). The expanded command is
    /// run through `sh -c` with stderr merged into stdout (vim's `'shellpipe'`
    /// semantics) so the directory-stack / multi-line `'errorformat'` matchers see
    /// the output in its original interleaving. Without a `!`, the cursor jumps to
    /// the first valid entry. The actual spawn is async (`nx._qf_make`); on a build
    /// with no local process spawn (the serverless web build) it fails loud.
    fn ex_make(&mut self, args: &str, bang: bool, is_grep: bool, is_loclist: bool) {
        let opts = self.editor.global_options();
        let (prg, efm) = if is_grep {
            (&opts.grepprg, &opts.grepformat)
        } else {
            (&opts.makeprg, &opts.errorformat)
        };
        if prg.trim().is_empty() {
            let which = if is_grep { "grepprg" } else { "makeprg" };
            self.editor.echo(format!("E91: '{which}' option is empty"));
            return;
        }
        let args = args.trim();
        let expanded = if prg.contains("$*") {
            prg.replace("$*", args)
        } else if args.is_empty() {
            prg.clone()
        } else {
            format!("{prg} {args}")
        };
        let verb = match (is_loclist, is_grep) {
            (false, false) => "make",
            (false, true) => "grep",
            (true, false) => "lmake",
            (true, true) => "lgrep",
        };
        let title = format!(":{verb} {args}");
        // Merge stderr into stdout in the child so the parser sees one ordered
        // stream (the make/gcc `Entering directory` lines and errors interleave).
        let cmd = format!("{expanded} 2>&1");
        // `Some(0)` targets the current window's location list at drain time; `None`
        // the global quickfix list.
        let loclist_win = is_loclist.then_some(0u64);
        if let Err(e) = self
            .lua
            .run_qf_make(&cmd, efm, title.trim_end(), true, !bang, loclist_win)
        {
            self.editor
                .echo(format!("E5108: Error starting :make: {e}"));
        }
        self.apply_lua_effects();
    }

    /// The languages a `:TSInstall` / `:TSUpdate` targets: its explicit whitespace-
    /// separated arguments, or — as a convenience when called with none — the current
    /// buffer's resolved filetype, so a bare `:TSInstall` installs the grammar for the
    /// file you're editing. `buffer_filetype` already maps the extension to the language
    /// name (`foo.rs` → `rust`) and honours a `:set ft=` override, so it's the right
    /// default. Empty when neither yields a name (no args and an unknown/absent extension).
    fn ts_install_langs(&self, args: &str) -> Vec<String> {
        let explicit: Vec<String> = args.split_whitespace().map(str::to_string).collect();
        if !explicit.is_empty() {
            return explicit;
        }
        self.editor
            .buffer_filetype(self.editor.current_buffer_id())
            .into_iter()
            .collect()
    }

    /// `:TSInstall <lang>…` — fetch + compile each named grammar into the data dir
    /// off the editor thread. The work (network + a C compile) can take seconds, so
    /// each language runs on a `spawn_blocking` worker; its result returns on the
    /// `install_events` `select!` arm ([`EditHost::on_install_done`]). We echo a
    /// "installing…" line now so the user sees the command took. Native only — the
    /// browser build's `:TSInstall` arm calls the wasm `ts_install` below instead.
    #[cfg(feature = "native")]
    fn ts_install(&mut self, args: &str) {
        let langs = self.ts_install_langs(args);
        if langs.is_empty() {
            self.editor.echo(
                "TSInstall: usage: :TSInstall <language>… (or open a file to install its language)",
            );
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

    /// `:TSInstall <lang>…` on the browser build — fetch each named *prebuilt* grammar
    /// (`.wasm` + queries) at runtime through the same `ts_install` effect, only here it
    /// crosses to the JS host (web-tree-sitter lives UI-side), which fetches from a CDN,
    /// caches in OPFS, and registers it. Fire-and-forget like the native path: the
    /// outcome lands later via [`EditHost::complete_ts_install`]. No C compiler / `dlopen`
    /// is involved — the browser loads a `.wasm` grammar, not a native `.so`.
    #[cfg(not(feature = "native"))]
    fn ts_install(&mut self, args: &str) {
        let langs = self.ts_install_langs(args);
        if langs.is_empty() {
            self.editor.echo(
                "TSInstall: usage: :TSInstall <language>… (or open a file to install its language)",
            );
            return;
        }
        self.editor
            .echo(format!("TSInstall: installing {}…", langs.join(", ")));
        for lang in langs {
            self.fx.ts_install(lang);
        }
    }

    /// `:TSInstallInfo` on the browser build — panel-list the grammars available to the
    /// JS highlighter (the offline bundle + whatever `:TSInstall` cached in OPFS), the
    /// wasm analogue of native's on-disk parser scan. Highlighting is JS-side here, so
    /// there is no parser dir / per-grammar query listing to show.
    #[cfg(not(feature = "native"))]
    fn ts_install_info(&mut self) {
        let langs = self.ts_installed_list();
        let mut lines = Vec::new();
        if langs.is_empty() {
            lines.push("No treesitter grammars installed.".to_string());
            lines.push(String::new());
            lines.push("Install one with  :TSInstall <language>".to_string());
        } else {
            lines.push(format!("Available treesitter grammars ({}):", langs.len()));
            lines.push(String::new());
            for lang in &langs {
                lines.push(lang.clone());
            }
        }
        self.editor
            .open_scratch_listing("[TSInstall info]", lines, 0);
    }

    /// `:TSInstallInfo` — open a panel listing every parser installed across the
    /// data-dir search path (nxvim's own dir + a borrowed neovim `site/`), each
    /// with the queries it ships and the root it resolves from. Installed parsers
    /// only: the full installable catalog lives behind a network fetch we don't do
    /// for a read-only info command.
    #[cfg(feature = "native")]
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
        self.editor
            .open_scratch_listing("[TSInstall info]", lines, 0);
    }

    /// Apply a finished `:TSInstall` job: on success, reload the grammar so every
    /// open buffer of that language re-highlights / re-indents against the new
    /// parser without a manual `:e`; on failure, echo the (loud) reason.
    #[cfg(feature = "native")]
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
        // A `colors/<name>.lua` on the runtimepath wins (so a user can shadow a
        // bundled scheme); otherwise fall back to a scheme embedded in the
        // binary, so `:colorscheme nxvim` works with zero config.
        let src = match self.find_runtime_file(&format!("colors/{name}.lua")) {
            Some(path) => match std::fs::read_to_string(&path) {
                Ok(src) => src,
                Err(e) => {
                    self.editor
                        .echo(format!("E185: Cannot read color scheme '{name}': {e}"));
                    return;
                }
            },
            None => match builtin_colorscheme(name) {
                Some(src) => src.to_string(),
                None => {
                    self.editor
                        .echo(format!("E185: Cannot find color scheme '{name}'"));
                    return;
                }
            },
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

    /// Surface the text a `nx._ex_*` autocmd driver returned: empty is nothing,
    /// a multi-line listing opens a read-only scratch buffer (like `:LspInfo`), and a
    /// single line is echoed (a message or an `E…` error).
    fn surface_autocmd_output(&mut self, title: &str, out: &str) {
        if out.is_empty() {
            return;
        }
        if out.contains('\n') {
            let lines = out.lines().map(str::to_string).collect();
            self.editor.open_scratch_listing(title, lines, 0);
        } else {
            self.editor.echo(out);
        }
    }
}

/// The Lua source of a colorscheme bundled in the binary, by name, or `None`
/// for an unknown name. These ship in the embedded `runtime/colors/` tree so
/// `:colorscheme <name>` works with no user config (and on the wasm build,
/// which has no filesystem). A user file on the runtimepath shadows these — the
/// caller searches the runtimepath first.
fn builtin_colorscheme(name: &str) -> Option<&'static str> {
    match name {
        "nxvim" => Some(include_str!("../runtime/colors/nxvim.lua")),
        _ => None,
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

/// The user's home directory from `$HOME` (the Unix `:cd` target / `~` base),
/// or `None` when it is unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Expand a `:cd` path argument: a leading `~` / `~/` resolves against `$HOME`,
/// anything else is taken verbatim (a relative path resolves against the current
/// cwd inside `set_current_dir`, so no canonicalization is needed here).
fn expand_cd_arg(arg: &str) -> PathBuf {
    if arg == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = arg.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(arg)
}
