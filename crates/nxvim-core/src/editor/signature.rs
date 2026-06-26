//! Opt-in **signature-help auto-trigger** (the `(`/`,`-as-you-type popup).
//!
//! Manual signature help (`<C-k>` → `nx.lsp.signature_help`) is a one-shot request
//! that opens the *transient* doc float — dismissed by the very next key in
//! [`Editor::input`]. That model is wrong for an auto-trigger: the float would flash
//! away the instant you type the first argument. So this adds a small **session**:
//! while you are filling a call, the signature float is kept across keystrokes (a
//! *sticky* doc float), and the request is re-fired only when the active parameter can
//! change (a `,`) or the call can end (a close bracket / a deletion).
//!
//! The trigger characters are **the server's own** (`signatureHelpProvider.{trigger,
//! retrigger}Characters`, usually `(` and `,`), pushed in by the host when a server
//! that advertises them attaches *and* the user opted in. So
//! [`Editor::signature_trigger_chars`] is non-empty **iff** the feature is both enabled
//! and supported — that emptiness is the whole on/off gate, no separate flag needed.

use crate::input::{Key, KeyCode};

impl super::Editor {
    /// Whether the signature auto-trigger is live: the host pushed a non-empty trigger
    /// set (opted in + a server advertises them). Empty ⇒ the manual `<C-k>` path is
    /// the only way to signature help, exactly as before.
    pub fn signature_autotrigger_enabled(&self) -> bool {
        !self.signature_trigger_chars.is_empty()
    }

    /// Replace the active signature trigger set (the server's advertised chars, or
    /// empty to turn the auto-trigger off). Clearing it also ends any open session so a
    /// detach / a disable doesn't leave a sticky float behind.
    pub fn set_signature_trigger_chars(&mut self, chars: Vec<char>) {
        self.signature_trigger_chars = chars;
        if self.signature_trigger_chars.is_empty() {
            self.end_signature_session();
        }
    }

    /// React to an insert-mode keystroke once the edit has landed: decide whether to
    /// (re)fire signature help. Called from [`Editor::handle_insert`] after the char is
    /// inserted, so the request issues against the post-edit cursor.
    ///
    /// - a **trigger char** (`(`) starts — or keeps — a session and fires;
    /// - while a session is live, a `,` (next parameter) or a close bracket / a
    ///   backspace/delete (the call may have ended) re-fires so the float tracks the
    ///   active parameter and closes when you leave the call;
    /// - a plain argument character fires nothing — the active parameter hasn't moved,
    ///   so the sticky float just stays as it is.
    pub(crate) fn signature_after_insert(&mut self, key: &Key) {
        if !self.signature_autotrigger_enabled() || !self.mode.is_insert() {
            return;
        }
        match key.code {
            KeyCode::Char(c) if self.signature_trigger_chars.contains(&c) => {
                self.signature_session = true;
                self.signature_auto_request = true;
            }
            KeyCode::Char(')' | ']' | '}') if self.signature_session => {
                self.signature_auto_request = true;
            }
            KeyCode::Backspace | KeyCode::Delete if self.signature_session => {
                self.signature_auto_request = true;
            }
            _ => {}
        }
    }

    /// Whether a signature session is currently open — the host reads this to decide
    /// whether a signature float should be sticky and whether an empty reply should
    /// silently close it (auto) or echo "no signature" (the manual `<C-k>` path).
    pub fn signature_session_active(&self) -> bool {
        self.signature_session
    }

    /// End a signature session: drop the sticky-float protection and close the
    /// signature doc float. Idempotent — a no-op when no session is open. Called on an
    /// empty reply (you left the call), on leaving insert mode, and when the feature is
    /// turned off.
    pub fn end_signature_session(&mut self) {
        if !self.signature_session {
            return;
        }
        self.signature_session = false;
        self.close_doc_float(super::float::SIGNATURE_DOC_FLOAT);
    }
}
