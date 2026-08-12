# Documentation site (rust-book style, GitHub Pages)

A versioned, navigable documentation site in the style of *The Rust Programming
Language* book — built with [mdBook](https://rust-lang.github.io/mdBook/), hosted
on GitHub Pages, and **generated from the source** so it cannot drift.

"Generated from the source" means two distinct pipelines, both run at build time
by a single generator (`book/gen/generate.py`, stdlib-only Python 3):

1. **Curated narrative** — hand-organized book chapters (introduction, getting
   started, configuration, plugin authoring, architecture) assembled from the
   existing `docs/*.md` and the README. Long-form source docs
   (`architecture.md`, `plugin-authoring.md`, `known-approximations.md`, …) are
   *imported verbatim* into the book with their relative links rewritten to
   absolute GitHub URLs, so a single edit to the canonical doc updates the book.

2. **`btv.*` API reference** — auto-extracted from the Lua prelude
   (`crates/bemtvi-lua/src/prelude/*.lua`). Every public declaration
   (`function btv.NS.name(args)` or `btv.NS.name = function(args)`; `btv._private`
   excluded) plus the contiguous `--` doc-comment block above it becomes an
   entry, grouped into one page per top-level namespace (`btv.buf`, `btv.fs`,
   `btv.lsp`, …). This is the canonical surface per ADR 0002.

The generator also renders `book/src/SUMMARY.md` from `SUMMARY.template.md` (a
`{{API_REFERENCE}}` placeholder is replaced with the generated namespace list),
so the table of contents stays correct as the API grows.

## Why mdBook + Python generator

- mdBook *is* the rust-book toolchain — the look the request asked for, zero
  custom theming needed for a first cut.
- The generator is **build tooling**, not shipped product code, so it sits
  outside the Rust workspace. Python 3 stdlib is preinstalled on the CI runner
  and on macOS; the script has no third-party dependencies. (If we later want it
  in-tree as a Rust `xtask`, the extraction logic ports directly.)
- Generated output is **not committed** (`book/src/api/` and the rendered
  `SUMMARY.md` are git-ignored). The prelude and the template are the source of
  truth; CI regenerates on every build. Locally: `python3 book/gen/generate.py
  && mdbook serve book`.

## Layout

```
book/
  book.toml                 # mdBook config (title, repo links, git-edit URL)
  gen/generate.py           # the from-source generator
  src/
    SUMMARY.template.md      # TOC with {{API_REFERENCE}} placeholder (committed)
    SUMMARY.md               # rendered (git-ignored)
    introduction.md          # curated (committed)
    guide/*.md               # curated getting-started (committed)
    plugins/*.md             # curated plugin-dev (committed)
    architecture/*.md        # imported from docs/ (git-ignored)
    api/*.md                 # generated btv.* reference (git-ignored)
    appendix/*.md            # imported from docs/ (git-ignored)
.github/workflows/docs.yml  # build + deploy to GitHub Pages
```

## Phases (commit + pause for review between each)

1. **Plan** (this doc).
2. **Scaffold** — `book.toml`, `SUMMARY.template.md`, committed curated chapters,
   `.gitignore` entries.
3. **Generator** — `generate.py`: doc-import (with link rewriting) + btv.* API
   extraction + SUMMARY rendering. Fail-loud if the prelude dir or a referenced
   source doc is missing (no silent empty pages — per CLAUDE.md).
4. **Local verify** — install mdBook, run generator + `mdbook build`, confirm the
   API pages carry real extracted content and links resolve.
5. **Deploy** — `docs.yml` GitHub Actions workflow: checkout, setup Python +
   mdBook, run generator, `mdbook build`, upload artifact, `actions/deploy-pages`.
   One-time: enable Pages "GitHub Actions" source in repo settings (manual; noted
   in the workflow comments).

## Non-goals (first cut)

- No per-version (mike-style) doc archiving — single `latest` site.
- No structured `---@param`/`---@return` typed signatures; the prelude documents
  in prose, so the reference reproduces the prose + the declared signature line.
  A typed-annotation pass can come later if the prelude adopts EmmyLua comments.
- Design specs/plans stay in `docs/` and are linked, not inlined.
