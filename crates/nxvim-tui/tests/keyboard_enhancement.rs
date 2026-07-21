//! Tier 1: the kitty-keyboard-protocol guard pushes the enhancement flags on
//! creation and **pops them on drop** — including on a panic unwind. ratatui's
//! panic hook restores raw mode and the alternate screen but knows nothing about
//! the keyboard protocol, so a panic in the event loop must not leave a terminal
//! stuck in progressive-enhancement mode (which changes how every subsequent
//! keystroke is encoded in the user's shell). Black-box: drives the public
//! `KeyboardEnhancement` guard against an in-memory sink and inspects the bytes.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use nxvim_tui::KeyboardEnhancement;

/// A `Write` whose bytes outlive the writer, so we can inspect what a guard
/// emitted after it has been dropped.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The exact bytes crossterm emits for a command, produced through the same
/// `execute!` path the guard uses so the comparison is byte-for-byte.
fn sequence(cmd: impl crossterm::Command) -> Vec<u8> {
    let mut buf = Vec::new();
    crossterm::execute!(buf, cmd).unwrap();
    buf
}

fn push_sequence() -> Vec<u8> {
    sequence(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
    ))
}

fn pop_sequence() -> Vec<u8> {
    sequence(PopKeyboardEnhancementFlags)
}

#[test]
fn pushes_disambiguate_flag_on_enable() {
    let buf = SharedBuf::default();
    let _guard = KeyboardEnhancement::push(buf.clone());
    let emitted = buf.0.lock().unwrap().clone();
    assert!(
        emitted.starts_with(&push_sequence()),
        "constructing the guard must push the DISAMBIGUATE_ESCAPE_CODES flag"
    );
}

#[test]
fn pops_flags_on_normal_drop() {
    let buf = SharedBuf::default();
    {
        let _guard = KeyboardEnhancement::push(buf.clone());
    } // guard dropped at end of scope
    let emitted = buf.0.lock().unwrap().clone();
    assert!(
        emitted.ends_with(&pop_sequence()),
        "leaving the guard's scope must pop the keyboard enhancement flags"
    );
}

#[test]
fn pops_flags_on_panic_unwind() {
    let buf = SharedBuf::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = KeyboardEnhancement::push(buf.clone());
        panic!("event loop blew up");
    }));
    assert!(result.is_err(), "the panic should have propagated");
    let emitted = buf.0.lock().unwrap().clone();
    assert!(
        emitted.ends_with(&pop_sequence()),
        "a panic in the guarded scope must still pop the enhancement flags"
    );
}
