//! Ex-command resolution: the `:`-command entry point, the fallback resolver for
//! commands the core didn't recognize (LSP commands, user commands, colorscheme),
//! and runtime-file lookup.

use crate::cwd::CdScope;
use crate::lsp::LspReqKind;
use crate::EditHost;
use std::path::{Component, Path, PathBuf};

impl EditHost {
    pub(crate) fn run_command(&mut self, cmd: &str) {
        // Pick up any keymap / autocmd registry change before the command runs — like
        // the `nx_input` batch does. A `:e` opens a file, and whether it defers for a
        // `BufReadCmd` handler depends on the up-to-date `au_active_events` cache (the
        // `bufreadcmd_active` mirror); without this a `:edit` issued before the first
        // keystroke would miss a handler registered at startup.
        self.refresh_keymaps();
        self.editor.command(cmd);
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Run `cmd` to full convergence with its message output suppressed — the
    /// `:sil[ent][!] {cmd}` modifier, and what `nx.cmd(cmd, { silent = … })`
    /// compiles to. The `!` form is the one plugins lean on to ignore a command
    /// that errors or doesn't exist (a plugin manager's `silent! runtime
    /// plugin/rplugin.vim`).
    ///
    /// Suppression is done by snapshot-and-restore around the run (the inner
    /// command may defer further — `run_command` drains that): whatever it echoed
    /// is rolled back off both the message line and the `:messages` history, which
    /// is where vim puts it too (`msg_hist_add` skips the entry outright while
    /// `msg_silent` is set).
    ///
    /// `bang` is the only thing that separates the two forms, exactly as in vim:
    ///
    /// * `:silent!` (`bang`) sets vim's `emsg_silent` as well, so *errors are
    ///   swallowed too* and the line is always restored.
    /// * a bare `:silent` keeps errors. Vim resets `msg_silent` the moment one is
    ///   emitted ("an error causes messages to be switched back on", message.c) —
    ///   so the error **and anything the command said after it** stay visible,
    ///   while the output that preceded it is still dropped. That is what the
    ///   error-position split below reproduces: [`Editor::echo`] already classifies
    ///   each recorded line by vim's `E###:` convention, so the level distinction
    ///   the flag needs is in the history already.
    pub(crate) fn run_silent(&mut self, cmd: &str, bang: bool) {
        if cmd.is_empty() {
            return;
        }
        let saved_msg = self.editor.message.clone();
        let saved_err = self.editor.message_error;
        let saved_len = self.editor.messages.len();
        let cmd = cmd.to_string();
        self.run_command(&cmd);
        // The history is a capped ring, so a command that logged a flood can leave
        // it *shorter* than the mark we took; clamp before slicing from it.
        let saved_len = saved_len.min(self.editor.messages.len());
        let first_error = (!bang)
            .then(|| {
                self.editor.messages[saved_len..]
                    .iter()
                    .position(|m| m.error)
            })
            .flatten();
        match first_error {
            // Messages came back on at the error: keep it and everything after,
            // and leave the message line showing whatever the command left there.
            Some(off) => {
                self.editor.messages.drain(saved_len..saved_len + off);
            }
            // Nothing survives the modifier — put the line back as we found it.
            None => {
                self.editor.messages.truncate(saved_len);
                self.editor.message = saved_msg;
                self.editor.message_error = saved_err;
            }
        }
    }

    /// Resolve an ex-command the core didn't recognize: load a colorscheme,
    /// dispatch a Lua user command if one is registered under that name, or
    /// report the standard unknown-command error. `cmd` is the trimmed line.
    ///
    /// `range` is the explicitly addressed 0-based inclusive line range the core
    /// resolved off the head of the line (`:'<,'>LspCodeAction`, `:5,10LspCodeAction`),
    /// or `None` when the command carried no address.
    pub(crate) fn resolve_command(&mut self, cmd: &str, range: Option<(usize, usize)>) {
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
            // `:reconnect` / `:disconnect` — drive a remote session's daemon link. The link
            // re-dials underneath the seams the editor already holds, so neither command
            // touches local buffers/undo. No-ops with a loud message in a local session.
            "reconnect" => self.ex_reconnect(),
            "disconnect" => self.ex_disconnect(),
            // Phase-1 LSP observability: dump server/document state into a listing.
            // The LSP ex-command surface is NOT native-gated: every method below
            // works on the wasm edit-host too, where servers run on the daemon and
            // requests/replies ride `HostEffects::lsp_request` (the web python demo
            // drives basedpyright this way). Gating them out made `:LspDiagnostics`
            // & friends fall through to an unknown-command error on the web build
            // even though diagnostics were flowing.
            "LspInfo" => {
                let lines = self.lsp_info_lines();
                self.editor.open_scratch_listing("[LSP info]", lines, 0);
            }
            // Phase-2: list the current buffer's diagnostics as a navigable
            // location list; `<CR>` on a row jumps to it (handled in the core).
            "LspDiagnostics" => match self.diagnostics_location_list() {
                Some(entries) => self.editor.open_location_list(entries, "LSP diagnostics"),
                None => self.editor.echo("No diagnostics"),
            },
            // Phase-3/4: go-to / references / hover / signature help as ex-commands
            // (the keymap-free path; the reply jumps the cursor, opens a panel
            // location list, or floats the docs). The ex-command path is
            // fire-and-forget (no Lua promise), so each passes `cb_id = 0`.
            //
            // Each takes an optional `[server]` — the config name of the attached
            // client to route to (`:LspHover pyright`), the ex twin of
            // `nx.lsp.hover{ name = … }`. Without it the request goes to the
            // capability-ordered default pick, which on a two-server buffer is not
            // always the one you meant.
            "LspDefinition" | "LspDeclaration" | "LspTypeDefinition" | "LspImplementation"
            | "LspReferences" | "LspHover" | "LspSignatureHelp" => {
                let kind = match name {
                    "LspDefinition" => LspReqKind::Definition,
                    "LspDeclaration" => LspReqKind::Declaration,
                    "LspTypeDefinition" => LspReqKind::TypeDefinition,
                    "LspImplementation" => LspReqKind::Implementation,
                    "LspReferences" => LspReqKind::References,
                    "LspHover" => LspReqKind::Hover,
                    _ => LspReqKind::SignatureHelp,
                };
                match lsp_server_arg(args) {
                    Ok(server) => self.request_lsp(kind, 0, server),
                    Err(msg) => self.editor.echo(msg),
                }
            }
            // Phase-6: buffer-mutating features. Format/code-action take only the
            // optional `[server]`; rename reads the new name the dispatcher split
            // off — or, with no name, prompts for it through `vim.lsp.buf.rename()`
            // (`vim.ui.input`, Phase 8) instead of erroring.
            // `:LspFormat [server]` — the optional argument picks which attached
            // server formats (the ex twin of `nx.lsp.format{ name = … }`).
            "LspFormat" => match lsp_server_arg(args) {
                Ok(server) => self.request_lsp_format(0, server),
                Err(msg) => self.editor.echo(msg),
            },
            "LspRename" if args.trim().is_empty() => {
                if let Err(e) = self.lua.exec("vim.lsp.buf.rename()") {
                    self.editor
                        .echo(format!("E5108: Error in :LspRename prompt: {e}"));
                }
                self.apply_lua_effects();
            }
            // `:LspRename {newname} [server]` — the new identifier is the first word
            // (an identifier never holds a space), so a second one is the client to
            // route to, as on every other `:Lsp*` verb.
            "LspRename" => {
                let args = args.trim();
                let new_name = args.split_whitespace().next().unwrap_or("");
                match lsp_server_arg(&args[new_name.len()..]) {
                    Ok(server) => self.request_lsp_rename(new_name, 0, server),
                    Err(msg) => self.editor.echo(msg),
                }
            }
            // The ex-command is always the interactive, unfiltered form — the kind
            // filter / one-shot apply are `nx.lsp.code_action(opts)` options. An
            // address (`:'<,'>LspCodeAction`, typed straight off a Visual selection)
            // scopes the request to those **whole lines** — an ex address is a line,
            // not a column, so the range runs to the end of the last addressed one.
            // With no address the request falls back to the cursor (or, called from a
            // Lua keymap, to the live selection). `[server]` asks only that client
            // instead of merging every capable server's actions into one chooser.
            "LspCodeAction" => match lsp_server_arg(args) {
                Ok(server) => {
                    let range =
                        range.map(|(lo, hi)| (lo, 0, hi, self.editor.buffer().line_len(hi)));
                    self.request_lsp_code_action(0, Default::default(), range, server)
                }
                Err(msg) => self.editor.echo(msg),
            },
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
            // `:sil[ent][!] {cmd}` — the silent command modifier. The core defers the
            // whole `silent …` string here, so this is the one place it resolves.
            _ if is_silent(base) => self.run_silent(args.trim(), bang),
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
            // `:TSInstall <lang>…` / `:TSUpdate <lang>…` — install a treesitter
            // grammar through the per-world `ts_install` effect (native: fetch +
            // compile into the data dir, see `nxvim_ts::install`; browser: the JS
            // host fetches a prebuilt `.wasm` grammar, see [`Self::ts_install`]).
            // The guard defers to a real nvim-treesitter plugin: if the user loaded
            // one and it registered `:TSInstall`, this arm is skipped and the
            // user-command arm below runs the plugin's instead (no silent shadow).
            "TSInstall" | "TSUpdate" if !self.lua.has_user_command(name, cur_buf) => {
                self.ts_install(args)
            }
            // `:TSInstallInfo` — list the installed grammars (native: the on-disk
            // parser scan; browser: what the JS highlighter can load). Same
            // defer-to-plugin guard as the install commands.
            "TSInstallInfo" if !self.lua.has_user_command(name, cur_buf) => self.ts_install_info(),
            // `:help [topic]` — the help system ships as the optional `nxvim-help`
            // plugin (it registers `:help` / `:h`, handled by the user-command arm
            // below when installed). With the plugin absent, point the user at it
            // rather than emit the bare unknown-command error.
            _ if matches!(base, "help" | "h") && !self.lua.has_user_command(name, cur_buf) => {
                self.editor.echo(
                    "help: the nxvim-help plugin is not installed — add it with \
                     :Plugins (nxvim/nxvim-help), then use :help {topic}",
                )
            }
            // `:helpt[ags]` — tag generation lives in nxvim-help as `:NxHelptags`
            // (nxvim has no built-in `:helptags`). A plugin that registered
            // `:helptags` itself would shadow this via the arm below.
            _ if matches!(base, "helpt" | "helpta" | "helptag" | "helptags")
                && !self.lua.has_user_command(name, cur_buf) =>
            {
                self.editor.echo(
                    "helptags: install the nxvim-help plugin (:Plugins) and use \
                     :NxHelptags to generate help tags",
                )
            }
            // Look the command up by its bare name (`base`): a trailing `!` is the
            // invocation's bang, never part of the registered name, so matching on
            // `name` would send `:PluginSync!` to E492 even though the command
            // exists.
            _ if self.lua.has_user_command(base, cur_buf) => {
                if let Err(e) = self.lua.run_user_command_bang(base, args, cur_buf, bang) {
                    self.editor
                        .echo(format!("E5108: Error executing command {base}: {e}"));
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
    pub(crate) fn ex_chdir(&mut self, scope: CdScope, arg: &str) {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();

        // Daemon session: the cwd lives on the remote, so validate + canonicalize the
        // target on the daemon (off-tick `fs_chdir`) instead of touching the local process
        // cwd. The cwd moves **optimistically** right now — so a relative `:e` / `getcwd` in
        // the same breath resolves against the new dir — while the announcing `DirChanged`
        // (and any rollback) wait for the daemon: `apply_chdir` finalizes the canonical dir,
        // or reverts on `E344`. `-` (previous) and a relative path resolve against the
        // edit-host's `DirState` and so can move optimistically; `""` (→ the daemon's
        // `$HOME`) and `~…` expand against the *daemon's* home, which we can't predict, so
        // they install only on the ack. See `docs/plans/2026-06-23-remote-cwd.md`.
        if self.editor.host_fs_offtick() {
            let (wire, optimistic): (String, Option<PathBuf>) = match arg {
                "-" => match self.dirs.prev(scope, win, tab) {
                    Some(p) => {
                        let p = p.to_path_buf();
                        (p.to_string_lossy().into_owned(), Some(p))
                    }
                    None => {
                        self.editor.echo("E186: No previous directory");
                        return;
                    }
                },
                "" => (String::new(), None),
                _ if arg.starts_with('~') => (arg.to_string(), None),
                _ => {
                    let joined = if Path::new(arg).is_absolute() {
                        PathBuf::from(arg)
                    } else {
                        let (_, base) = self.dirs.effective(win, tab);
                        base.join(arg)
                    };
                    // Lexically normalize so the optimistic dir is clean and matches the
                    // daemon's canonical form when no symlink is in play (a symlink
                    // difference is reconciled on the ack).
                    let abs = lexical_normalize(&joined);
                    (abs.to_string_lossy().into_owned(), Some(abs))
                }
            };
            let undo = optimistic.map(|dir| self.dirs.set_optimistic(scope, win, tab, dir));
            if undo.is_some() {
                self.publish_cwd_mirror();
            }
            let token = self.next_chdir_token;
            self.next_chdir_token += 1;
            self.pending_chdirs.insert(
                token,
                crate::cwd::PendingChdir {
                    scope,
                    win,
                    tab,
                    undo,
                },
            );
            self.fx.fs_chdir(wire, token);
            return;
        }

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
        // Keep the `nx._cwd` mirror (`vim.fn.getcwd`) in step with `DirState`.
        self.publish_cwd_mirror();
        // Announce the change so `DirChanged` handlers (project / session plugins) run.
        let r = self
            .lua
            .fire_dir_changed(scope.pattern(), &cwd.display().to_string());
        self.report_autocmd_err("DirChanged", r);
        self.apply_lua_effects();
    }

    /// `:pwd` — print the working directory (the current window's effective dir) on the
    /// message line. Reads [`DirState`] rather than `std::env::current_dir()` so a daemon
    /// session reports the *daemon's* cwd, not the local process's; for a local session
    /// `DirState` tracks the process cwd, so this is unchanged there.
    fn ex_pwd(&mut self) {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, dir) = self.dirs.effective(win, tab);
        self.editor.echo(dir.display().to_string());
    }

    /// `:reconnect` — re-dial the remote daemon now, resetting the auto-retry budget. Use
    /// after the link gave up (status `disconnected`) or to retry sooner than the backoff.
    /// The seams rebind in place, so the editor keeps its buffers/undo. A loud no-op in a
    /// local session.
    fn ex_reconnect(&mut self) {
        // The reconnectable link is native-only (its handle lives in the native transport
        // tree); the wasm edit-host has no `:reconnect` yet (a later phase).
        #[cfg(feature = "native")]
        if let Some(link) = &self.daemon_link {
            link.reconnect();
            self.editor.echo("reconnecting to the daemon…");
            return;
        }
        self.editor
            .echo("E: :reconnect needs a daemon session (this session is local)");
    }

    /// `:disconnect` — drop the live daemon link and stay disconnected until `:reconnect`.
    /// The editor keeps editing locally; remote ops (save, LSP, watch, terminal) fail loud
    /// until reconnected. A loud no-op in a local session.
    fn ex_disconnect(&mut self) {
        #[cfg(feature = "native")]
        if let Some(link) = &self.daemon_link {
            link.disconnect();
            self.editor.echo("disconnecting from the daemon");
            return;
        }
        self.editor
            .echo("E: :disconnect needs a daemon session (this session is local)");
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

    /// `:TSInstall <lang>…` — fetch each named grammar off the editor thread,
    /// fire-and-forget through the `ts_install` effect seam (we echo an
    /// "installing…" line now so the user sees the command took). What the effect
    /// *does* is per-world: native fetches + C-compiles into the data dir on a
    /// `spawn_blocking` worker, returning on the `install_events` `select!` arm
    /// ([`EditHost::on_install_done`]); the browser build crosses to the JS host
    /// (web-tree-sitter lives UI-side), which fetches a *prebuilt* `.wasm`
    /// grammar and its queries from a CDN, caches in OPFS, and registers it —
    /// landing later via [`EditHost::complete_ts_install`]. One command body;
    /// the divergence lives behind `fx.ts_install`.
    fn ts_install(&mut self, args: &str) {
        let langs = self.ts_install_langs(args);
        if langs.is_empty() {
            self.editor.echo(
                "TSInstall: usage: :TSInstall <language>… (or open a file to install its language)",
            );
            return;
        }
        // A repo spec (`owner/repo`) means compiling arbitrary source — impossible in
        // the browser build (prebuilt `.wasm` grammars from a CDN, no C compiler). Fail
        // loud rather than silently no-op (there is no CDN entry for a custom repo).
        // Native `install()` dispatches repo specs itself, so they pass straight through.
        #[cfg(not(feature = "native"))]
        let langs = {
            let (repos, langs): (Vec<String>, Vec<String>) =
                langs.into_iter().partition(|l| l.contains('/'));
            if !repos.is_empty() {
                self.editor.echo(format!(
                    "TSInstall: installing from a GitHub repo ({}) needs a C compiler and \
                     isn't supported in the browser build — use a native nxvim",
                    repos.join(", ")
                ));
            }
            if langs.is_empty() {
                return;
            }
            langs
        };
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
                let inherited = if report.inherited.is_empty() {
                    String::new()
                } else {
                    format!(" +inherited[{}]", report.inherited.join(", "))
                };
                // A repo install surfaces the grammar's declared file-types (Phase 2
                // will register these for detection); a catalog install has none.
                let file_types = if report.file_types.is_empty() {
                    String::new()
                } else {
                    format!(" file-types[{}]", report.file_types.join(", "))
                };
                self.editor.echo(format!(
                    "TSInstall: installed {} @ {short} [{}] (queries: {queries}{inherited}){file_types}",
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
            // The query form: report the active scheme (`g:colors_name`, which
            // every load below records), `default` before any scheme loads — vim's
            // behavior for `:colorscheme` with no argument.
            let active = self
                .lua
                .get_global_var("colors_name")
                .unwrap_or_else(|| "default".to_string());
            self.editor.echo(active);
            return;
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
        // Drop the groups the *previous* scheme owned before sourcing this one, so
        // the two palettes replace rather than stack. Without this every group the
        // incoming scheme leaves undefined keeps the outgoing scheme's value — most
        // visibly when a truecolor attach defaults in the bundled `nxvim` (One Dark)
        // and the user's config picks its own theme a moment later: the theme paints
        // what it models and One Dark shows through everywhere else, so the result is
        // a blend of two palettes that shifts with startup timing. Only the scheme's
        // own groups go; a plugin's stay (and a plugin restyling on `ColorScheme`
        // re-registers below anyway). Blank definitions are how the registry removes.
        let dropped: Vec<String> = std::mem::take(&mut self.scheme_groups)
            .into_iter()
            .collect();
        for group in &dropped {
            self.editor
                .highlights
                .set_ns(0, group, nxvim_core::highlight::HlDef::default());
        }
        // Erase the same rows from the Lua mirror right now. The full mirror push is
        // gated on the registry generation and only lands between turns, so the
        // `ColorScheme` handler fired below would otherwise still read these groups as
        // defined, with the OUTGOING theme's colours — and a plugin re-deriving its
        // defaults there would skip them as "already styled".
        let _ = self.lua.clear_hl_mirror_rows(&dropped);
        if let Err(e) = self.lua.exec(&src) {
            self.editor
                .echo(format!("E5108: Error loading colorscheme {name}: {e}"));
        }
        // Read the queued definitions before `apply_lua_effects` drains them: these
        // are the groups this scheme owns, and the next load drops exactly them.
        self.scheme_groups = self.lua.peek_global_highlight_names().into_iter().collect();
        self.apply_lua_effects();
        let _ = self.lua.set_global_var("colors_name", name);
        let r = self.lua.fire_autocmd("ColorScheme", name);
        self.report_autocmd_err("ColorScheme", r);
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

/// The color schemes bundled in the binary: `(name, Lua source)`, with the source
/// embedded from the `runtime/colors/` tree. The single source of truth for both
/// [`builtin_colorscheme`] (loads one) and the `:colorscheme` completion catalog
/// (the server hands the names to Lua as `nx._builtin_colorschemes`) — add a scheme
/// here and both pick it up. They ship embedded so `:colorscheme <name>` works with
/// no user config (and on the wasm build, which has no filesystem).
pub(crate) const BUILTIN_COLORSCHEMES: &[(&str, &str)] =
    &[("nxvim", include_str!("../runtime/colors/nxvim.lua"))];

/// The Lua source of a colorscheme bundled in the binary, by name, or `None`
/// for an unknown name. A user file on the runtimepath shadows these — the
/// caller searches the runtimepath first.
fn builtin_colorscheme(name: &str) -> Option<&'static str> {
    BUILTIN_COLORSCHEMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, src)| *src)
}

/// The optional `[server]` argument every `:Lsp*` verb takes: the config name of
/// the attached client to route the request to (`:LspHover pyright`), or `None`
/// when the argument is absent and the default capability pick applies.
///
/// A *second* word is an error, not a second server — these commands route to one
/// client, so `:LspHover pyright ruff` is a typo that must say so rather than
/// silently asking pyright. (`E488` is vim's trailing-characters error, what
/// `:command -nargs=?` reports for an extra argument.)
fn lsp_server_arg(args: &str) -> Result<Option<&str>, String> {
    let mut words = args.split_whitespace();
    let server = words.next();
    match words.next() {
        Some(extra) => Err(format!("E488: Trailing characters: {extra}")),
        None => Ok(server),
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

/// Lexically normalize an **absolute** path — collapse `.` and `..` components without
/// touching the filesystem. Used for the optimistic remote `:cd` dir: the daemon
/// canonicalizes for real on its side, but this keeps the dir `getcwd` shows clean and
/// makes it match the daemon's canonical form whenever no symlink is involved (a symlink
/// difference is reconciled on the ack). `..` never climbs above the root.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            c => out.push(c),
        }
    }
    out.iter().collect()
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
