# `:help` on demand — bootstrapping bemtvi-help and shipping the book as vimdoc

Today `:help` in a fresh install prints a pointer at a plugin the user has to go install by
hand, and the ~415 KB of prose in [the bemtvi book](2026-06-19-documentation-site.md) exists
only as a website. This plan closes both gaps at once, without giving up the single binary:

```
:help btv.buf.line()
  → "Help isn't installed yet. Fetch bemtvi-help and the bemtvi docs from GitHub? [y/N]"
  → y
  → (async) clone bemtvi/bemtvi-help @ pinned commit, bemtvi/bemtvi-docs @ tag v0.5.0
  → re-dispatch  →  the help window opens on *btv.buf.line()*
```

Nothing is embedded in the binary and nothing is bundled beside it. The editor stays one
executable that knows *where its documentation lives* and fetches it the first time someone
asks for it — the same bet `:TSInstall` already makes for tree-sitter grammars.

## The two halves

**Content.** A third emitter in `book/gen` renders vimdoc from the same model the book is
built from, and a GitHub Actions job publishes it to a generated `bemtvi/bemtvi-docs` repo,
tagged per bemtvi release.

**Delivery.** The existing "plugin not installed" arm for `:help` becomes a bootstrap that
installs `bemtvi-help` *and* `bemtvi-docs` through `btv.plugins`, then retries the command.

They are independent: the emitter is useful without the bootstrap (people can install the
docs repo by hand), and the bootstrap is useful without the emitter (it would fetch just
the plugin). Phase them separately.

## Why the docs are a git repo, not an archive

The first sketch of this was a release asset — a `.tar.gz` of the vimdocs, fetched and
unpacked on first `:help`. Rejected, because it buys a second install path for nothing.

`btv.plugins` already clones through `btv.git_local` — **first-party gix, no `git` binary**
(`crates/bemtvi-lua/src/prelude/plugins.lua:6`) — and it is already remote-aware by
construction (`docs/plans/2026-07-03-remote-aware-plugin-manager.md`). If the docs are a
repo with a `doc/` directory, then:

- installing them is **one more spec in the same call** that installs the help plugin;
- `bemtvi-help` discovers them with **zero code**, because its index is
  `btv.runtime_file("doc/*.txt", true)` over the runtimepath — registration is by
  convention, exactly like neovim;
- `:Plugins` lists them, `:PluginSync` updates them, and removal already works.

An archive needs all of that written again: fetch bytes, verify a checksum, extract, place
under a state dir, register on the runtimepath, and then answer *"how does the user update
or remove this?"* with a mechanism `btv.plugins` already answers.

Being fair to the archive: the machinery is not from scratch. `crates/bemtvi-ts/src/install.rs`
already does tar/gz/xz/zip with `sha2` verification for `:TSInstall`, and the workspace
deps carry that rationale (`Cargo.toml:206`). But it is native-only — blocking `ureq`, run
off-tick — so per the tier-1 remote rule it owes a separate answer for the daemon and wasm
worlds, which the git path gets for free. And the size argument is weaker than it feels:
the entire book is ~415 KB of markdown, so a shallow clone at a tag is about one HTTP
request's worth of bytes.

**Where the archive would genuinely win** is the serverless browser build, which has no
gix leg at all. The answer there is not a runtime fetch either — bake the docs into the
build, the way `crates/bemtvi-edithost/build-plugins.sh` already amalgamates the recommended
plugin set into one bundle. See *Three worlds* below.

## Why not vendor the docs into bemtvi-help

