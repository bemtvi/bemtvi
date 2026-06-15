-- nxvim Lua prelude — the LSP verb surface (nx.lsp), MINIMAL slice.
-- This is a partial nx-lsp Phase A (docs/specs/2026-06-14-nx-lsp-design.md): only
-- the two position-family verbs whose replies render through the content float —
-- nx.lsp.buf.hover() and nx.lsp.buf.signature_help() — routed over the existing
-- nx._lsp_buf(kind) bridge. The full Phase A surface (config / enable /
-- definition / references / rename / code_action / …) lands later; until then a
-- server is started with the raw nx._lsp_start bridge (see examples/). Aliased
-- onto vim.lsp.buf.* per ADR 0002's muscle-memory whitelist.
local vim = vim
nx = nx or {}
nx.lsp = nx.lsp or {}
nx.lsp.buf = nx.lsp.buf or {}

-- The position-family request ids the bridge dispatches — kept in sync with
-- LspReqKind::as_u16 (crates/nxvim-server/src/lsp/mod.rs).
local KIND_HOVER = 5
local KIND_SIGNATURE_HELP = 6

-- nx.lsp.buf.hover(): request hover for the symbol under the cursor. The reply
-- opens the cursor-anchored content float (server-side, off the input path); an
-- empty reply echoes a brief message. Typically mapped to `K` in normal mode.
function nx.lsp.buf.hover()
  nx._lsp_buf(KIND_HOVER)
end

-- nx.lsp.buf.signature_help(): request signature help at the cursor. The reply
-- opens the content float with the active signature (and its active parameter in
-- brackets). Typically mapped in insert mode.
function nx.lsp.buf.signature_help()
  nx._lsp_buf(KIND_SIGNATURE_HELP)
end

-- vim.lsp.buf.* muscle-memory aliases (ADR 0002 whitelist).
vim.lsp = vim.lsp or {}
vim.lsp.buf = vim.lsp.buf or {}
vim.lsp.buf.hover = nx.lsp.buf.hover
vim.lsp.buf.signature_help = nx.lsp.buf.signature_help
