# Rendering reStructuredText in doc floats

> **Status: COMPLETE.** Both phases landed; this document is now *history*,
> not a live to-do.

## Why this document exists

LSP's `MarkupKind` is a **closed two-value set** — `plaintext` or `markdown`.
There is no rst kind, no asciidoc kind, and no extension point: a client
advertises which of the two it accepts (bemtvi asks for markdown first, in both
`hover.contentFormat` and `completion.completionItem.documentationFormat`), and
the server must answer in one of them. A server whose docstrings are
reStructuredText therefore declares `plaintext`, because that is the only honest
value available to it.

So a python docstring reaches the doc float as a block of text the protocol says
is *not markup*, while being, in fact, markup. Since the previous commit that
text renders **verbatim** — a real improvement over putting it through the
markdown renderer, which reflowed its line breaks into one paragraph, ate its
`*`/`_` as emphasis, and read its four-space literal blocks as code. But
verbatim still shows `**bold**`, `:param path:` and `.. code-block:: python` as
literal text, and it leaves the one thing rst declares that markdown never
does — *the language of a code block* — on the floor.

The ecosystem's answer to rst is conversion: `python-lsp-server` and
`jedi-language-server` both convert rst docstrings to markdown through the
`docstring-to-markdown` package when the client prefers markdown. When that
package is present, none of this is needed. This plan is for when it is not, and
for servers that never convert (esbonio, anything hand-rolled).

## What decides that a block is rst

**Nothing the protocol says — so it must be declared.** There is no signal to
read: `plaintext` is what rst arrives as, and so is genuinely-plain text. Any
detection would be sniffing (`starts with :param`, `has a ::` line), which is
exactly the kind of heuristic that rots — a plain docstring that happens to
contain `*` would be silently reinterpreted as markup.

The trigger is an explicit per-server option, `docs_format = "rst"` on
`btv.lsp.config`, applying **only** to blocks that server declared `plaintext`.
A block it declared `markdown` is markdown, whatever the option says: the server
told us, and it outranks our configuration.

## Scope

The **docstring dialect** of rst, not the whole of Docutils. rst is a large
specification with substitutions, citations, footnotes, roles, grid tables,
option lists, and a directive registry that is open by design. Docstrings use a
narrow, well-known part of it, and that is what is rendered; everything else must
degrade to *showing its text*, never to eating it.

No new dependency. The one Rust rst parser (`rust-rst`) is stale and incomplete,
and the subset here is small enough to parse directly — the same shape as
`markdown.rs`, which is a hand-written renderer driven by a parser (pulldown-cmark
there, our own line scanner here).

---

## Phase 1 — the renderer ✅

**Goal.** `bemtvi_core::rst::render(src) -> markdown::Rendered` — the same
output the markdown renderer produces (stripped display lines, `@markup.*` byte
spans, whole-line fills, fenced-code blocks), so every consumer that already
renders a `Rendered` gets rst for free. Purely additive: nothing existing changes
behavior in this phase.

**Design.** rst drives the *same* `markdown::Renderer` that pulldown-cmark's
events drive — `open`/`close`/`write`/`newline`/`block_gap`/`rule`, the list
prefix machinery, the `quote` bar, `flush_code`'s block bookkeeping. The renderer
struct becomes `pub(crate)` for it. That is what keeps the two formats
pixel-identical where they agree (a bullet is the same bullet, a heading takes
the same `@markup.heading.N`) and keeps the whole layout story in one place.

**Block constructs.**

| rst | rendered as |
| --- | --- |
| section title (under/overline of one punctuation char) | `@markup.heading.N`, level by order of first appearance of that char |
| transition (4+ punctuation chars alone) | a full-width rule (`MdFill`), like a markdown `---` |
| paragraph | reflowed, inline markup applied |
| literal block (`::` + indented) | `MdCode { lang: None }`, dedented; the `::` marker removed or reduced to `:` per spec |
| doctest block (`>>>`) | `MdCode { lang: Some("python") }` — a doctest is python by definition |
| `.. code-block:: <lang>` / `code` / `sourcecode` | `MdCode { lang: Some(<lang>) }` — **the prize**: a declared language markdown never gets from converted rst |
| `.. note::` / `warning::` / `versionadded::` … | a styled label line + the body rendered as rst |
| unknown directive | label line naming it + body as rst — visible, never eaten |
| comment (`..` + non-directive) | dropped (rst comments do not render) |
| field list (`:param x:`, `:returns:`) | an aligned two-column block; `:type x:` / `:rtype:` merge into their `param`/`returns` row as `x (int)`, the Sphinx reading |
| bullet / enumerated list | the markdown list prefix machinery |
| definition list (term + indented body) | term styled, body indented |
| block quote (indented, no other structure) | the `▎ ` quote bar |
| line block (`\| line`) | verbatim lines |
| grid / simple table | verbatim — ASCII art already reads as a table, and misparsing one is worse than showing it |

