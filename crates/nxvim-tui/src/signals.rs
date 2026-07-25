//! The TUI's half of the fatal-signal handling: what to hand the terminal back as,
//! and how to wake this client's event loop.
//!
//! The machinery lives in [`nxvim_view::signals`] (it is shared with the GUI client)
//! — read that module first; this one only supplies the two client-specific pieces:
//!
//! - the **escape sequence** the hard path writes from inside the signal handler,
//!   pre-rendered here from the very same crossterm commands this client's RAII
//!   guards emit on drop ([`MouseCapture`](crate::MouseCapture),
//!   [`BracketedPaste`](crate::BracketedPaste),
//!   [`KeyboardEnhancement`](crate::KeyboardEnhancement),
//!   [`CursorStyleGuard`](crate::CursorStyleGuard)) and in the same order
//!   [`run`](crate::run) drops them, so the two paths cannot drift apart;
//! - the **wake-up**, as a [`ShutdownSignal`] the event loop can `select!` on.

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::style::ResetColor;
use crossterm::terminal::LeaveAlternateScreen;

pub use nxvim_view::signals::{exit_as_signal_if_killed, shutdown_requested};

/// Install the fatal-signal handlers, returning the event loop's shutdown wake-up.
///
/// Call **before** the terminal is put into raw mode / the alternate screen (see
/// [`run`](crate::run)), so the termios captured is the user's original cooked-mode
/// one. Only the first call installs anything.
pub fn install() -> ShutdownSignal {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    nxvim_view::signals::install(nxvim_view::signals::Config {
        restore_sequence: restore_sequence(),
        restore_termios: true,
        // Called from the watchdog thread; an unbounded send never blocks, and the
        // event loop picks it up on its next turn.
        on_shutdown: Box::new(move || {
            let _ = tx.send(());
        }),
    });
    ShutdownSignal { rx }
}

/// Everything [`run`](crate::run) undoes on the way out, as bytes: mouse reporting,
/// bracketed paste, the kitty keyboard flags and the cursor shape first, then the
/// alternate screen last.
fn restore_sequence() -> Vec<u8> {
    let mut seq: Vec<u8> = Vec::new();
    // Infallible: these only format ANSI into an in-memory buffer.
    let _ = crossterm::execute!(
        seq,
        DisableMouseCapture,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        SetCursorStyle::DefaultUserShape,
        ResetColor,
        Show,
        LeaveAlternateScreen,
    );
    seq
}

/// The wake-up [`install`] hands the event loop: fires once, when a signal has asked
/// for a graceful shutdown.
pub struct ShutdownSignal {
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl ShutdownSignal {
    /// Resolve when a graceful shutdown has been requested. Cancel-safe (it is
    /// awaited from a `tokio::select!` arm), and pends forever where no signal can
    /// arrive — including on a closed channel, which can't mean "shut down" and must
    /// not spin the select arm.
    pub async fn recv(&mut self) {
        if self.rx.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
}
