# Multi-encoding support & resilience to invalid UTF-8

> **Status: DONE for the native + daemon paths (2026-06-15).** Phase 0 (foundation)
> and Phase 1 (options) landed 2026-06-14; Phases 2 (read seam), 3 (write seam), and
> 4 (tests/example/docs) landed 2026-06-15. The one remaining gap is the **wasm read
> path** (the browser decodes OPFS bytes to text in JS before they cross the FFI) —
> see "Remaining work" at the bottom; wasm *writes* already encode correctly.
>
> ### Key deviation from the original design: **no PUA escape.**
> The plan sketched a Supplementary-PUA-A escape (`U+F0000 + b`) to round-trip
> undecodable bytes. It turned out to be **unnecessary**: `encoding_rs`'s
> windows-1252 (our `latin1` terminal fallback) is a *total, bijective* single-byte
> codec — all 256 byte values decode to distinct scalars (even `0x81`/`0x8d`/… map to
> pass-through C1 controls, never `U+FFFD`) and encode back exactly (verified over
> all 256). So "strict decode of each `'fileencodings'` entry + windows-1252 total
> fallback + exact encode-back" already guarantees byte-identical round-trips, with
> no escape layer. PUA couldn't have helped the only residual lossy case anyway
> (lone surrogates in BOM'd UTF-16 — which can't be stored in a Rust `String`). This
> keeps the seam simpler; the "PUA bijection" test became a "valid-UTF-8 SPUA scalar
> round-trips untouched" regression guard.
>
> ### Second deviation: **UTF-16 is encoded by hand on write.**
> `encoding_rs::encode` cannot *emit* UTF-16 (its `output_encoding` is UTF-8 for the
> UTF-16 families), so `encode_from_str` writes UTF-16LE/BE code-unit by code-unit
> itself (decode still goes through `encoding_rs`). The BOM is re-emitted explicitly.

## Why this document exists

nxvim is **UTF-8-only at the I/O boundary**, and the two read paths disagree:

- The local read path (`Buffer::from_file`, `crates/nxvim-core/src/buffer.rs:264`)
  does `Rope::from_reader(...)?`, which **errors on the first invalid byte**. A
  file with a few bad bytes — or any latin1 / utf-16 file — refuses to open and
  falls back to an empty *named* buffer (`Buffer::named`, `buffer.rs:227`; see the
  test `unreadable_startup_file_keeps_its_name_and_echoes_the_error`,
  `crates/nxvim-server/tests/editing/core_editing.rs:10`).
- The daemon read path (`crates/nxvim-server/src/lifecycle.rs:35`) silently does
  `String::from_utf8_lossy`. So the *same file* behaves differently local vs.
  remote, and lossy decode **destroys the original bytes** on the next `:w`.

We want two things, decided with the requester:

