//! Keyboard macros — `<F2>{reg}` … `<F2>` recording and `<F3>{reg}` playback.
//!
//! The keys are vim's *behaviour* on bemtvi's own bindings: `q` and `@` stay free
//! (`q` is the close key half the read-only surfaces bind, and vim's `q` shadows
//! it for every user who never records a macro). `<F2>{reg}` starts a recording,
//! `<F2>` ends it, `<F3>{reg}` plays it back. The vim spelling is one map away —
//! `btv.keymap.set("n", "q", "<F2>")` / `btv.keymap.set("n", "@", "<F3>")`.
//!
//! # Where the keys come from
//!
//! Vim records the keys the user *typed* and re-applies mappings when the macro
//! plays back. bemtvi must do the same: so much of the editor is Lua keymaps
//! (the LSP `gd`/`K`, the completion triggers, every plugin) that recording the
//! keys *after* mapping resolution would capture **nothing** for a Lua-handler
//! map — the replay would silently skip the action.
//!
//! [`Editor::input`] only ever sees what the keymap matcher released, so it is
//! the wrong hook. The recording is fed instead by the server, which calls
//! [`Editor::note_macro_key`] at the three places a *typed* key reaches the
//! editor: a raw key the matcher released, the **LHS** of a mapping it fired,
//! and the literal-argument bypass (`f{char}`, `r{char}`, `"{reg}`, an
//! operator's motion). A mapping's RHS, `nvim_feedkeys` typeahead, `:normal`,
//! and playback itself are suppressed there, so what lands here is exactly what
//! the user pressed, once.
//!
//! The hook fires when a key **executes**, not when it was typed. That is what
//! makes the terminating `<F2>` the last key in the buffer even when the matcher
//! withheld it behind a live mapped prefix, so [`Editor::stop_recording`] can
//! simply pop it.
//!
//! # What a macro register holds
//!
//! bemtvi key **notation** (`ciwfoo<Esc>`), not vim's raw bytes: the editor
//! already speaks one lossless, readable encoding everywhere, and
//! [`key_to_notation`] round-trips through [`parse_keys`] (a typed `<` is
//! written `<lt>`). So a macro is an ordinary charwise register — it persists
//! through shada, lists in `:registers`, pastes with `"ap`, and can be authored
//! by hand — with no macro-specific plumbing anywhere else.

use super::command::is_macro_record_key;
use super::registers::RegKind;
use super::Editor;
use crate::input::{key_to_notation, parse_keys, Key};

/// A macro the editor has asked the **server** to play back: the register's keys,
/// already parsed, and how many times to run them.
///
/// Playback cannot recurse through [`Editor::input`] the way `:normal` does: the
/// recording holds the *LHS* of every mapping the user fired (see the module
/// docs), so the keys have to re-enter the keymap matcher — which lives one layer
/// up. So core resolves the register and hands the request over; the server owns
/// the drive loop (a stack, so a macro can call another).
#[derive(Debug, Clone)]
pub struct MacroPlay {
    /// The register being played, for `btv.macro.executing()`.
    pub reg: char,
    /// The register's contents, parsed from notation back into keys.
    pub keys: Vec<Key>,
    /// How many times to run them — `{count}<F3>a`.
    pub count: usize,
}

impl Editor {
    /// The register a recording is in flight for (`<F2>{reg}` … `<F2>`), or `None`.
    /// Read by the message line, the statusline segment, and `btv.macro`.
    pub fn recording_register(&self) -> Option<char> {
        self.macro_record.as_ref().map(|(reg, _)| *reg)
    }

    /// The register currently **playing back**, or `None` — vim's
    /// `reg_executing()`. Owned by core so every reader (Lua, a statusline, a
    /// plugin deciding to skip expensive work mid-macro) sees one answer, but set
    /// by the server, which is where the playback loop lives.
    pub fn executing_register(&self) -> Option<char> {
        self.macro_executing
    }

    /// Tell core which register is playing back (`None` when the stack empties).
    /// Called by the server's playback drive; see [`Editor::executing_register`].
    pub fn set_executing_register(&mut self, reg: Option<char>) {
        self.macro_executing = reg;
    }

