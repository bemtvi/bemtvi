//! How bemtvi spawns child processes, as a policy every spawn site shares.
//!
//! On Windows a console program started by a process that has no console of its
//! own gets a brand-new console window. `bemtvi-gui` is a GUI-subsystem binary, so
//! without this every language server, `btv.run` job, grammar compile and daemon
//! child would flash up its own empty console window. Every child bemtvi starts
//! talks over pipes (or a pseudo-console, for `:terminal`) and never needs a visible
//! console, so each spawn site passes its command through [`no_console_window`],
//! TUI or GUI alike. Elsewhere it does nothing.

/// Start `command`'s child without a console window (Windows `CREATE_NO_WINDOW`).
/// A no-op on every other platform. A `tokio::process::Command` passes its
/// `as_std_mut()`.
pub fn no_console_window(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `CREATE_NO_WINDOW` from `processthreadsapi.h`: the child still gets a console
        // (so the console programs it starts in turn share it), just never a visible one.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
