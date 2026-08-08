//! The Lua runtime and the beginnings of the `vim.*` standard library.
//!
//! nxvim embeds vendored PUC Lua 5.4 (the single backend, via mlua's `lua54`
//! feature — LuaJIT was dropped, matching the wasm build). Scripts run inside
//! the *server*, exactly as in neovim, and influence the editor through the same
//! mechanisms RPC clients use.
//!
//! The surface is split in two: editor-touching functions are installed from
//! Rust ([`install`] — `vim.cmd`, `vim.api.nvim_command`/`nvim_echo`/`nvim_set_hl`,
//! `vim.fn.*`, and the `print` capture), while the broad pure-Lua part of
//! `vim.*` — the table / list / string helpers, `vim.g`/`vim.o`/`vim.opt`/
//! `vim.env`, `vim.iter`, and the registration APIs (`nvim_create_user_command`,
//! `nvim_create_autocmd`, the `nx._fire` autocmd dispatcher) — lives in the
//! `src/prelude/` Lua modules, loaded in order at init. The data-flow stays "Lua -> queued
//! commands / output / highlights -> core mutation": effects are buffered in
//! `Shared` ([`runtime`]) and drained by the server after each chunk.
//!
//! Module layout:
//! - [`ops`] — the plain-data ops/types the server drains and the mirror payloads it pushes.
//! - [`runtime`] — [`LuaRuntime`]: the VM, the `Shared` effect buffer, and the Rust-facing API.
//! - [`install`] — installing the `vim.*` Rust bridge into a fresh VM.
//! - [`convert`] — the Lua↔`rmpv`/`serde_json` value bridges and opts readers.
//! - [`host`] — filesystem / process / glob / standard-path host primitives.
//! - [`luafs`] — the project-facing Lua filesystem seam ([`LuaFs`]).

mod convert;
mod fswire;
mod gitwire;
mod glob;
mod host;
mod httpwire;
mod install;
mod luafs;
mod ops;
mod runtime;
mod search;
mod vimregex;

pub use fswire::{fs_job_from_value, fs_job_to_value, fs_result_from_value, fs_result_to_value};
pub use gitwire::{
    git_job_from_value, git_job_to_value, git_result_from_value, git_result_to_value,
};
pub use host::stdpath;
pub use httpwire::{
    http_request_from_value, http_request_to_value, http_result_from_value, http_result_to_value,
};
pub use luafs::{run_fs_job, FileKind, LuaDirEntry, LuaFs, LuaStat, StdLuaFs};
pub use ops::{
    build_server_request, mount_root_redirect, split_mount_path, BufOp, CallbackArgs, ConfirmReq,
    DecorInvalidate, DecorMark, DecorPublish, DiagnosticData, DockOp, ExtmarkOp, FeedKeysOp,
    FsError, FsJob, FsValue, GitError, GitHunk, GitJob, GitStatusEntry, GitValue, GlobalOptionOp,
    HlSet, HttpError, HttpMountError, HttpRequest, HttpResponse, HttpServerReply,
    HttpServerRequest, InlayHintMirrorData, LayerOp, LoopOp, LspClientData, LspOp, LspProgressData,
    LspServerCapabilities, NamedListOp, OpenTarget, OptionValue, PanelOp, QfItem, QfSetOp,
    RawKeymap, RawRhs, RegisterSetOp, SemanticTokenData, StatuslineKind, StatuslineTarget, TabOp,
    TerminalOpenReq, TsOp, UiInputReq, ViewOp, VirtChunkData, VirtDecorData, WindowOp,
};
pub use runtime::{
    extract_prelude, BoGlobalMirror, BoMirror, BufBytesEdit, BufMirror, CmdlineComplete,
    ExtmarkMirror, FloatMirror, GoMirror, HlDefMirror, JumpMirror, LinesDelta, LuaRuntime,
    OptionCatalogRow, QfMirror, TabMirror, WindowMirror, WoGlobalMirror,
};
