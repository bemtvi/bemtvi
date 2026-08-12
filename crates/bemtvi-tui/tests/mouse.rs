//! Tier 1: the mouse-capture guard disables mouse reporting on drop — including
//! on a panic unwind. ratatui's panic hook restores raw mode and the alternate
//! screen but leaves mouse mode on, so a panic in the event loop must not leak
//! mouse-reporting escape codes into the user's shell. Black-box: drives the
//! public `MouseCapture` guard against an in-memory sink and inspects the bytes.

use std::io::Write;
use std::sync::{Arc, Mutex};

use bemtvi_tui::MouseCapture;
use crossterm::event::DisableMouseCapture;

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

/// The exact bytes crossterm emits for `DisableMouseCapture` on this platform,
/// produced through the same path the guard uses so the comparison is exact.
fn disable_sequence() -> Vec<u8> {
    let mut buf = Vec::new();
    crossterm::execute!(buf, DisableMouseCapture).unwrap();
    buf
}

#[test]
fn mouse_capture_disabled_on_normal_drop() {
    let buf = SharedBuf::default();
    {
        let _guard = MouseCapture::enable(buf.clone());
    } // guard dropped at end of scope

    let emitted = buf.0.lock().unwrap().clone();
    assert!(
        emitted.ends_with(&disable_sequence()),
        "leaving the guard's scope must emit DisableMouseCapture"
    );
}

#[test]
fn mouse_capture_disabled_on_panic_unwind() {
    let buf = SharedBuf::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = MouseCapture::enable(buf.clone());
        panic!("event loop blew up");
    }));
    assert!(result.is_err(), "the panic should have propagated");

    let emitted = buf.0.lock().unwrap().clone();
    assert!(
        emitted.ends_with(&disable_sequence()),
        "a panic in the guarded scope must still disable mouse capture"
    );
}
