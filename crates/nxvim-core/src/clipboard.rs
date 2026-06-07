//! The system-clipboard seam (the `"+` and `"*` registers).
//!
//! `nxvim-core` defines only the *interface*; the implementation — a real OS
//! clipboard (`arboard`) or, in tests, an in-memory fake — is injected from the
//! server, exactly like [`crate::SyntaxEngine`]. This preserves core's invariant
//! (no I/O, no async, no platform code) while letting the editor route `"+y` /
//! `"+p` through whatever provider the front end supplies.
//!
//! A front end with **no** provider (a bare-core test, or a headless box whose
//! platform backend failed to start) leaves the editor's clipboard `None`;
//! selecting `"+` / `"*` then errors loudly rather than silently falling back to
//! the unnamed register.

/// Synchronous access to the host's clipboard, for the `"+` / `"*` registers.
///
/// The `bool` in each signature is *linewise*: `true` when the text pastes as
/// whole lines, `false` for charwise — the same flag the rest of the public
/// register surface ([`crate::Editor::register_mirror`]) crosses the crate
/// boundary with. `Send`, because the server owns it on its own thread.
pub trait Clipboard: Send {
    /// The clipboard's current contents as `(text, linewise)`, or `None` when it
    /// is empty or unreadable (paste then does nothing).
    fn get(&self) -> Option<(String, bool)>;
    /// Replace the clipboard's contents.
    fn set(&self, text: &str, linewise: bool);
}
