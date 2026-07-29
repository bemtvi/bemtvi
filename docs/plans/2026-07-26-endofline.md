# `'endofline'` / `'fixendofline'` — saving a file without a trailing newline

*2026-07-26*

## The problem

nxvim's rope always ends with a `\n` (`line_count == rope.len_lines() - 1`; the final
phantom line is never edited or shown). That invariant is load-bearing: every byte
offset, `line_start`, mark/extmark shift, tree-sitter point, cursor clamp and motion
assumes it.

But nxvim stores **only** the rope, so it has no way to record the one fact the rope
can't express: *the file on disk had no final newline*. Consequences today:

- `:w` on a file read as `a\nb` writes `a\nb\n`. Silently. There is no opt-out.
- `:w` on a 0-byte file writes 1 byte (`to_save_bytes` returns the whole rope).
- The LSP layer tells every server the document ends with a newline even when the
  file doesn't, and — worse — that a *freshly created, 0-byte* file contains `"\n"`.
  Two sites work around the fallout by special-casing `buffer.len_bytes() == 1`
  (`lsp/edit.rs:558`, `lsp/edit.rs:1280`) and retargeting the last edit's byte range
  to `0..1` so it eats the phantom.

## The reframe: the invariant is not the bug

Vim's buffer is a list of lines with an *implicit* newline after every line, including
the last. nxvim's rope-with-mandatory-trailing-`\n` is the same model spelled
differently — the phantom `\n` **is** vim's implicit final newline. What vim has that
nxvim doesn't is the pair of options that record and control the on-disk fact:

```
$ nvim --headless -u NONE   (probe, 2026-07-26)
  noeol.txt  ("a\nb")   eol=0 fixeol=1     :w → "a\nb\n"   (fixeol adds it)
                                  nofixeol :w → "a\nb"     (preserved)
  eol.txt    ("a\nb\n") eol=1 fixeol=1     :w → "a\nb\n"
  empty.txt  (0 bytes)  eol=1 fixeol=1     :w → 0 bytes    (ML_EMPTY)
  nofile.txt (absent)   eol=1 fixeol=1     :w → 0 bytes
  after a fixeol :w that added the newline, vim leaves &eol stale at 0
```

So: **keep the rope invariant, add the state.** Relaxing the invariant instead would
put an "is this the last line?" branch through the entire core for one bit of
information, and vim doesn't do it either.

`'endofline'` is structurally identical to `'bomb'`, which already exists: detected on
read, consumed only on write, mirrored to `nx.bo`, zero effect on the rope. Follow that
precedent exactly.

## The model

Two buffer-local options plus one derived accessor.

- **`'endofline'` (`'eol'`)** — whether the buffer's document ends with a newline.
  Set from the bytes on read. Default `false` (an empty document does not end with a
  newline).
- **`'fixendofline'` (`'fixeol'`)** — whether a write appends the missing newline.
  Default `true`, matching vim, so the observable default behavior is unchanged.

- **`Buffer::document_text()`** — the buffer's content *as a document*: the rope, minus
  its final `\n` when `!endofline`. This is the canonical "what bytes does this buffer
  represent" and it is what both the write path and the LSP layer consume.
  `Buffer::is_empty_document()` is `document_text().is_empty()`.
- **`Buffer::save_text()`** — `document_text()` plus the `'fixendofline'` newline when
  the document is non-empty and lacks one. `to_save_bytes` encodes this.

Read detection (`decoded` is the file's text *before* `ensure_trailing_newline`):

```
endofline = decoded.ends_with(line_break)      // "" → false, "\n" → true, "a\nb" → false
```

Checked against every case vim was probed on:

| file        | eol   | rope     | document | `:w` fixeol | `:w` nofixeol | vim `:w` |
| ----------- | ----- | -------- | -------- | ----------- | ------------- | -------- |
| `a\nb`      | false | `a\nb\n` | `a\nb`   | `a\nb\n`    | `a\nb`        | same     |
| `a\nb\n`    | true  | `a\nb\n` | `a\nb\n` | `a\nb\n`    | `a\nb\n`      | same     |
| `\n`        | true  | `\n`     | `\n`     | `\n`        | `\n`          | same     |
| 0 bytes     | false | `\n`     | ``       | 0 bytes     | 0 bytes       | same     |
| absent      | false | `\n`     | ``       | 0 bytes     | 0 bytes       | same     |

The empty-document rule covers vim's `ML_EMPTY` **as read** — a 0-byte file stays 0
bytes — while still round-tripping a file that genuinely contains one newline, and it
does so without vim's extra hidden bit, because defining `endofline` *honestly* (an
empty document has no final newline) already distinguishes those two reads. It does not
reproduce `ML_EMPTY` for a buffer **emptied by editing**; see divergence 3.

