# A lone `\r` is a character: the LF-only line model

Status: done (all three phases).

## The problem

The rope splits lines with ropey's `LineType::LF_CR`, which breaks a line at `\n`, at
`\r\n`, and at a lone `\r`. Tree-sitter counts rows by `\n` alone. So in a buffer holding a
lone `\r`, every tree-sitter row after it is one less than the editor's: a query, a fold,
an indent, a text object or a highlight lands on the wrong line. bemtvi-md-table hit it as
a table reported one row above where it is, with `:MdTableExpand` writing delimiter dashes
into the header row (the plugin now refuses rows that aren't a table, but the rows are
still wrong for every tree-sitter consumer).

`LF_CR` was chosen so that files of either `'fileformat'` split into lines. That reason no
longer holds: a file is normalized on read (`normalize_eol`: `\r\n` → `\n` for a Dos file,
`\r` → `\n` for a Mac one) and converted back on write, so the rope only ever holds `\n`
line breaks. A `\r` left in it is a character the file holds inside a line, a stray CR in
a Unix file, and splitting there is the one thing the model does that vim doesn't: vim
shows it as `^M` inside its line.

## The decision

Lines break at `\n` only (`LineType::LF`), everywhere the editor counts rows: the core,
the treesitter engine, and the browser's tree-sitter leg, which then agree with
tree-sitter's own rows by construction instead of by conversion. A lone `\r` displays as
`^M` (the renderer already substitutes C0 controls).

The one place that has to keep counting a lone `\r` as a break is LSP: the protocol
defines a line end as `\n`, `\r\n` or `\r`, and servers count rows that way. LSP positions
go through a conversion between the two models, by byte offset: a core `(row, col)` is a
byte, and a byte is an LSP `(line, character)` under `LF_CR`, and back. ropey keeps both
metrics, so either direction is O(log n), and in a buffer without a lone `\r` (nearly every
one) the two agree.

## Phases

### Phase 1: the line model

- One canonical constant, `bemtvi_core::buffer::LINE_TYPE = LineType::LF`, used by the core
  buffer, undo, the treesitter engine and the browser edit host (which today each spell
  `LF_CR` themselves). The LSP shadow keeps its own `LF_CR`, documented as the protocol's.
- ropey's `metric_lines_lf` feature on (keeping `metric_lines_lf_cr` for LSP).
- Check every place that splits text into lines itself (`str::lines` strips a `\r` before
  `\n`; `split('\n')` doesn't) where it has to agree with the rope.
- Tests: a lone `\r` is part of its line (line count, `getline`, the painted `^M`, motions
  over it); a `btv.treesitter.query` row after a lone `\r` matches the buffer row; an
  incremental parse across edits around a `\r` stays right; reading and writing Unix, Dos
  and Mac files is unchanged (a Dos file's `\r\n` still never reaches the rope).

At the end of phase 1 the editor and tree-sitter agree, and LSP positions after a lone `\r`
are off by one (they agreed before only because both counted it). Phase 2 fixes that.

### Phase 2: LSP positions

- A conversion pair on the core `Buffer`: a byte to an LSP line (and the line's start
  byte) under `LF_CR`, and an LSP line to its start byte.
- Route the two existing chokepoints through it: `lsp_position_in` (outgoing) and the
  position resolver behind `lsp_range_to_bytes_in` (incoming: edits, formatting, rename).
- Route the sites that read a server's line number as a row directly: diagnostics,
  locations (definition / references / symbols, including a deferred jump that converts
  when the file lands), inlay hints, semantic tokens, document highlights, folding ranges,
  code lenses, call hierarchy, and the browser's `SyncLspClient` path.
- Tests against the reactive LSP mock with a document holding a lone `\r`: a hover sends
  the position the server expects; a diagnostic past the `\r` paints on its line; a
  definition jump and a formatting edit land where the server meant.

Done. As built:

- `Buffer::lsp_lines_differ` (O(1): the two line counts) gates every conversion, so a
  buffer without a lone `\r` keeps the exact code path it had. `lsp_line_count`,
  `lsp_line_start`, `lsp_line_text` and `lsp_line_of_byte` answer in the protocol's lines
  (`bemtvi_core::buffer::LSP_LINE_TYPE`, which the LSP shadow now uses too).
- `LspPosResolver` converts in the protocol's lines, with `point` (to a row and byte
  column) and `row_of_lsp_line`; `editor_pos` / `editor_range` read coordinates already
  in the editor's rows, which is what `vim.diagnostic.set` entries are (they share the
  diagnostic surfaces with server ones and must not be converted).
- Converted: both chokepoints, the diagnostic mirror (`btv.diagnostic.get` now reports
  rows and byte columns, as documented; for a UTF-16 server its columns used to be the
  raw characters), locations and symbols through `location_point`, the direct and the
  deferred jump, inlay hints (and their request range), semantic tokens, folding ranges.
  Document highlights, code lenses, call hierarchy and selection ranges are not
  implemented, so there was nothing to convert.
- Lua: the buffer mirror carries `lsp_lines_differ`; `btv.lsp.position_params` and
  `btv.lsp.locations_to_items` convert over the document's rows only when it is set (or,
  for a file read off disk, when a scan finds a `\r` before a row's end).
- Limitation: a location in a file no buffer has loaded keeps the server's raw line and
  character for the picker row it shows, since its text isn't here to convert against.
  A jump there converts once the file is read.

### Phase 3: the browser and the docs

- Verify the browser build end to end (`--no-default-features`, the `verify-*.mjs`
  checks): the tree-sitter worker's rows, the edit host's text-object and fold
  conversions, LSP over the daemon.
- architecture.md's text model section, and the comments that still describe `LF_CR`.

Done. A release build of the edit host passes the tree-sitter, markdown, encoding,
shada, width and LSP checks (`verify-treesitter{,-query,-folds,-indent,-textobjects}`,
`verify-markdown-highlight`, `verify-encoding`, `verify-shada`, `verify-width`,
`verify-lsp`, `verify-lsp-reconnect`, `verify-diagnostic-signs`,
`verify-basedpyright-lsp`). `verify-treesitter-query.mjs` gains a lone-`\r` case (query
rows agree with buffer rows, and across edits around the `\r`), which a build of `main`
from before phase 1 fails. The worker's own row splitting (`text.split('\n')` in
`ts-buffers.js` / `highlight.js`) already counted `\n` alone; under `LF_CR` it was the
core that disagreed with it. architecture.md's text model describes the LF model and the
LSP conversion.
