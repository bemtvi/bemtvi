# A static musl TUI build

**Status:** planned (2026-11-25). Not started.

**Decision** (David, 2026-11-25): ship a second Linux TUI binary, statically linked against
musl, as an extra download beside today's glibc build. It carries a few core treesitter
grammars compiled in, and **cannot load new ones**: no `:TSInstall`, no `dlopen`. The glibc
build, macOS, Windows, the GUI and the web build are unchanged. In particular **the glibc
TUI ships exactly as today, with no embedded grammars** (David, 2026-11-25): built-in
grammars exist only in the musl binary.

**Why.** Today's Linux TUI is dynamically linked against glibc 2.17. That covers every glibc
distro since ~2014, but it doesn't run on musl distros (Alpine, distroless-static and scratch
containers, some embedded systems), and a fresh install highlights nothing until `:TSInstall`
has downloaded Zig (~50 MB) and compiled grammars, which needs a network. A static binary with
grammars built in runs anywhere Linux runs and highlights the core languages offline from the
first start.

**Why no grammar loading.** A fully static musl binary has no dynamic loader: `dlopen` fails
with `Dynamic loading not supported` (checked under podman, 2026-11-25: a static
`aarch64-linux-musl` program failed to `dlopen` both a musl and a glibc grammar `.so`).
`:TSInstall`'s whole output is a `.so` to `dlopen`, so it can't work in this binary. Loading
grammars as wasm through tree-sitter's wasmtime runtime would, but that adds a large runtime
to a binary whose point is to be small and simple; it is out of scope.

**What the native build does today, for contrast.** The shipped binaries bundle **no**
grammars: `tree-sitter-rust` / `tree-sitter-md` are only dev-dependencies. Every grammar
comes from `<data>/parser/<lang>.so` (`bemtvi-ts/src/loader.rs`, `libloading`) with its
queries from `<data>/queries/<lang>/` or the runtimepath. A built-in grammar registry is new.

---

## Phase 1 — grammar loading becomes a cargo feature

Make everything that exists only to fetch, compile and `dlopen` grammars optional, behind a
default-on feature (working name `grammar-loading`) threaded `bemtvi-ts` → `bemtvi-server`
→ `bemtvi`:

- `bemtvi-ts`: `loader.rs`'s `libloading` path, `install.rs` (and its `ureq` / `tar` / `flate2`
  / `xz2` / `zip` / `sha2` deps, where nothing else needs them), `generate.rs` (and
  `tree-sitter-generate` / `rquickjs`).
- `bemtvi-server`: the `:TSInstall` / `:TSUpdate` / `:TSUninstall` arms in `excmd.rs` and their
  `btv.treesitter` counterparts.

**Off means loud, not silent** (`CLAUDE.md` → *No silent stubs*). Follow the
`disabled_features` precedent (`E5803`–`E5805`: refuse loudly, by name, with a code): with the
feature off, `:TSInstall lua` echoes an error with the next free code (E5807 as of writing;
check again) saying this build has no grammar loading and naming the grammars it has
built in. The `btv.treesitter` install functions raise the same message. `:TSInstallInfo` still
works and lists the built-in grammars, since it answers a question rather than doing a thing.

Plugins and configs need to ask instead of trying and catching. Before adding anything, look
for an existing capability surface (`btv.version()` today returns a string only). If none fits,
add one small documented field (e.g. `btv.treesitter.can_install()`), not a general
feature-flag registry.

**Verify:** the whole workspace still builds and passes with default features. A new test
binary (or a `--no-default-features`-style CI step for `bemtvi-ts` / `bemtvi-server`) shows
the refusals: each command errors with the code and the built-in list, and the `btv` API
raises. Also confirm the wasm build (`--no-default-features` on `bemtvi-server`) is unaffected,
since it never used the native loader.

## Phase 2 — built-in grammars

A compile-time registry of grammars linked into the binary, behind a second feature (working
name `builtin-grammars`), default **off**: only the musl build enables it. The glibc build
and every other target never turn it on.

**The core set.** To decide before starting. Proposed, picked for "what you edit on a box with
no network": `lua` (bemtvi's own config), `bash`, `markdown` + `markdown_inline`, `json`,
`toml`, `yaml`, `diff`. Measure each grammar's size in the binary before settling: parser
tables vary from ~100 KB to several MB (C++, C#, TypeScript are the big ones, and are left
out for that reason).

**Grammars.** Use each language's crates.io grammar crate (`tree-sitter-lua`, …), which
compiles its `parser.c` / scanner with `cc` and exposes a `LANGUAGE`. Add them as workspace
deps (caret, 2-day rule), optional, pulled in only by `builtin-grammars`.

**Queries.** The highlight / indent / fold / injection / textobject / locals queries come
from the same pinned nvim-treesitter (and -textobjects) refs `:TSInstall` reads, vendored into
the repo (e.g. `crates/bemtvi-ts/builtin/queries/<lang>/*.scm`) and embedded with
`include_str!`. Follow `; inherits:` chains when vendoring (as `:TSInstall` does).