### Three deliberate, documented divergences from vim

1. **`nofixeol` on a buffer that never read a no-eol file.** `:enew`, `:set nofixeol`,
   type `x`, `:w` → nxvim writes `x`, vim writes `x\n` (vim reports `eol=1` for a
   buffer with no file behind it). nxvim's answer is the one the option literally asks
   for, and it falls straight out of the honest definition. Invisible under the default
   `fixeol`.
2. **A write updates `'endofline'`.** After a `'fixendofline'` write appended the
   newline, nxvim sets `endofline = true` — the file on disk really does end with one.
   Vim leaves `&eol` stale at `0` (probed above). Keeping it accurate matters here
   because the LSP sync path keys off it (below): a stale `false` would keep telling
   every server the file is unterminated when it no longer is.
3. **A buffer emptied by *editing* keeps the terminator its file had.** `ggdG` on a
   file read as `a\n`, then `:w` → nxvim writes `\n`, vim writes 0 bytes (`0L, 0B`).
   Vim's 0 is `ML_EMPTY`, not `'eol'`: probed, `&eol` is still `1` after the `ggdG`,
   so this is precisely the second hidden bit — and it is load-bearing for *undo*.
   With one honest flag, clearing it when the document goes empty would make
   `ggdG` + `u` + `:w` under `'nofixendofline'` write `a`, silently **dropping** a
   terminator the file has. Paying one byte on an emptied buffer is the better half
   of that trade, so the flag is left describing what it honestly still describes.
   (A file read *without* a terminator and emptied the same way writes 0 bytes, as in
   vim — its flag was already off.)

## Phases

### Phase 1 — the state and the write path (core)

1. `BufferOptions { endofline: bool, fixendofline: bool }` (+ defaults `false` / `true`,
   `OptionInfo` entries with the `eol` / `fixeol` abbreviations).
2. `Buffer::document_text()` / `save_text()` / `is_empty_document()` /
   `document_len_bytes()`.
3. `to_save_bytes` builds on `save_text()`. That is the single choke point every write
   funnels through — local `Buffer::write`, the daemon save, and the wasm save all
   snapshot through `Editor::enqueue_save_of` (`editor/buffers.rs:871`), so the
   tier-1 remote requirement is satisfied by construction.
4. `Buffer::write` / `mark_written` set `endofline` from the bytes actually written.
5. Read detection in `Buffer::from_file` and in the off-tick landing
   `Editor::load_bytes_into_enc` (daemon / wasm / `:e`).
