//! The nxvim LSP client.
//!
//! Unlike the treesitter syntax worker (a separate, crash-isolated nxvim
//! *process*), the LSP client runs **in-process**, inside the server's runtime,
//! and spawns language servers as its own children (LSP Decision 1: a crashing
//! `rust-analyzer` just closes a pipe; it cannot segfault nxvim, so the
//! out-of-process guard the grammar worker needs buys nothing here).
//!
//! [`LspManager`] is the LSP analogue of `SyntaxClient`: a cheap handle the
//! server holds plus background tasks. It manages **N** child processes (one per
//! `(language, workspace-root)` [`ServerKey`]), each driven by its own
//! [`async_lsp`] client `MainLoop` task that owns that server's framing and
//! JSON-RPC id space. The manager bridges those per-server loops to the single
//! [`LspCommand`]/[`LspEvent`] channel pair the single-message-at-a-time editor
//! thread sees, so the editor never blocks on a language server (Decision 3).
//!
//! Heavy protocol deps (`async-lsp`, `lsp-types`, `serde_json`) live **only**
//! here; `nxvim-server` reaches them through this crate's surface. The
//! `lsp_types` re-export below is the exact version `async-lsp` builds against,
//! so the types the server constructs match the client API with no version skew.

mod log;
mod manager;
pub mod mock;

pub use lsp_types;
pub use manager::{
    LspEvent, LspManager, LspNotify, LspReply, LspRequest, PositionEncoding, ReqToken, ServerCaps,
    ServerKey, ServerSpawn,
};