**The real risk: query/grammar version skew.** nvim-treesitter's queries are written against a
specific grammar revision. A crates.io grammar release from another revision can rename a node
and the query fails to compile, or quietly stops matching. Guard it:

- Pin each grammar crate to the release closest to the revision nvim-treesitter's
  `parsers.lua` names at our pinned ref, and record the mapping beside the vendored queries.
- A test compiles **every** embedded query against its built-in grammar and fails on any error.
- Per language, a highlight test on a small sample asserts real captures (a keyword, a string,
  a comment), so a query that compiles but matches nothing is caught too.
- One script re-vendors queries and checks pins, so bumping the nvim-treesitter ref later is
  mechanical.

**Lookup order.** Built-in grammars are found by the same language names. Queries on the
runtimepath (`queries/<lang>/*.scm`) still override or extend the embedded ones, exactly as
for an installed grammar. A user who drops a `highlights.scm` in their config gets it. No
shipped build has both features, so an installed `.so` and a built-in grammar never compete;
if the two features are ever enabled together, fail the build rather than define a precedence
nobody uses.

**Licenses.** Every bundled grammar and the vendored queries (Apache-2.0) go into
`scripts/gen-licenses.sh`: grammar crates through cargo-about, accepting any new license in
`licenses/about.toml`, and the query files listed by hand.

**Verify:** the compile-all-queries test; the per-language highlight tests; folds, indent and
textobjects on the languages whose queries have them (reuse the existing `treesitter_*` suites
with a built-in language instead of a fixture `.so`); a test that a runtimepath query overrides
an embedded one.

## Phase 3 — the musl binary, built and measured locally

- Build: `cargo zigbuild --locked --release -p bemtvi --target x86_64-unknown-linux-musl`
  (and `aarch64-unknown-linux-musl`) with `--no-default-features --features …` chosen to turn
  off `grammar-loading` and turn on `builtin-grammars`. Expect friction in C dependencies: the
  vendored PUC Lua, the vim regex engine (`vim-regex`), tree-sitter, the grammar crates and
  `ring` all compile C or assembly. Zig ships musl, which is why zigbuild is the first thing to
  try. Keep the CI cargo-zigbuild Zig on 0.13.0 if it works (it is held for the glibc floor);
  if 0.13 can't build the musl target, raise that separately.
- `file` must say `statically linked` and `ldd` must say `not a dynamic executable`.
- **Allocator.** musl's `malloc` is known to be slow under multithreaded load, and the server is
  a tokio runtime. Measure first: run the timing suites (`fold_perf`, `extmark_render`,
  `preview_perf`, `open_latency_bench`) and `examples/bench` on glibc vs musl in the same
  podman machine. If musl is clearly slower, add a `#[global_allocator]` (mimalloc or similar)
  for the musl target only, and measure again.
- **Size.** Record the binary size against the glibc TUI, with and without the grammar set,
  so the core-set decision rests on numbers.
- **Smoke in containers** (podman, both arches): `alpine`, `busybox:musl`, `debian:stable-slim`,
  `centos:7`, and a `scratch` image holding only the binary plus a terminfo. In each, run
  `bemtvi --test-plugin` on a small spec that opens a sample of each built-in language and
  asserts highlighting, folds and that `:TSInstall` refuses loudly. Also check by hand in a PTY:
  terminal colors, `:terminal`, the clipboard (OSC 52 is the only path a static binary in a
  container can count on), mouse, an LSP server on `PATH`.

## Phase 4 — CI and release

- `build.yml`: two TUI rows, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, with
  a distinct `kind` (e.g. `tui-musl`) that selects the feature set and an archive name that says
  what it is (e.g. `bemtvi-x86_64-linux-musl.tar.gz`). Same binary name inside, same `LICENSE`
  and generated `THIRD-PARTY-LICENSES.txt`, attested like the rest.
- `test.yml`: a job that builds the musl binary and runs the phase 3 container smoke spec in
  `alpine` and `scratch`, so a regression in static-ness or in a built-in grammar blocks a
  release.
- `edge.yml` builds it too, since it builds every release artifact.
- Docs: the install page and README say which Linux download to pick (glibc: full `:TSInstall`;
  musl: runs anywhere, built-in grammars only, listed). `:version` (or the intro screen) says
  it is the static build and names its grammars. The first release notes carrying it get a
  bullet under `## Packaging` (or similar).

## Open questions

- The core grammar set (phase 2), settled with phase 3's size numbers.
- Whether the remote daemon should also ship as musl. bemtvi's treesitter runs in the
  editing core, never on the daemon (the daemon ships bytes), so a musl daemon needs no
  grammars. It would just be the same static binary started with `--daemon`; check that it
  works in phase 3 and decide whether to advertise it.