**Inline markup.** ` ``literal`` ` → `@markup.raw`; `**strong**`; `*emphasis*`;
`` `title reference` `` and `:role:`content`` → `@markup.raw` (role prefix
stripped); `` `label <url>`_ `` → label + url, as a markdown link renders;
`name_` / `` `phrase`_ `` → `@markup.link.label`; `\*` escapes.

Docutils' **inline-markup recognition rules** are implemented, not approximated:
a start-string must be preceded by whitespace or one of ``-:/'"<([{`` and not
followed by whitespace; an end-string must not be preceded by whitespace and must
be followed by whitespace or one of ``-.,:;!?\/'")]}>``. This is what keeps
`**kwargs` from opening a strong span and `a*b*c` from emphasising `b` — without
it every python docstring that documents `*args, **kwargs` renders wrong.

**Lua surface.** `btv.rst.render(src)`, mirroring `btv.markdown.render` exactly
(1-based lines, char columns, the same `lines`/`highlights`/`fills`/`code`
shape), through a `btv._rst_render` bridge. It is the pure transform surface, so
it is both what plugins get and what the tests drive.

**Tests.** `crates/bemtvi-server/tests/rst.rs`, the shape of `tests/markdown.rs`:
black-box through the running server via `btv.rst.render`, one case per construct
above, plus the recognition-rule cases (`**kwargs` stays literal) and the
degradation cases (unknown directive, table).

**Files.** `crates/bemtvi-core/src/rst.rs` (new), `markdown.rs` (Renderer
visibility), `bemtvi-core/src/lib.rs`, `bemtvi-lua/src/install.rs`,
`bemtvi-lua/src/prelude/rst.lua` (new) + its prelude registration,
`crates/bemtvi-server/tests/rst.rs` (new).

---

## Phase 2 — wiring it to the doc floats ✅

**Goal.** A server declared `docs_format = "rst"` gets its `plaintext` hover and
completion documentation rendered by Phase 1's renderer.

**Approach.**
- `markdown::DocFormat` gains `Rst`. It is never produced by the protocol
  distiller — only by the config override — so the LSP layer keeps mapping the
  two real `MarkupKind`s and nothing else.
- The override is applied where the reply is distilled into a `DocsSection`
  (`show_merged_hover`, `lsp_complete_docs_sections`), per contributor: with two
  servers on a buffer, one may be rst and the other markdown, and each section
  renders its own way.
- `render_doc_sections` dispatches `DocFormat::Rst` to `Renderer::feed_rst`.
- `btv.lsp.config(name, { docs_format = "rst" })`, validated at registration
  against the closed set (`markdown` | `plaintext` | `rst`) and failing loud on
  anything else — a typo'd format must not silently mean "markdown".

**Tests.** Through the mock server, both surfaces: an rst docstring declared
`plaintext` renders as rst under the option, and as verbatim plaintext without
it; a block the same server declares `markdown` stays markdown regardless.

**Files.** `bemtvi-core/src/markdown.rs`, `bemtvi-server/src/lsp/{request,completion}.rs`,
`bemtvi-lua/src/prelude/lsp.lua`, `crates/bemtvi/tests/lsp_markdown_hover.rs`,
`crates/bemtvi/tests/lsp_complete.rs`, book docs for the option.

---

## Deliberately out of scope

- **Sniffing.** No content-based detection, in either phase.
- **`filetype=rst` buffers.** This is doc-float rendering, not an rst mode; a
  `.rst` file is a source file and keeps its tree-sitter highlighting.
- **The rest of Docutils.** Citations, footnotes, substitutions, option lists,
  and the parts of the directive registry beyond admonitions and code blocks
  render as their own text rather than being interpreted.
