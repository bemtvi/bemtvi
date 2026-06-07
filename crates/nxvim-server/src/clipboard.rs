//! The real host-clipboard provider backing the `"+` / `"*` registers.
//!
//! Core defines the [`Clipboard`] seam; this is the server's concrete
//! implementation. Rather than pull in a platform GUI crate, it shells out to
//! the standard per-OS clipboard tools (`pbcopy`/`pbpaste` on macOS,
//! `wl-copy`/`wl-paste` or `xclip` on Linux). The shell-out is *lazy* — the tool
//! runs only when a `"+` yank/paste actually happens — so a headless box never
//! pays for a clipboard it doesn't use, and [`SystemClipboard::detect`] returns
//! `None` when no supported tool is on `PATH`, leaving the editor to error
//! loudly on `"+` instead of silently succeeding.
//!
//! Linewise-ness isn't something the OS clipboard stores, so it follows vim's
//! convention: text is linewise iff it ends in a newline (a linewise yank keeps
//! its trailing `\n`). Reading echoes that back; writing leaves the text as-is.

use std::io::Write;
use std::process::{Command, Stdio};

use nxvim_core::Clipboard;

/// A `(set, get)` pair of clipboard commands — `set` reads the new contents on
/// stdin, `get` prints the current contents on stdout.
struct Tool {
    set: &'static [&'static str],
    get: &'static [&'static str],
}

/// Host clipboard driven by an external tool. Holds the resolved command pair so
/// each call just spawns it.
pub struct SystemClipboard {
    tool: Tool,
}

impl SystemClipboard {
    /// The first clipboard tool available on this host, or `None` when none is
    /// found (the editor then has no `"+` provider and errors loudly on use).
    pub fn detect() -> Option<SystemClipboard> {
        let candidates: &[Tool] = if cfg!(target_os = "macos") {
            &[Tool {
                set: &["pbcopy"],
                get: &["pbpaste"],
            }]
        } else {
            &[
                Tool {
                    set: &["wl-copy"],
                    get: &["wl-paste", "--no-newline"],
                },
                Tool {
                    set: &["xclip", "-selection", "clipboard"],
                    get: &["xclip", "-selection", "clipboard", "-o"],
                },
            ]
        };
        candidates
            .iter()
            .find(|t| tool_exists(t.set[0]))
            .map(|t| SystemClipboard {
                tool: Tool {
                    set: t.set,
                    get: t.get,
                },
            })
    }
}

impl Clipboard for SystemClipboard {
    fn get(&self) -> Option<(String, bool)> {
        let out = Command::new(self.tool.get[0])
            .args(&self.tool.get[1..])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let linewise = text.ends_with('\n');
        Some((text, linewise))
    }

    fn set(&self, text: &str, _linewise: bool) {
        // A linewise yank already carries its trailing `\n`, so the text is
        // written verbatim; reading it back re-derives linewise from that newline.
        let Ok(mut child) = Command::new(self.tool.set[0])
            .args(&self.tool.set[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// Whether `name` resolves to an executable on `PATH` (a cheap `command -v`).
fn tool_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
