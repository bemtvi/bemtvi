-- bemtvi:prelude/rst — the btv.rst.* surface: turn a reStructuredText string into
-- rendered, styled display lines. The sibling of btv.markdown (over the native
-- btv._rst_render bridge), and the second markup format bemtvi renders.
--
-- It exists because LSP cannot name rst. `MarkupKind` is a closed two-value set —
-- `plaintext` or `markdown` — so a language server whose docstrings are
-- reStructuredText declares them `plaintext`, the only honest value it has, and the
-- text arrives claiming not to be markup while being exactly that. Nothing in the
-- protocol tells the two apart, so nothing here guesses: a block is rendered as rst
-- only because someone said it is.
--
-- Available on every build (native and browser/wasm) — the renderer is pure Rust in
-- bemtvi-core with no editor state or I/O.

btv.rst = btv.rst or {}

-- btv.rst.render(src) -> { lines = {string,..}, highlights = { hl, .. },
--                         fills = { fill, .. }, code = { block, .. } }
--
-- Parse `src` as reStructuredText into stripped display lines with the markup syntax
-- removed (`**bold**` -> `bold`, a section underline consumed, `- x` -> `• x`,
-- `:param path:` laid out as an aligned column), plus the styling to paint over them.
-- The return shape is **identical** to `btv.markdown.render`'s, so one piece of
-- layout code renders either format:
--
-- ```
-- { line = <1-based line>, col_start = <1-based char col>,
--   col_end = <exclusive char col>, group = "<@markup.* capture>" }
-- ```
--
-- Columns are CHARACTER columns (not bytes). `group` is a neovim `@markup.*`
-- treesitter capture, the same set the markdown renderer paints, so a colorscheme
-- styles both identically. `fills` are row rules (a transition), and `code` are the
-- code blocks: `{ first_line = <1-based>, last_line = <1-based, inclusive>, lang = ? }`.
--
-- `lang` is where rst has the advantage: a `.. code-block:: python` directive
-- *declares* its language and a `>>>` doctest block is python by definition, so those
-- blocks come back ready to syntax-highlight, where a markdown rendering of the same
-- docstring recovers nothing.
--
-- The **docstring dialect**, not the whole of Docutils: section titles, transitions,
-- paragraphs, bullet/enumerated lists, definition lists, field lists (`:param x:`,
-- with `:type x:` folded into its row the way Sphinx reads it), literal blocks,
-- doctest blocks, line blocks, block quotes, admonitions and code directives, plus
-- inline literals / strong / emphasis / interpreted text / references. Anything else
-- degrades to showing its own text: an unknown directive renders its name and body,
-- a table renders as the ASCII art it already is.
--
-- Pure and infallible: unsupported constructs still contribute their text.
function btv.rst.render(src)
  return btv._rst_render(src or "")
end
