//! The real host-clipboard provider backing the `"+` / `"*` registers.
//!
//! Core defines the [`Clipboard`] seam; this is the server's concrete
//! implementation. Rather than pull in a platform GUI crate, it shells out to
//! the standard per-OS clipboard tools (`pbcopy`/`pbpaste` on macOS,
//! `wl-copy`/`wl-paste` or `xclip` on Linux, `tmux` inside a multiplexer). The
//! shell-out is *lazy* — the tool runs only when a `"+` yank/paste actually
//! happens — so a headless box never pays for a clipboard it doesn't use, and
//! [`SystemClipboard::detect`] returns `None` when no *usable* tool is found.
//!
//! Usable is the operative word: a tool being installed is not the same as it
//! working. `xclip` on a machine with no `$DISPLAY` (the ssh case) exits with
//! "can't open display" on every yank, so gating each X/Wayland tool on its
//! display variable is what keeps a copy from silently going nowhere — the same
//! condition vim and neovim apply.
//!
//! When nothing usable is found the server falls back to [`Osc52Clipboard`]: the
//! *terminal* becomes the clipboard. That is the ssh story — no tool can run on
//! the remote box that would reach the machine the user is sitting at, but the
//! terminal emulator in front of them can be asked to set its own clipboard with
//! an OSC 52 escape, which is exactly what travels back down the ssh pipe.
//!
//! Linewise-ness isn't something the OS clipboard stores, so it follows vim's
//! convention: text is linewise iff it ends in a newline (a linewise yank keeps
//! its trailing `\n`). Reading echoes that back; writing leaves the text as-is.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bemtvi_core::Clipboard;

/// A `(set, get)` pair of clipboard commands — `set` reads the new contents on
/// stdin, `get` prints the current contents on stdout — plus the environment
/// variable that must be non-empty for the pair to be able to work at all
/// (`None` for a tool with no such prerequisite).
struct Tool {
    set: &'static [&'static str],
    get: &'static [&'static str],
    needs_env: Option<&'static str>,
}

/// Host clipboard driven by an external tool. Holds the resolved command pair so
/// each call just spawns it.
pub struct SystemClipboard {
    set: &'static [&'static str],
    get: &'static [&'static str],
}

impl SystemClipboard {
    /// The first *usable* clipboard tool on this host, or `None` when none is
    /// found — the editor then falls back to OSC 52 if the client can do it, and
    /// otherwise has no `"+` provider and errors loudly on use.
    pub fn detect() -> Option<SystemClipboard> {
        let candidates: &[Tool] = if cfg!(target_os = "macos") {
            &[Tool {
                set: &["pbcopy"],
                get: &["pbpaste"],
                needs_env: None,
            }]
        } else {
            &[
                Tool {
                    set: &["wl-copy"],
                    get: &["wl-paste", "--no-newline"],
                    needs_env: Some("WAYLAND_DISPLAY"),
                },
                Tool {
                    set: &["xclip", "-selection", "clipboard"],
                    get: &["xclip", "-selection", "clipboard", "-o"],
                    needs_env: Some("DISPLAY"),
                },
                Tool {
                    set: &["xsel", "--clipboard", "--input"],
                    get: &["xsel", "--clipboard", "--output"],
                    needs_env: Some("DISPLAY"),
                },
                // Inside tmux with no display of its own to reach: `load-buffer -w`
                // sets the tmux buffer *and* forwards it to the outer terminal over
                // OSC 52, which is how a copy escapes an ssh + tmux session. Ordered
                // after the display tools (a local tmux on a desktop should still use
                // the real X/Wayland clipboard) and before this module's own OSC 52
                // fallback, because tmux's default `set-clipboard external` refuses
                // to pass an *application's* OSC 52 through — going via tmux is the
                // only write that lands.
                Tool {
                    set: &["tmux", "load-buffer", "-w", "-"],
                    get: &["tmux", "save-buffer", "-"],
                    needs_env: Some("TMUX"),
                },
            ]
        };
        candidates
            .iter()
            .find(|t| t.needs_env.is_none_or(env_is_set) && tool_exists(t.set[0]))
            .map(|t| SystemClipboard {
                set: t.set,
                get: t.get,
            })
    }
}

impl Clipboard for SystemClipboard {
    fn get(&self) -> Option<(String, bool)> {
        let out = Command::new(self.get[0])
            .args(&self.get[1..])
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
        let Ok(mut child) = Command::new(self.set[0])
            .args(&self.set[1..])
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

/// Whether environment variable `name` is set to something non-empty — the
/// "is there actually a display / a tmux server to talk to" test.
fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

// ===== OSC 52 ===============================================================

/// State shared between an [`Osc52Clipboard`] (which the editor tick writes
/// through, synchronously) and the [`EditHost`](crate::EditHost) (which pushes the
/// queued escapes to the client after the tick). Behind a `Mutex` because
/// [`Clipboard`] is `Send` — in practice both ends are the one server thread.
#[derive(Default)]
pub(crate) struct Osc52State {
    /// Escape sequences queued by [`Clipboard::set`], drained into `btv_ui_send`
    /// notifications by [`EditHost::flush_ui_sends`](crate::EditHost::flush_ui_sends).
    /// The editor tick cannot touch the transport itself, so the write is queued
    /// here and leaves with the frame it belongs to.
    pub(crate) pending: Vec<String>,
    /// What this session last put on the clipboard, so a paste has something to
    /// read. See [`Osc52Clipboard::get`] for why the terminal isn't asked.
    pub(crate) owned: Option<(String, bool)>,
}

/// The handle both ends hold.
pub(crate) type Osc52Handle = Arc<Mutex<Osc52State>>;

/// A clipboard that *is* the user's terminal: each write leaves as an OSC 52
/// escape the client emits, so the text lands on the machine running the terminal
/// emulator rather than the machine running the editor. This is what makes `"+y`
/// work over ssh, where no clipboard tool on the remote box could reach the
/// user's desktop.
pub(crate) struct Osc52Clipboard {
    state: Osc52Handle,
}

impl Osc52Clipboard {
    pub(crate) fn new(state: Osc52Handle) -> Osc52Clipboard {
        Osc52Clipboard { state }
    }
}

impl Clipboard for Osc52Clipboard {
    /// What this session last copied.
    ///
    /// OSC 52 *can* be read (`ESC]52;c;?`), but that is a round trip: the reply
    /// comes back through the client's input stream some unbounded time later,
    /// and most terminals refuse the read outright or prompt the user for it
    /// (it lets any program on a remote host exfiltrate the local clipboard).
    /// The seam here is synchronous, and blocking the editor on a terminal that
    /// may never answer is exactly the freeze the architecture forbids — so this
    /// answers from what we own, the same fast path neovim's provider takes for a
    /// selection it owns. Pasting text copied in *another* app is what the
    /// terminal's own paste (bracketed paste) is for.
    fn get(&self) -> Option<(String, bool)> {
        self.state.lock().unwrap().owned.clone()
    }

    fn set(&self, text: &str, linewise: bool) {
        let mut state = self.state.lock().unwrap();
        state.owned = Some((text.to_string(), linewise));
        state.pending.push(osc52_sequence(text));
    }
}

/// The OSC 52 "set clipboard" sequence for `text`: `ESC ] 52 ; c ; <base64> ESC \`.
///
/// Selection `c` is the system clipboard (what the user's Ctrl+V pastes); bemtvi's
/// `"+` and `"*` share one provider, so both land there rather than splitting `"*`
/// onto the X primary selection. `ESC \` (ST) terminates it — the form neovim
/// emits, and the one multiplexers pass through.
fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", BASE64.encode(text.as_bytes()))
}
