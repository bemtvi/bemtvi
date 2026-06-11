//! LSP integration tests, end to end through the real stack: the in-process
//! server spawns the **real** `nxvim` binary as a scripted mock language server
//! (`--__lsp-mock`, selected via `NXVIM_LSP_CMD`), which speaks real LSP over
//! stdio and records every message it receives to a file. LSP replies are
//! asynchronous, so these tests **poll** the record file / buffer state (bounded
//! wait) until the expected message arrives, exactly like the syntax tests poll
//! redraws.
//!
//! These tests spawn subprocesses and share process-global env, so they
//! serialize on a single lock.
//!
//! The generic black-box harness comes from the shared `nxvim-test-harness`
//! crate; the LSP-specific helpers (mock configuration, record polling, the
//! LSP-aware redraw/panel accessors) live in `support`. Each phase has its own
//! submodule.
//!
//! This file is the integration-test crate root, so submodules under `lsp/` are
//! addressed with `#[path]` (the root's module path prefix is the `tests/`
//! directory itself, not `tests/lsp/`).

#[path = "lsp/support.rs"]
mod support;

#[path = "lsp/attach.rs"]
mod attach;
#[path = "lsp/buf_api.rs"]
mod buf_api;
#[path = "lsp/client_api.rs"]
mod client_api;
#[path = "lsp/completion.rs"]
mod completion;
#[path = "lsp/daemon.rs"]
mod daemon;
#[path = "lsp/diagnostic_api.rs"]
mod diagnostic_api;
#[path = "lsp/diagnostics.rs"]
mod diagnostics;
#[path = "lsp/formatting.rs"]
mod formatting;
#[path = "lsp/goto.rs"]
mod goto;
#[path = "lsp/inlay.rs"]
mod inlay;
#[path = "lsp/lifecycle.rs"]
mod lifecycle;
#[path = "lsp/real_server.rs"]
mod real_server;
#[path = "lsp/semantic.rs"]
mod semantic;
