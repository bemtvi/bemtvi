# Plan: opt-in signature-help auto-trigger on the server's trigger chars

**Date:** 2026-06-26
**Status:** ✅ done (native verified via the mock LSP). The wasm edit-host shares the
ungated `EditHost` drain and compiles under `bemtvi-server --no-default-features`; the
emscripten artifact wasn't rebuilt here (workspace-excluded crate + a full disk), but
no edit-host-specific code changed.

## Goal

Auto-show LSP signature help **while typing a call** (e.g. after `print(`), opt-in,
driven by the **server-advertised** `signatureHelpProvider.triggerCharacters`
(usually `(` and `,`). Default-on in the python web demo
(`crates/bemtvi-edithost/web/demo-seed/init.lua`, basedpyright).

Today signature help is manual only (`<C-k>` → `btv.lsp.signature_help()`), and the
doc-float is *transient* — dismissed by the next key in `Editor::input`. So a naive
auto-fire would flash away as soon as you type the first argument. This adds a small
**signature session** so the float persists while you fill the call.

## Design

### Capability capture (`bemtvi-lsp`)
- `ProviderCaps` gains `signature_trigger_chars: Vec<String>`, populated in
  `provider_caps` from `signatureHelpProvider.{trigger,retrigger}Characters`.

### Core (`bemtvi-core`)
- New `editor/signature.rs`: `signature_trigger_chars: Vec<char>` (set only when the
  user opted in AND a server advertises them — non-empty ⟺ enabled+supported),
  `signature_session: bool`, `pub signature_auto_request: bool` (one-shot, drained by
  the host tick).
- Insert path (`insert.rs`): after an edit, `signature_after_insert(key)` —
  a trigger char starts/keeps a session and raises the request; `,`/close-bracket and
  backspace/delete refresh it while a session is live; plain arg chars don't fire (the
  active parameter only changes at commas), so the sticky float just stays.
- Float (`float.rs`): `SIGNATURE_DOC_FLOAT` const; `input()` keeps that one float
  during a session (`close_transient_doc_floats`) instead of dismissing it. The session
  ends — closing the float — on InsertLeave or an empty reply.

### Server (`bemtvi-server`, shared by native + wasm edit-host)
- `signature_auto: bool` flag set by `LspOp::SignatureAutoTrigger { enable }`.
- `ServerRuntime` gains `signature_trigger_chars: Vec<char>`; on `Initialized` we record
  the advertised chars; on attach (when `signature_auto`) we push them into core; on the
  last detach we clear them and end any session. Flipping the flag rescans attached
  servers.
- Host tick drains `signature_auto_request` → `request_lsp(SignatureHelp)`.
- `show_signature_help`: during a session, a non-empty reply opens a **sticky**
  signature float and an empty reply ends the session (closes it) silently; manual
  `<C-k>` keeps the existing transient float + "no signature" echo.

### Lua + demo
- `btv.lsp.signature_help_autotrigger(enable)` → `btv._signature_autotrigger(bool)` →
  `LspOp::SignatureAutoTrigger`.
- Web demo: `btv.lsp.signature_help_autotrigger(true)`.

## Tests
- `lsp_float.rs`: mock advertises `signatureHelpProvider.triggerCharacters`; with
  autotrigger on, typing `(` floats the signature; it survives the next key (sticky);
  off by default (typing `(` floats nothing); `<C-k>` still works.
