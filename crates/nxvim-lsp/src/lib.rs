//! The nxvim LSP client.
//!
//! Unlike the treesitter syntax worker (a separate, crash-isolated nxvim
//! *process*), the LSP client runs **in-process**, inside the server's runtime,
//! and spawns language servers as its own children (LSP Decision 1: a crashing
//! `rust-analyzer` just closes a pipe; it cannot segfault nxvim, so the
//! out-of-process guard the grammar worker needs buys nothing here).
//!
//! [`LspManager`] is a cheap handle the server holds plus background tasks. It
//! manages **N** child processes (one per
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

// `client` holds both the pure capability/handshake helpers (always compiled —
// the sync wasm client reuses them) and the async-lsp `MainLoop`/`Router` (gated
// to `native` inside the module). `convert`, `log`, and `protocol` are plain data
// / pure transforms, wasm-safe.
mod client;
mod convert;
mod log;
mod protocol;

// The async client surface — tokio + `async-lsp`. Gated to `native`; absent in the
// browser build (`wasm32-unknown-emscripten`), which uses the sync client below.
#[cfg(feature = "native")]
mod dispatch;
#[cfg(feature = "native")]
mod manager;
#[cfg(feature = "native")]
pub mod mock;
#[cfg(feature = "native")]
mod transport;

// The browser edit-host's synchronous, byte-driven LSP client (Phase 6e): the
// no-tokio analogue of `manager` + the async half of `client`/`dispatch`, driving
// language servers over the daemon's raw `lsp_*` stdio wire. Compiled only when
// `native` is off (the wasm build), so the native build never carries it as dead
// code.
#[cfg(not(feature = "native"))]
mod sync_client;

pub use lsp_types;
// The editor↔manager data types (`crate::protocol`) and the one normalization
// helper `nxvim-server` reuses directly (`crate::convert`). `LspRequest::Raw`/
// `LspReply::Raw` (Phase 5) carry raw `serde_json::Value`s, so the `serde_json`
// re-export below lets downstream crates name the type without a direct dependency
// on the protocol JSON layer. All wasm-safe (plain data / pure transforms).
pub use convert::{normalize_workspace_edit_value, try_normalize_workspace_edit_value};
pub use protocol::{
    ApplyEditOutcome, CapabilityRegistration, ChangeAnnotationData, CodeActionData,
    CompletionItemData, FoldRangeData, InlayHintData, LspEvent, LspNotify, LspReply, LspRequest,
    PositionEncoding, ProgressKind, ProgressUpdate, ProviderCaps, RefreshKind, ReqToken,
    SemanticLegend, SemanticTokensData, ServerCaps, ServerKey, ServerSpawn, SignatureInfo,
    SymbolData, WorkspaceChange, WorkspaceEditData,
};
pub use serde_json;

// The native async manager + transport seam: the manager spawns each server
// through the transport (`nxvim-server` injects a daemon-backed one for the
// edit-host split; the default is a real local child).
#[cfg(feature = "native")]
pub use manager::LspManager;
#[cfg(feature = "native")]
pub use transport::{ExitFuture, LocalLspTransport, LspChannel, LspProcess, LspTransport};

// The browser sync client (Phase 6e), the `LspManager` analogue the wasm edit-host
// drives in place of the async manager.
#[cfg(not(feature = "native"))]
pub use sync_client::{SyncLspClient, WireOp};
