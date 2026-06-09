//! Ex-command resolution: the `:`-command entry point, the fallback resolver for
//! commands the core didn't recognize (LSP commands, user commands, colorscheme),
//! and runtime-file lookup.

use crate::lsp::LspReqKind;
use crate::Server;
use std::path::PathBuf;

impl Server {
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
        match name {
            "colorscheme" | "colo" => self.set_colorscheme(args.trim()),
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
            _ if is_doautocmd(base) => {
                match self.lua.ex_doautocmd(args) {
                    Ok(out) => self.surface_autocmd_output("Autocommands", &out),
                    Err(e) => self.editor.echo(format!("E5108: Error in :doautocmd: {e}")),
                }
                // A fired autocmd may have queued `vim.cmd(...)` / callbacks.
                self.apply_lua_effects();
            }
            _ if self.lua.has_user_command(name) => {
                if let Err(e) = self.lua.run_user_command(name, args) {
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
