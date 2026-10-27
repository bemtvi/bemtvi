//! The `:` command lines that ask a GUI client for a native file dialog instead of running
//! as typed. Pure: which dialog a line asks for, and the command it becomes. The editor
//! decides as it submits the line ([`crate::editor::Editor::file_dialogs`]), so it acts on
//! the line it runs and not on whatever a client last drew; the dialog itself is the
//! client's to show.
//!
//! - The `…o` open family ([`open_dialog_verb`]) — `:eo`, `:spo`, `:vso`, `:tabeo`,
//!   `:newo`, `:vnewo` (and bare `:e`, an alias of `:eo`) — asks for the **open** dialog
//!   and re-runs the base command (`:e`/`:sp`/`:vs`/`:tabe`/…) with the chosen file.
//! - Opening a directory (`:e somedir`, [`open_path_command`]) asks for the open dialog
//!   there, instead of the in-window listing.
//! - `:wo`, and a bare `:w` on an unnamed buffer ([`save_dialog_needed`]), ask for the
//!   **save** dialog and write the buffer to the chosen path (`:w <file>`). (`:wo`
//!   mirrors the `…o` open family and, unlike `:wn`, collides with no real ex-command —
//!   vim's `:wn` is `:wnext`.)

/// The base ex verb to run after the native **open** dialog, if `cmdline` is one
/// of the `…o` open commands — `:eo`→`e`, `:spo`→`sp`, `:vso`→`vs`, `:tabeo`→`tabe`,
/// `:newo`→`new`, `:vnewo`→`vnew` — or bare `:e`/`:edit`, an alias of `:eo`. `None`
/// for anything else (including bare `:sp`/`:vs`/`:tabe`, which keep their usual
/// no-argument behavior). Pure, so it is unit-tested in `tests/keys.rs`.
pub fn open_dialog_verb(cmdline: &str) -> Option<&'static str> {
    match cmdline.trim() {
        // Bare `:e`/`:edit` aliases `:eo`; `:eo` is the explicit form.
        "e" | "ed" | "edi" | "edit" | "eo" => Some("e"),
        "spo" => Some("sp"),
        "vso" => Some("vs"),
        "tabeo" => Some("tabe"),
        "newo" => Some("new"),
        "vnewo" => Some("vnew"),
        _ => None,
    }
}

/// If `cmdline` is a file-**opening** ex command carrying a path argument, the
/// `(base_verb, arg)` to act on: the canonical verb to re-run and the raw path tail.
/// Only the commands the server routes through `ex_edit` — where a *directory*
/// argument opens the in-window netrw listing — are matched: `:e`/`:edit`,
/// `:sp`/`:split`, `:vs`/`:vsplit`, and `:tabe`/`:tabedit`/`:tabnew`. Everything
/// else is left alone, so a directory argument to `:cd`, `:lcd`, `:w`, `:grep`,
/// `:set`, … is *not* mistaken for an open (the caller would otherwise wrongly pop a
/// file picker for `:cd somedir`). `None` for a non-open command or one with no
/// argument (the bare forms are [`open_dialog_verb`]'s job). The caller decides
/// whether `arg` is actually a directory; this is pure, so it is unit-tested in
/// `tests/keys.rs`.
pub fn open_path_command(cmdline: &str) -> Option<(&'static str, &str)> {
    let (cmd, arg) = cmdline.trim_start().split_once(char::is_whitespace)?;
    let arg = arg.trim();
    if arg.is_empty() {
        return None; // no argument → a bare command, not an open-with-path
    }
    // A trailing `!` (`:e! dir`) is a force flag, irrelevant to listing a directory.
    let base = match cmd.strip_suffix('!').unwrap_or(cmd) {
        "e" | "edit" => "e",
        "sp" | "spl" | "split" => "sp",
        "vs" | "vsp" | "vspl" | "vsplit" => "vs",
        "tabe" | "tabed" | "tabedit" | "tabnew" => "tabe",
        _ => return None,
    };
    Some((base, arg))
}