1. **Multi-encoding** — read latin1 / utf-16 / BOM'd files, remember the source
   encoding per buffer, and convert back on write (vim's `fileencoding`).
2. **Round-trip-safe resilience** — a file with undecodable bytes opens and stays
   editable, and `:w` reproduces the original bytes **exactly** (no silent
   corruption — the project's fail-loud rule).

The internal text model is a `ropey::Rope` (UTF-8 only, byte-offset indexed) and
everything downstream — grapheme/width (`unicode.rs`), the vendored vim regex
engine (`nxvim-regex`), LSP position math (`lsp/mod.rs:480`) — depends on that.
So the rope **stays UTF-8**, exactly as neovim keeps its internal encoding UTF-8.
All conversion lives at the byte↔str seam.

The guiding principle, as elsewhere in nxvim: **fail loud.** An unrepresentable
character on write aborts with a named error rather than mangling the file.

This plan is divided into self-contained phases. Each is sized to be picked up in
a single focused session. Later phases assume earlier ones landed.

---

## Design decisions (resolved up front)

- **Rope stays UTF-8.** Conversion happens only at the read/write seam.
- **Round-trip-safe invalid bytes via PUA escape.** Each undecodable byte `b`
  maps to scalar `U+F0000 + b` (Supplementary PUA-A, range `U+F0000..=U+F00FF`).
  On write these map back to the raw byte, reproducing the original bytes exactly.
  - SPUA-A is chosen over the BMP PUA (`U+E000`): real files routinely use the
    `U+E000` block for icon fonts (Nerd Fonts / Powerline), so escaping there
    would corrupt them. SPUA-A is virtually never present in real text.
  - The escape must be **bijective over the byte stream**: on the lossy decode
    path, a *legitimately present* `U+F00xx` scalar must itself be escaped, or
    open→write corrupts it. (No range is provably collision-free — lone
    surrogates, the only safe choice, cannot be stored in a Rust `String` — so we
    minimize, not eliminate, the risk.)
- **One conversion implementation, shared by every path.** Pure helpers live in
  `nxvim-core` (transcoding is pure CPU and is part of "Buffer file read/write",
  so it respects the pure-sync-core rule in CLAUDE.md). Local-sync, daemon, and
  wasm read paths all funnel through the same decoder; all write paths through the
  same encoder. **No behavior fork.**
- **`fileencodings` default `["ucs-bom", "utf-8", "latin1"]`** (neovim's is
  `ucs-bom,utf-8,default,latin1`). BOM sniff first via
  `encoding_rs::Encoding::for_bom`; then strict utf-8; then latin1 (decoded as
  `windows-1252`, browser-style) as the always-succeeds terminal fallback.
- **Fail loud on unmappable write.** Writing to a non-UTF-8 `fileencoding` where
  a char isn't representable aborts the write with a named error
  (`E513: conversion failed (cannot represent U+20AC '€' in latin1)`).
  `encoding_rs`'s default — emitting HTML numeric character references — is silent
  corruption and **must be intercepted**. PUA-escaped chars are unescaped to raw
  bytes *before* transcoding, so bytes that came from the file always round-trip
  regardless of the target encoding.

---

## Phase 0 — Dependency + core conversion module  ✅ (foundation only)

> **Landed (partial):** the dependency and the `Encoding` name/registry type
> (`crates/nxvim-core/src/encoding.rs`: `Encoding`, `from_label`, `Display` →
> vim spelling, `is_fileencodings_entry`) are in. The transcode helpers
> (`decode_to_rope` / `encode_from_str` + PUA escape) are **not** yet — they land
> with the read/write seams in Phases 2–3, which is where they're first used.
>
> **Deviation from the original plan:** `encoding_rs` is added to `nxvim-core` as
> a *plain, always-on* dependency, **not** behind a feature gate. It is pure Rust
> (no C), compiles for `wasm32-unknown-emscripten`, and pulls in no native-only
> deps — verified: `cargo build -p nxvim-core --no-default-features` and
> `cargo build -p nxvim-server --no-default-features` both compile. Feature-gating
> would have forced `#[cfg]` on the `BufferOptions`/`Options` fields and every
> mirror, for no benefit. `simd-accel` stays off (its default).

**Goal:** the pure transcode helpers exist and are unit-exercised through a thin
black-box path; nothing is wired into open/save yet.

- Add `encoding_rs` (pure Rust; Firefox's encoding library) to root `Cargo.toml`
  `[workspace.dependencies]` pinned `=x.y.z`; pull into `nxvim-core` via
  `<dep>.workspace = true`. **Disable the `simd-accel` feature** (it needs
  nightly). Gate behind a **default-on** core feature so the
  `--no-default-features` edithost build still pulls it in (mirror the existing
  `vim-regex` / `serde` feature pattern in `crates/nxvim-core/Cargo.toml`).
- New `crates/nxvim-core/src/encoding.rs`:
  - `decode_to_rope(bytes, fileencodings) -> (String, Encoding, bool /*bomb*/)`
    — BOM sniff → try the `fileencodings` list in order → transcode to UTF-8,
    applying the **bijective** PUA escape on the lossy path.
  - `encode_from_str(&str, enc, bomb) -> Result<Vec<u8>>` — PUA-unescape →
    transcode → prepend BOM when `bomb`; **error loudly** on any unmappable char,
    naming the char and its position.
  - An `Encoding` wrapper over `encoding_rs::Encoding` that `Display`s to a
    vim-style name (`utf-8`, `latin1`, `utf-16le`, …) and parses from one.

**Dependencies:** none.
**Verifiable when:** `cargo build`, both feature configs build, and the
emscripten target (`wasm32-unknown-emscripten`) builds with the feature on.

## Phase 1 — Buffer options & `:set` / `vim.bo` wiring  ✅ DONE (2026-06-14)

> **Landed.** `:set fileencoding=…` / `fenc`, `:set fileencodings=…` / `fencs`,
> `:set bomb`, and the `vim.bo.fileencoding` / `vim.bo.bomb` / `vim.o.fileencodings`
> equivalents are accepted, validated (E474 on a bad label, fail-loud), and read
> back through `:set …?` and the Lua mirrors. Setting `fenc` marks the buffer
> modified. No I/O effect yet (Phases 2–3). Covered by 10 black-box tests in
> `crates/nxvim-server/tests/editing/encoding.rs`; full workspace green.

**Goal:** `:set fileencoding=…`, `:set fileencodings=…`, `:set bomb`, and the
`vim.bo` equivalents are accepted and round-trip through `:set fenc?`. No effect
on I/O yet (still wired in Phases 2–3).

Model everything on the existing `regexsyntax` enum-string buffer option:

- `crates/nxvim-core/src/options.rs`: `BufferOptions` (struct `:220`, default
  `:259`) gains `fileencoding` and `bomb: bool`; global `Options` gains
  `fileencodings: Vec<String>`. Add canonical names (`:527` region):
  `fileencoding`/`fenc`, `fileencodings`/`fencs`, `bomb`.
- `crates/nxvim-core/src/editor/options.rs` `apply_set_str` (`:178`, enum branch
  at `:208`): validate `fileencoding` against known encodings; parse
  `fileencodings`; toggle `bomb`. Setting `fenc` marks the buffer modified (it
  implies a re-encode next write), matching vim.
- `crates/nxvim-core/src/editor/windows.rs:1073` and the `_buf_set_option` bridge
  in `crates/nxvim-server/src/effects.rs` (around `:400`): forward
  `vim.bo.fileencoding` / `vim.bo.bomb`.

**Dependencies:** Phase 0 (for the encoding name parse/validate).

## Phase 2 — Read seam: unify on bytes (resilience lands here)  ✅ DONE (2026-06-15)

> **Landed.** `Buffer::from_file` now reads raw bytes and decodes through
> `decode_to_rope` (threading `'fileencodings'` from every call site; the two
> `Editor` constructors use `encoding::DEFAULT_FILEENCODINGS`). `Editor::load_bytes_into`
> sits beside `load_str_into` and the daemon read path (`apply_open` →
> `load_replica_bytes`) routes raw bytes through it — the `from_utf8_lossy` fork is
> gone. `:e!` reload re-runs the decode via `from_file`. The **wasm** read path still
> takes a JS-decoded string (see "Remaining work").

**Goal:** every read path decodes through `decode_to_rope`; invalid-UTF-8 and
non-UTF-8 files **open** and carry their detected `fileencoding`/`bomb`. Local and
daemon agree.

- `crates/nxvim-core/src/buffer.rs` `from_file` (`:256`): read raw bytes →
  `decode_to_rope` → store `fileencoding`/`bomb` → `ensure_trailing_newline` →
  stat as today. (Forfeits the current streaming `from_reader` 1×-memory open;
  ~2× peak at open. Optionally keep `from_reader` for the detected-clean-utf-8
  fast path.)
- `crates/nxvim-core/src/editor/buffers.rs`: add `load_bytes_into(buffer, name,
  bytes, &fileencodings)` beside `load_str_into` (`:454`) — decodes via the core
  helper, sets encoding state, roots undo on the converted text. Keep
  `load_str_into` for genuinely-already-str callers (scratch buffers).
- `crates/nxvim-server/src/lifecycle.rs` `apply_open`/`load_replica` (`:34`/`:65`):
  pass the raw `bytes` to `load_bytes_into` instead of `from_utf8_lossy`. Do the
  same for the wasm replica loader. This removes the local/daemon fork.
- Ensure the `:e!` reload path re-runs the same decode (the undo root must match
  the decoded text).

**Dependencies:** Phases 0–1.
**Behavior change:** the existing
`unreadable_startup_file_keeps_its_name_and_echoes_the_error` test changes
meaning — such a file now *opens* (and round-trips in Phase 3). Update it.

## Phase 3 — Write seam: encode, fail loud, BOM, byte count  ✅ DONE (2026-06-15)

> **Landed.** `Buffer::to_save_bytes` now returns `Result<Vec<u8>>` and encodes via
> `encode_from_str` (BOM re-emit on `'bomb'`, UTF-16 by hand); `Buffer::write`
> encodes *before* touching disk and reports the **encoded** byte count.
> `enqueue_save`/`enqueue_save_of` return `Option<u64>` — an unrepresentable char
> echoes `E513` and enqueues nothing (so a `:wq`'s deferred quit never fires, file
> untouched, buffer stays dirty). The daemon and wasm off-tick saves share
> `to_save_bytes`, so both write surfaces are covered by the one change.

**Goal:** `:w` encodes the rope back to `fileencoding`, re-emits the BOM, and
reproduces original bytes exactly for resilience buffers; unrepresentable chars
abort loudly.

- `crates/nxvim-core/src/buffer.rs` `to_save_bytes` (`:652`) and `write` (`:628`):
  replace `self.text.to_string().as_bytes()` with `encode_from_str(rope,
  fileencoding, bomb)`. The daemon (`to_save_bytes`) and wasm (`eh_save_bytes`)
  write paths already route through `to_save_bytes`, so fixing it once covers
  both write surfaces.
- `:w` byte-count message must report the **encoded** byte count (what's on disk),
  computed from the encoded output — not `self.text.len()`.
- Unmappable char → abort the write, file untouched, named error (no NCR
  fallback).

**Dependencies:** Phases 0–2.

## Phase 4 — Tests, example, docs  ✅ DONE (2026-06-15)

> **Landed.** 7 new round-trip tests in `crates/nxvim-server/tests/editing/encoding.rs`
> (invalid-UTF-8 byte-identical, latin1, utf-16le+BOM, utf-8+BOM, `fenc` conversion,
> fail-loud `E513`, valid-UTF-8 SPUA scalar). The `unreadable_startup_file…` test
> became `invalid_utf8_startup_file_opens_named_and_resilient`. A daemon test
> (`nonutf8_file_decodes_over_the_wire_like_local`) proves local↔daemon agreement.
> `examples/encoding/` ships a latin1 + an invalid-UTF-8 sample with an `init.lua`
> walkthrough (both verified to round-trip byte-identically through the seam).
> `architecture.md` → *Text model* gained the UTF-8-internal / `'fileencoding'` note.
> **Note:** because nxvim maintains a trailing newline in the rope, a byte-identical
> round-trip needs the source file to already end in one (every fixture/sample does).

**Goal:** end-to-end coverage and a runnable example.

New `crates/nxvim-server/tests/editing/encoding.rs` (behind the `editing.rs`
entrypoint), each test writing a temp file then `:e`-ing it:

- **Invalid UTF-8 round-trips**: write `[..valid.., 0xff, 0xfe, ..]`, `:e`, assert
  it opens non-empty, edit nothing, `:w`, assert bytes on disk are
  **byte-identical** to the original.
- **latin1**: `0xe9` → buffer shows `é`, `fileencoding=latin1`, `:w` reproduces
  `0xe9`.
- **utf-16 + BOM**: UTF-16LE BOM file decodes, `bomb=true`, `:w` re-emits the BOM
  and the utf-16 newline.
- **`:set fenc=utf-8` then `:w`** converts a latin1 buffer to utf-8 on disk.
- **fail-loud**: latin1 `fileencoding` + a `€` in the buffer → `:w` errors, file
  unchanged.
- **PUA bijection**: a valid utf-8 file containing `U+F00xx` survives open→write
  unchanged.
- Update the daemon open test so local and daemon agree.

Plus a runnable `examples/encoding/` (a latin1 sample + an invalid-utf-8 sample +
README), per the project's example convention, and a short note in
`docs/architecture.md` → *Text model* that internal is UTF-8 and `fileencoding`
governs the on-disk form.

**Dependencies:** Phases 0–3.

## Remaining work — wasm read path

The browser read path is the one place that still doesn't share the decoder. The
Worker (`crates/nxvim-edithost/web/worker.mjs`) reads an OPFS file as an
`ArrayBuffer`, decodes it to a **string** with `TextDecoder` (lossy on invalid
bytes), and passes that string across the `eh_fs_read_complete` FFI (`"string"`
arg) into `complete_fs_read` → `load_replica_wasm` → `load_str_into`. So a wasm
buffer's `'fileencoding'` stays the utf-8 default and non-UTF-8/invalid files are
mangled by JS before Rust ever sees them. wasm **writes** are already correct (they
share `Buffer::to_save_bytes`), so a wasm-opened utf-8 file is consistent; the gap
is only multi-encoding/invalid-byte *resilience* in the browser.

To close it (a self-contained slice, needs Playwright verification per the web-test
convention): change `eh_fs_read_complete`'s file case to take raw bytes
(`ptr` + `len`) instead of a C string, have the Worker pass the `ArrayBuffer` bytes
(allocated into wasm memory) instead of `TextDecoder`-ing them, and route them
through `Editor::load_bytes_into` (the same decoder the native/daemon paths use).
The directory (`kind == 2`, JSON) and error (string) cases stay string-based.

## Phase 5 — (Later / optional) legibility & breadth

Not required for the feature to be correct; tracked here so it isn't lost:

- Render the latin1-fallback's high bytes as `<xx>` hex tokens (extmark/syntax
  overlay) instead of the font's tofu box — closer to vim's display of an
  unprintable byte, without giving up the exact round-trip.
- Add CJK / other encodings to the `fileencodings` default and the validator.

---

## Risks (carried from design review)

1. **Read seam unification (Phase 2) is the load-bearing change** — without
   `load_bytes_into` and the daemon/wasm reroute, the decoder can't be shared and
   the fork persists.
2. **`encoding_rs` default emits numeric character references** for unmappable
   chars — silent corruption; Phase 3 must intercept and error.
3. **PUA escape must be bijective over the byte stream**, not just over the
   invalid bytes (Phase 0) — easy to forget; covered by the PUA-bijection test.
4. **BOM re-emission + utf-16 newline encoding** on write, and the `:w`
   byte-count now diverging from rope length for non-UTF-8 files (Phase 3).
5. **emscripten build**: `simd-accel` off; verify the core encoding feature flows
   through the `--no-default-features` edithost build.

Leave these `from_utf8_lossy` spots alone — they are PTY/clipboard/subprocess
streams, not buffer files: `clipboard.rs:77`, `terminal.rs:38`, the ssh/remote
output paths.

## Verification (whole feature)

- `cargo test -p nxvim-server --test editing encoding`
- `cargo test --workspace` (no regression; the `unreadable_startup_file…` test
  changes meaning in Phase 2)
- `cargo clippy --all-targets -- -D warnings` && `cargo fmt --all`
- Both build configs: default, and `--no-default-features --features lua51`
  (edithost); then the `wasm32-unknown-emscripten` edithost build.
- Manual: `cargo run -p nxvim -- examples/encoding/latin1.txt`, confirm render
  and that `:w` keeps bytes identical (`cmp` before/after).