    /// Note one **typed** key against an in-flight recording — the server's hook
    /// (see the module docs). A no-op when nothing is recording, so the call site
    /// stays a single unconditional line.
    pub fn note_macro_key(&mut self, key: Key) {
        if let Some((_, keys)) = self.macro_record.as_mut() {
            keys.push(key);
        }
    }

    /// Begin recording into `reg` (`<F2>{reg}`). An uppercase name **appends** to
    /// the lowercase register (vim's `qA`); the append is applied when the recording
    /// stops, so the register keeps its old contents until then.
    pub(crate) fn start_recording(&mut self, reg: char) {
        if self.macro_record.is_some() {
            // Unreachable through the grammar (an `<F2>` while recording stops it), but
            // a Lua caller could ask; refuse loudly rather than lose the recording.
            self.echo("E872: Already recording");
            return;
        }
        self.macro_record = Some((reg, Vec::new()));
        self.echo(String::new());
    }

    /// Stop the in-flight recording and commit it to its register (`<F2>`).
    ///
    /// The terminating `<F2>` itself was already noted by the server hook (it is a
    /// typed key like any other), so it is popped here — the one place that knows
    /// it is not part of the macro. An uppercase register name appends.
    pub(crate) fn stop_recording(&mut self) {
        let Some((reg, mut keys)) = self.macro_record.take() else {
            return;
        };
        // Drop the `<F2>` that ended the recording. It is the last key noted — the
        // hook fires when a key *executes*, and this runs from that key's own
        // dispatch — unless the stop came from somewhere the hook does not feed (a
        // macro playing back an `<F2>`, where the last noted key belongs to the
        // user, not to this stop). Checking the key itself keeps both honest.
        if keys.last().copied().is_some_and(is_macro_record_key) {
            keys.pop();
        }
        let text: String = keys.iter().map(|&k| key_to_notation(k)).collect();
        let append = reg.is_ascii_uppercase();
        let name = reg.to_ascii_lowercase();
        self.registers.set_api(name, text, RegKind::Char, append);
        self.echo(String::new());
    }
}

impl Editor {
    /// Take the playback the last `<F3>{reg}` asked for, if any. The server polls
    /// this after every key it dispatches, so a macro that plays another macro
    /// simply queues the next frame.
    pub fn take_macro_play(&mut self) -> Option<MacroPlay> {
        self.macro_play.take()
    }

    /// `{count}<F3>{reg}` — resolve a register into a playback request.
    ///
    /// `reg` is `None` for `<F3><F3>` ("the last register played"). The `:`
    /// register is special-cased the way vim's `@:` is: its contents are the last
    /// **ex command**, so they are replayed as `:{cmd}<CR>` rather than as
    /// normal-mode keys.
    pub(crate) fn play_macro(&mut self, reg: Option<char>, count: usize) {
        let Some(name) = reg.or(self.macro_last_played) else {
            self.echo("E748: No previously used register");
            return;
        };
        let Some((text, _kind)) = self.register_text(Some(name)) else {
            // An empty register is an empty macro: nothing to run, nothing to say.
            self.macro_last_played = Some(name);
            return;
        };
        let keys = if name == ':' {
            parse_keys(&format!(":{text}<CR>"))
        } else {
            parse_keys(&text)
        };
        self.macro_last_played = Some(name);
        if keys.is_empty() {
            return;
        }
        self.macro_play = Some(MacroPlay {
            reg: name,
            keys,
            count,
        });
    }
}

impl Editor {
    /// Signal that this keystroke **failed** — the place vim would beep: a motion
    /// that could not move, an unmatched `f{char}`, an empty register, an error
    /// message. bemtvi has no bell, so nothing is heard; what the flag buys is the
    /// one thing the silence used to cost us, which is a macro that knows when to
    /// stop. Cleared at the top of every [`Editor::input`].
    ///
    /// Every `E###` message routes here on its own (see [`Editor::echo`]), so the
    /// explicit calls are only for the *silent* failures.
    pub(crate) fn beep(&mut self) {
        self.command_failed = true;
    }

    /// Take the "this keystroke failed" flag. The server calls it after each key
    /// of a macro playback: a failure drops the remaining repeats and every
    /// suspended frame, which is vim's rule and the reason `100<F3>a` is safe to
    /// type.
    pub fn take_command_failed(&mut self) -> bool {
        std::mem::take(&mut self.command_failed)
    }
}