6. Surface wiring, following `'bomb'` exactly: `apply_set_bool` slots,
   `set_buffer_option_bool` (`nx.bo`), `BoMirror` + `BUF_OPT_CANON` / `BUF_OPT_DEFAULT`
   in `prelude/state.lua`, `StatuslineCtx` + the `[noeol]` default-statusline flag +
   the `&endofline` / `&fixendofline` statusline-expr resolvers (both, since `&eol`
   alone can't tell a *preserved* missing terminator from one about to be supplied).

**The `[noeol]` cue is `Buffer::is_unterminated_document()`, not `!endofline`.** The
flag is honestly off for an *empty* document too, and reporting that as a missing
newline puts `[noeol]` on `[No Name]` and on every brand-new file (vim shows `[New]`
there, and reports `'eol'` on). The marker also belongs only to a **document**, not to
editor chrome — a panel/listing/`nx.view`/terminal is never written to disk, so it is
gated on `buftype == ""` (the canonical kind signal, threaded into `StatuslineCtx`
alongside `%{&buftype}`; a scratch buffer's *name* is `[Messages]`, so its `path` is not
the signal). Both gates hold for the two places the cue appears:

- the default status line's `[noeol]` suffix on the encoding, and
- the `"{name}" [noeol] {lines}L, {bytes}B written` echo, vim's own tag for a write
  that left the last line unterminated. Off the tick it rides the snapshot
  (`PendingSave::noeol` ← `Buffer::save_is_unterminated`), so the daemon ack reports
  the bytes that crossed the wire rather than the buffer whenever the ack landed.

Tests (`crates/nxvim-server/tests/editing/endofline.rs` — its own submodule beside the
`'fileformat'` / eol-shaped coverage): every row of the table above, `dos` + noeol,
a non-UTF-8 encoding + noeol, `:set noeol` / `:set nofixeol` round-trips, and the
same over the daemon (`--test daemon_save`).

### Phase 2 — the LSP document seam (sync)

Neovim's `buf_get_full_text` appends the line ending only `if vim.bo[bufnr].eol`, so
honoring `'endofline'` here *is* the reference behavior.

- `didOpen`, a `resync` batch, and a `FULL`-sync server all send `document_text()`
  and seed `shadow` from it (`lsp/sync.rs:459`, `lsp/sync.rs:629`).
- **A `!endofline` buffer stays incremental, via a bracketed replay.** The incremental
  path replays journaled *rope*-space byte deltas over `shadow`; when the document is
  the rope minus its last byte, a single rope-space edit is not a single
  document-space edit — `dd` on the last line of `a\nb` deletes rope bytes `2..4` but
  document bytes `1..3`, because the byte that *was* the phantom's predecessor becomes
  the new phantom. Endpoint remapping cannot express that.

  Bracketing can, exactly and in O(1) extra changes. An LSP `didChange` carries a
  *sequence* of edits, each addressing the document as the previous one left it, so:
  append the phantom, replay the journal verbatim in the rope coordinates it was
  written in, delete the phantom. Every intermediate state the server passes through is
  one nxvim really passed through — that is what makes it correct rather than merely
  plausible — and the shadow ends each batch equal to the document the server holds.

  The two brackets are **independent**, which is what carries a buffer across a
  *change* of `'endofline'` with no resync: the leading one is needed iff the shadow is
  a document (the flag as of the last sync, `LspServerDoc::shadow_endofline`), the
  trailing one iff the new document drops the phantom (the flag now). A
  `'fixendofline'` write therefore syncs as a lone "append the newline" change — and
  because such a write moves the document without touching the rope, `sync_lsp`
  compares `shadow_endofline` alongside `changedtick` so the flip is noticed at all.

  The only fallback left is a whole-document push when the replayed shadow doesn't end
  in `\n`, which requires a violated rope invariant — a guard, not a routine path.

### Phase 3 — the LSP apply path in document coordinates

An LSP range addresses the *document*, not the rope. One shared helper in `lsp/mod.rs`
replaces the ad-hoc conversion at all four call sites (`edit.rs:24`, `:62`, `:551`,
`:1274`):

- Clamp `Position.line` to the document's last row (`line_count() - 1` when
  `!endofline`, `line_count()` otherwise — the phantom row is a real, addressable
  document row only when the document ends with a newline).
- An edit whose range reaches the **document end** is extended to the rope end, so its
  replacement supersedes the phantom instead of being inserted before it, and
  `endofline` is re-derived from that edit's text (empty text → from the byte now
  preceding the range).

  *Which* edit that is, is decided by where it **starts**. `apply_edits_to` orders by
  start byte (descending), so among the edits reaching the document's end the tail is
  the one starting latest — not the one listed last, which is what a server emitting
  its edits bottom-up hands over. Ranking on `end` alone and breaking the tie on array
  index picks the wrong one there, and since the tail is the edit widened over the
  phantom, widening an earlier edit stretches it across its sibling and eats it
  (`let a = 1` + `[append ";", replace "1"→"2"]` came out `let a = 2`). Array order is
  the tie-break only for a genuine tie — same start *and* same end, the
  several-edits-share-a-position case the LSP spec allows.

This is the general form of the two `len_bytes() == 1` hacks, which are deleted. It also
fixes cases the hacks never covered: a formatter that adds a trailing newline to a
no-eol file now actually gives the buffer one, and a server appending text after the
final newline yields a no-eol document rather than a spurious blank line.

Worked through the existing `create` case: rope `\n`, `eol=false`, document `""`, so an
edit at `0:0` has range `0..0`, reaches the document end (`0`), extends to `0..1`,
consumes the phantom, and sets `endofline = true` from its `…\n` text — exactly the
result the hack produced, derived from the model instead of a length probe.

### Phase 4 — example and docs

`examples/endofline/` (`init.lua` + `sample.txt`, numbered type-this / see-that
sections) per the repo workflow rule, and the option docs the book generator picks up
from the `OptionInfo` table.

## Non-goals

- Relaxing the trailing-newline rope invariant.
- `'binary'` (vim's `:set binary` implies no-eol preservation plus much else) — a
  natural neighbor, out of scope.