`bemtvi-help` is the **viewer**. The prose has two upstream sources, both in this repo: the
Lua prelude docstrings (extracted by `book/gen/generate.py`) and `docs/*.md` (imported by
the same script's `IMPORTS` list at `generate.py:56`). A generated copy committed into the
viewer drifts from the running binary every time a docstring changes, and because the
plugin pin and the editor version are independent, a user can end up reading documentation
for an API their build does not have.

The docs must be versioned with the **editor**, which is why they get their own generated
repo tagged to the editor's version, and why the viewer stays content-free.

This also settles an open question: since the docs arrive as a runtimepath entry like any
plugin, **core never needs a runtimepath entry of its own**. `default_runtime()`
(`crates/bemtvi-server/src/lib.rs:468`) keeps its current shape — `$BEMTVI_RTP`, then the
config dir, then discovered plugins.

## Content: the vimdoc emitter

A new `book/gen/vimdoc.py` (sibling to `generate.py`, reusing its `extract_api()` and
import model) emits per-chapter markdown pre-processed for
[panvimdoc](https://github.com/kdheepak/panvimdoc), then runs the pinned panvimdoc over
each file. The plugin convention in `~/work/nxvim-plugins/WRITING-VIMDOCS.md` is the
template: markdown is authored (or generated), the `.txt` is never hand-aligned, and
pandoc owns the 78-column math.

Do **not** point panvimdoc at `book/src/*.md` directly. Those pages carry mdBook-specific
transforms that are wrong in a help file — most visibly `escape_angles_outside_code`
(`generate.py:190`), which is why `book/src/api/btv.buf.md` reads `-&gt;` where the source
says `->`. Every signature would ship mangled. The vimdoc path shares the *extraction*, not
the *rendering*.

Two passes, because links need a complete tag map:

1. **Emit + collect.** Render each chapter to markdown, recording every tag it will define.
2. **Rewrite + generate.** Rewrite cross-page links against that map, then run panvimdoc.

The link rewriting inverts what the book does: `generate.py` rewrites repo-relative links
*to GitHub URLs*, while vimdoc wants them rewritten to `|tags|`. The per-function
`<sub>Defined in [api.lua](…)</sub>` footers (~250 of them) are noise in `:help` — drop
them on this path.

### Tags

The API pages have a clean two-level shape — 58 `#` namespace headings and 320 `##`
function headings — so tags fall straight out:

```
#  `btv.buf`                  → *btv.buf*
## `btv.buf.name(bufnr)`      → *btv.buf.name()*
```

Dotted `btv.*` names are what users actually type and will not collide with anything else on
the runtimepath. Guide and feature chapters get an `bemtvi-` prefix on every section tag
(`*bemtvi-picker*`, `*bemtvi-docks*`) so they do not squat generic topics — the concern
`bemtvi-help/lua/bemtvi-help/helptags.lua` documents at length about its scanner.

Fenced ` ```lua ` blocks become panvimdoc's `>lua` … `<` regions, which the viewer already
renders as real code blocks with per-fence-language tree-sitter highlighting
(`docs/plans/2026-07-24-help-code-block-syntax.md`). The API examples land coloured for free.

### Scope: what ships as `:help`

The book has 84 pages and not all of them are help. Ship:

- the whole `api/` reference — the reason to do this at all;
- `guide/` (getting started, configuration);
- `features/` — user-facing behavior.

Leave on the website: `architecture/overview.md` (crate layout, roadmap), `plugins/testing.md`,
and the appendix. They are contributor docs; putting them in the tag namespace means `:help`
completion offers topics no user is looking for. Revisit per-chapter, not as a blanket rule.

### Ship a generated `doc/tags`

The index derives targets by scanning `doc/*.txt` when no tags file is present. That is fine
for one plugin's single file and wasteful across ~60 — so the emitter writes `doc/tags`
(or the job runs `:BtvHelptags`) and the repo commits it.

## Versioning: tag by version, never by SHA

The docs are generated *from* the commit being built, so their SHA cannot be known at build
time — a SHA pin is a chicken-and-egg. Instead the Action tags `bemtvi-docs` with the bemtvi
version, and the binary pins:

```
{ "bemtvi/bemtvi-docs", tag = <bemtvi version> }        -- env!("CARGO_PKG_VERSION")
{ "bemtvi/bemtvi-help", commit = "<pinned sha>" }      -- independent repo, SHA is fine
```

`btv.plugins` already supports both (`commit`/`tag`/`version` all pin, `commit` wins —
`plugins.lua:202`), and a pin is never auto-updated. The `bemtvi-help` pin lives beside the
existing first-party pins in the style of `build-plugins.sh`'s `PLUGINS` list.

Consequence to accept: a **dev build** between releases has no matching docs tag. Fall back
to the last released tag and say so in the message, rather than fetching `main` — silently
showing docs from a different commit is the drift this whole design exists to avoid.

## Delivery: the bootstrap

The seam already exists. `crates/bemtvi-server/src/excmd.rs:211` is a `:help`/`:h` arm that
fires only when no plugin has registered `:help`, and currently echoes *"add it with
:Plugins (bemtvi/bemtvi-help)"*. That echo becomes the bootstrap; `:helptags` at `:220` gets
the same treatment. When the plugin *is* installed, nothing changes — the user-command arm
below already wins.

The bootstrap itself is Lua in the prelude (`btv._help_bootstrap()`), because installing
plugins is what `btv.plugins` does and this must not become a second installer in Rust.

Four requirements, none optional:

- **Ask once.** Fetching and executing code from the network on a keystroke needs consent.
  Prompt with `btv.ui.confirm`, persist the answer in shada, and expose a config option so
  air-gapped and offline-first users can pre-answer either way and never see the prompt.
- **Fail loud.** Offline, or the fetch rejects: report what it tried to fetch and print the
  exact `btv.plugins` spec to install by hand. Never a `:help` that quietly does nothing —
  that is the no-silent-stubs rule.
- **Async.** A clone on the editor thread would freeze it. This rides `btv.async` like every
  other plugin install, with progress through the same channel `:PluginSync` uses.
- **Idempotent and re-entrant.** Two `:help` calls while the first clone is in flight must
  share one install, not race two clones into the same directory.

After the install lands, the bootstrap adds the runtimepath entries (`btv._add_rtp`), sources
the plugin, and **re-dispatches the original command with its argument** — the user typed
`:help btv.buf.line()` and should get that topic, not a bare help window.

## Three worlds

Per the tier-1 rule this has to work wherever the editor runs.

**Native.** The straightforward path; everything above describes it.

**Daemon.** `remote_config.rs` copies runtimepath files to a local per-process cache because
`require` and runtimepath resolution are synchronous and cannot await the wire. So a
*mid-session* install has to land in that cache and refresh it. The plugin manager's
local-always seams (`btv.fs_local` / `btv.git_local`) were built for exactly this case, so it
is likely already handled — **verify before designing anything**, it is the first thing to test.

**Browser / serverless.** No gix leg. Bake the docs in at build time rather than fetching:
the web build already amalgamates the recommended plugin set into one bundle
(`build-plugins.sh`), and the vimdoc job's output is the input for doing the same with
`doc/*.txt`. A browser session must not print "install the bemtvi-help plugin" — either help
is there or the build is broken.

## The Actions job

In this repo, on release tag (plus a `workflow_dispatch` for manual regeneration):

1. `python3 book/gen/generate.py && python3 book/gen/vimdoc.py`
2. Verify **reproducibility** — regenerate and diff. The plugin convention's freshness check
   depends on byte-identical output; the three post-processing steps in `gen-vimdoc.sh`
   (straight quotes, dropped date line, blank line after the TOC) exist for this reason.
3. Push the generated tree to `bemtvi/bemtvi-docs` and tag it with the release version.

The freshness guarantee cannot be a pre-commit hook here, since generator and output live in
different repos. It is the job's diff step plus a CI check on `main` that the emitter still
runs clean.

## Phases

**Phase 1 — the emitter, API pages only.** `book/gen/vimdoc.py` covering `book/src/api/`,
output reviewed by eye in a real help window. Nothing published, nothing fetched. This is
where the tag scheme and the `-&gt;` class of transform bugs get shaken out.

**Phase 2 — the rest of the chapters.** Guide + features, cross-page links resolving to
`|tags|`, generated `doc/tags`.

**Phase 3 — the Actions job and the `bemtvi-docs` repo.** Publishing and tagging, with the
reproducibility diff. At the end of this phase the docs are installable by hand — a real,
shippable milestone independent of the bootstrap.

**Phase 4 — the bootstrap.** The `excmd.rs` arms, `btv._help_bootstrap()`, consent + shada,
the config option, re-dispatch, and the daemon path.

## Testing

Black-box through the harness, as always. The bootstrap tests must be **hermetic** — no
network. `build-plugins.sh` sets the precedent with `BEMTVI_PLUGINS_BASE` (default
`https://github.com`, overridable to a `file:///` mirror); the bootstrap needs the same
seam so a test can point it at a throwaway local repo. **Verify first** that `btv.git_local`
clones a `file://` URL, since gix is doing the transport rather than the `git` binary.

Cover: the prompt appears and declining installs nothing; accepting installs and the
original topic opens; a rejected fetch produces a loud message naming the manual spec; two
concurrent `:help` calls share one install; and an already-installed plugin never triggers
the bootstrap. For the emitter, assert on generated tags and on the absence of
mdBook-only escaping — not merely that the generator exits 0.

## Non-goals

- **Moving the viewer into core.** `:help` stays an optional plugin; this makes it feel
  built-in, which is the point.
- **Bundling docs in the binary.** Rejected explicitly: it grows every release and pins the
  prose to a build. The whole design is *fetch on demand, pinned to the version*.
- **Auto-updating docs.** A pin is a pin. The docs move when the editor moves.
- **Serving the website from the editor.** Different audience, different rendering; the book
  and `:help` share a source, not a delivery.

## Verify before implementing

Three things this plan assumes and does not confirm:

1. Does `btv.git_local` clone from a `file://` URL? (Hermetic tests depend on it.)
2. Does a mid-session `btv.plugins` install refresh the daemon's local runtimepath cache?
3. Does panvimdoc's auto-tagging produce the tags described above from `##`
   `` `btv.buf.name(bufnr)` `` headings, or does it need explicit tag markers in the
   generated markdown?
