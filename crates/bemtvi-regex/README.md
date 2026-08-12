# bemtvi-regex

Vim regular expressions for bemtvi — by embedding the real engines.

This crate vendors neovim's `regexp.c` (which contains **both** vim regexp
engines: the Henry-Spencer-derived backtracking engine and the NFA engine,
plus vim's automatic engine selection and fallback) and compiles it as C via
`build.rs`, behind a safe Rust API (`VimRegex`, `VimBuffer`). Matching
semantics — magic levels, `\zs`/`\ze`, lookaround, backreferences, counted
and lazy multis, character classes, `'iskeyword'`, multi-line patterns,
multibyte case folding — are vim's own code, not a reimplementation.

Builds and passes its test suite on native targets **and**
`wasm32-unknown-emscripten` (the browser path; `wasm32-unknown-unknown`
cannot compile C, so the web client must use the emscripten target to get
this engine).

## Layout

| Path | What it is |
| --- | --- |
| `csrc/nvim/regexp.c` + headers | vendored from `vendor/neovim` @ `70cfeabe23` (v0.12.0-794), with `// BEMTVI:`-marked patches |
| `csrc/nvim/{mbyte,charset,strings,garray}.c` | supporting code; mbyte/charset/strings are *subsets* extracted with `extract-subset.py` |
| `csrc/nvim/*.h` (small ones) | shim headers replacing nvim subsystems (globals, options, memline, marks, plines, profile, eval) |
| `csrc/shim/btvre_shim.{h,c}` | the host interface: line provider, cursor/Visual/marks state, error sink, interrupts, allocation |
| `csrc/nvim/*.generated.h` | prototype headers produced by `gen-headers.sh` (runs neovim's own `gen_declarations.lua` via `nvim -l`) |
| `vendor/utf8proc/` | utf8proc 2.11.3 (neovim's pinned version), MIT — Unicode properties, exactly as upstream |

The engine reaches the host **only** through the shim: buffer lines come from
a callback on `buf_T` (replacing memline), `\%#`/`\%V`/`\%'m` read injected
cursor/Visual/mark state, errors land in a buffer the Rust side drains.
Upstream's two `ml_get_buf()` call sites were the entire text-access seam.

## What is intentionally not provided

Fail-loud by design (see CLAUDE.md conventions):

- **`\=` expression substitution** and **`submatch()`** need the vimscript
  evaluator; `vim_regsub_both()` reports an error if asked. The host
  implements `\=` on top of `reg_submatch()`.
- **`\%v` virtual columns** are computed from `'tabstop'` + utf8proc
  character widths, not vim's full charsize machinery (`'vartabstop'`,
  `'list'`, `<xx>` display of unprintable bytes are not modeled).
- Marks default to "not set" (vim's NOMATCH semantics) until the host
  registers a provider.

## Regenerating / re-vendoring

1. Update `vendor/neovim`, re-copy the vendored files, re-apply the
   `// BEMTVI:` patches (`git diff` against the previous vendored copy shows
   them all).
2. Re-extract subsets: `extract-subset.py <upstream.c> <symbol>...` (the
   header comment of each subset file lists its symbols).
3. `./gen-headers.sh` (needs `nvim` on PATH) to regenerate prototype headers.
4. `cargo test -p bemtvi-regex`, and for the browser target
   `CARGO_TARGET_WASM32_UNKNOWN_EMSCRIPTEN_RUNNER=node cargo test -p bemtvi-regex --target wasm32-unknown-emscripten`.

## License

`csrc/nvim/` is derived from neovim: the Vim-derived base is under the **Vim
license**, neovim-era modifications under **Apache-2.0** (see `LICENSE.txt`,
copied verbatim from neovim — this satisfies Vim license II.1/IV). Compliance
notes for distribution:

- Changes are distributed in source form with every copy (Vim license
  II.2.c): the patched files live in this repo, diffable against
  `vendor/neovim` @ `70cfeabe23bbeeb2995318cd1a5224d6069fad5b`.
- A "modified" notice belongs in bemtvi's `:version`/`--version` output once
  this crate is wired into the editor (Vim license II.3).
- `vendor/utf8proc/` is MIT (see its `LICENSE.md`).
- The shim (`csrc/shim/`), build script, and Rust code are Apache-2.0 like
  the rest of the workspace.