/// Whether `<CR>` over `cmdline` should pop the native **save** dialog: `:wo`
/// (save to a new file) always, or a bare `:w`/`:write` when the focused buffer is
/// `unnamed` (so a plain `:w` has no file to write to). `None`/false leaves the
/// command to run as typed. `:wo` is chosen to mirror the `…o` open family and
/// because it shadows no real ex-command (vim's `:wn` is `:wnext`). Pure, so it
/// is unit-tested in `tests/keys.rs`.
pub fn save_dialog_needed(cmdline: &str, unnamed: bool) -> bool {
    matches!(cmdline.trim(), "wo")
        || (unnamed && matches!(cmdline.trim(), "w" | "wr" | "wri" | "writ" | "write"))
}

/// What `<CR>` over a `:` command line should make the GUI do — the native-dialog
/// affordance, decided in one place (see [`dialog_action`]).
#[derive(Debug, PartialEq, Eq)]
pub enum DialogAction<'a> {
    /// Pop the **open** dialog, then run `<base> <picked>` (the `…o` family / bare `:e`).
    Open { base: &'static str },
    /// `cmdline` opens `arg` with `base`; if `arg` is a local **directory** the caller
    /// pops the open dialog anchored there, else the command runs as typed (`:e somedir`).
    OpenPath { base: &'static str, arg: &'a str },
    /// Pop the **save** dialog, then `:w <picked>` (`:wo` / a bare `:w` on an unnamed buffer).
    Save,
}

/// Decide what `<CR>` over a `:` command line should do in the GUI: pop a native file
/// dialog, or nothing (run the command as typed). Folds the three pure predicates
/// ([`open_dialog_verb`], [`open_path_command`], [`save_dialog_needed`]) into one
/// decision, in their established priority order.
///
/// Returns `None` in a **remote (daemon) session** (`remote == true`): the buffers live
/// on the *daemon's* fs, so a local native dialog would browse and write the wrong
/// machine — the command must run as typed and let the server handle it (the in-window
/// netrw listing for `:e <dir>`, `E32` for a nameless `:w`, …). The `OpenPath`
/// directory-vs-file test is the caller's (it touches the local fs), keeping this pure
/// and unit-tested in `tests/keys.rs`.
pub fn dialog_action(cmdline: &str, unnamed: bool, remote: bool) -> Option<DialogAction<'_>> {
    if remote {
        return None;
    }
    if let Some(base) = open_dialog_verb(cmdline) {
        Some(DialogAction::Open { base })
    } else if let Some((base, arg)) = open_path_command(cmdline) {
        Some(DialogAction::OpenPath { base, arg })
    } else if save_dialog_needed(cmdline, unnamed) {
        Some(DialogAction::Save)
    } else {
        None
    }
}

/// A file dialog a submitted command line asked for: what the editor hands the server to
/// pass on to its client ([`DeferredCmd::FileDialog`](crate::editor::DeferredCmd)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDialog {
    /// The open dialog, then `<base> <picked>`.
    Open { base: &'static str },
    /// `line` opens `arg` with `base`: the open dialog in that directory if `arg` is one,
    /// else `line` runs as typed. Whether it is one is the server's to find out (the core
    /// does no I/O), against the editor's working directory.
    OpenAt {
        base: &'static str,
        arg: String,
        line: String,
    },
    /// The save dialog, then `:w <picked>`.
    Save,
}

/// The [`FileDialog`] `cmdline` asks for, submitted with the focused buffer `unnamed` or
/// not; `None` runs it as typed.
pub fn file_dialog(cmdline: &str, unnamed: bool) -> Option<FileDialog> {
    Some(match dialog_action(cmdline, unnamed, false)? {
        DialogAction::Open { base } => FileDialog::Open { base },
        DialogAction::OpenPath { base, arg } => FileDialog::OpenAt {
            base,
            arg: arg.to_owned(),
            line: cmdline.to_owned(),
        },
        DialogAction::Save => FileDialog::Save,
    })
}
