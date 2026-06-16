# Quickfix & errorformat — implementation plan

> **Status: Phases 1–4 complete — the feature is done.**
> nxvim parses `errorformat` (a faithful port of vim's `quickfix.c`) into a
> structured quickfix list, populated via `setqflist` (structured items or
> raw-lines-plus-efm) and `:cbuffer`/`:cgetbuffer`/`:caddbuffer`, read back via
> `getqflist` (Phase 1); browses it in a real, persistent quickfix window
> (`:copen`/`:cclose`/`:cwindow`) with `<CR>`/`:cc`/`:cnext`/`:cprev`/`:cfirst`/
> `:clast` navigation honoring `'switchbuf'` (Phase 2); runs `:make`/`:grep` (async,
> via the job machinery) and the in-process `:vimgrep`, parsing their output into the
> list and jumping to the first error (Phase 3); and now keeps a 10-deep list-history
> **stack** (`:colder`/`:cnewer`), per-window **location lists** with every `:l*` twin,
> `:cfile`/`:cgetfile`/`:caddfile` ingest, and a dogfoodable `nx.qf` Lua surface with
> `setloclist`/`getloclist` (and `nx.diagnostic.setloclist` wired onto a real loclist)
> (Phase 4). This plan built the whole feature: a structured error list populated from
> command output (`:make`, `:grep`) or ingested text (`:cfile`/`:cbuffer`/`:cexpr`),
> parsed against a faithfully-ported vim `errorformat`, browsed in a quickfix window,
> and exposed as a dogfoodable `nx.qf` Lua surface.

## Why this document exists

The quickfix list is one of the last big everyday-vim workflows nxvim is missing:
run a build/lint/search, get a navigable list of `file:line:col` hits, step
through them with `:cnext`/`:cprev`, jump with `<CR>`. It is the connective tissue
between "a tool produced diagnostics" and "I am now editing the offending line".

The non-obvious enabler — and the reason this is worth porting faithfully rather
than approximating — is that **`nxvim-regex` already vendors vim's real
`regexp.c`** (`[[nxvim-regex-vendored-vim-engine]]`). Vim's errorformat is not its
own matcher: `errorformat` strings are compiled by `efm_to_regpat()` into ordinary
vim regex patterns, and the parser pulls fields out of `\1..\9` submatches. Since
we own the identical regex engine and `LineMatch.submatches` already exposes
`\1..\9` as byte ranges (`crates/nxvim-regex/src/lib.rs:222`), a faithful port of
`efm_to_regpat` plugs straight into a matcher with vim-identical semantics. That
is the part where fidelity matters; everything else (the multiline state machine,
the list stack, the window) is plumbing best written natively.

The fail-loud rule applies throughout (`[[dont-conflate-loads-with-works]]`, the
LSP plan's no-silent-stub convention): an `errorformat` directive we don't support
yet **errors with its name**, never silently drops the line; `:make` on the web
build (no native process spawn) **errors loudly**, never pretends to run.

## What's already in place (the seams these phases extend)

- **The vim regex engine.** `VimRegex::compile` / `exec_line` returning
  `LineMatch { start, end, submatches: [Option<(usize,usize)>; NSUBEXP] }`
  (`crates/nxvim-regex/src/lib.rs`). This is what a compiled errorformat runs on.
- **Navigable list panel (prior art).** `panel.rs` already backs the LSP
  diagnostics location-list: `set_panel_targets(Vec<Option<(PathBuf, usize,
  usize)>>)`, `set_panel_lines`, `handle_panel` jumping via
  `Editor::jump_to(&path, line, col)` on `<CR>`. The quickfix window needs
  *persistent real-window* semantics rather than the transient panel (see the
  Phase 2 decision), but the entry→target→jump mechanism is proven here.
- **`jump_to(&path, line, col)`** (`panel.rs` / `editor/mod.rs`) — open-or-reuse a
  window on a file and place the cursor. The jump primitive for `<CR>`/`:cc`.
- **Ex-command dispatch.** The big `match cmd { … }` in
  `crates/nxvim-core/src/editor/ex.rs` (~line 410+) is where `:copen`, `:cnext`,
  `:make`, etc. register, alongside `:ls`, `:terminal`, `:split`.
- **Job / process spawn.** `nx.spawn` / `vim.system` / `jobstart` stream stdout
  through `effects.rs` (`proc_spawn`, drained via `apply_lua_effects`); native
  only — the web path already **errors loudly** that jobs require a daemon
  (`effects.rs:1196`). `:make`/`:grep` ride this exact machinery.
- **Options plumbing.** `editor/options.rs` + `:set`; new string options
  (`errorformat`, `makeprg`, `grepprg`, `grepformat`, `switchbuf`) slot in here.
- **`nx.fn` surface.** `crates/nxvim-lua/src/prelude/vimfn.lua` is where
  `setqflist`/`getqflist`/`setloclist`/`getloclist` land; `nx.diagnostic.lua`'s
  `setloclist` (`diagnostic.lua:130`) shows the Lua→`*Op`→core round-trip to copy.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 1 — errorformat engine + core list model ✅

> **Done.** `crates/nxvim-core/src/editor/quickfix.rs` ports `efm_to_regpat` +
> the `qf_parse_line` state machine (multiline `%A/%C/%Z`, `%-`/`%+` flags, `%>`,
> `%D`/`%X` dir stack, the `%f %l %c %t %n %m %p %v %s %o …` field codes) onto the
> `nxvim-regex` engine; `QfEntry`/`QfList` live on the `Editor` behind `qf_list` /
> `qf_set_items` / `qf_set_from_lines`. The `'errorformat'` option (default
> `DFLT_EFM`) is wired through `:set`/`vim.o`. Ingest: `:cbuffer`/`:cgetbuffer`/
> `:caddbuffer`; Lua bridge: `vim.fn.setqflist` (op) + `vim.fn.getqflist`
> (`nx._qflist` mirror). Covered by `crates/nxvim-server/tests/quickfix.rs` (8
> tests: gcc single-line, `%t`/`%n` codes, multiline fold, `%D`/`%X` resolution,
> structured round-trip, append action, `:cgetbuffer`, and an `E377` fail-loud).
> Known simplifications deferred: `%f` env-expansion, `%b` buffer-existence and
> `%O`/`%P`/`%Q` file-existence checks, and vim's fuller `qf_push_dir` path
> resolution.

**Goal:** turn `(lines, errorformat)` into a `Vec<QfEntry>`, faithfully. No window
yet — exercised end-to-end through a minimal ingest + introspection surface.

**Work**

- New module `crates/nxvim-core/src/editor/quickfix.rs` (and `QfEntry` / `QfList`
  types). `QfEntry { filename: Option<PathBuf>, lnum: usize, col: usize, vcol:
  bool, text: String, kind: char /* 'E' 'W' 'I' ' ' */, nr: i32, valid: bool, …}`
  mirroring vim's `qfline_T` fields that callers actually read.
- **Port `efm_to_regpat()` from vim `quickfix.c`** to Rust — the conversion of one
  errorformat pattern into a vim regex string. Cover the field codes
  `%f %l %c %t %m %n %r %p %v %s %o`, the literal escapes (`%%`, `%\`, `%.`, `%#`,
  `%*`, `%[`, `%]`), and the prefix/control codes `%E %W %I %A %C %Z %G %O %P %Q
  %D %X %> %+ %-`. Emit a `VimRegex`-compatible pattern; compile with
  `VimRegex::compile`. **Any code we don't yet emit must `error!` with the literal
  directive** — never skip it silently.
- **Port the parse loop** (`qf_parse_line` / `qf_init_ext` core) as a Rust state
  machine over the multiline prefixes: single-line `%E/%W/%I`, continuation `%C`,
  end `%Z`, generic multiline `%A`, ignore/general `%G`, push `%>`,
  directory-stack `%D/%X` for `make[1]: Entering directory` resolution.
- **Default `errorformat`** seeded from vim's compiled-in default; `'errorformat'`
  as a string option in `options.rs`.
- **Minimal ingest + introspection (so Phase 1 is testable end-to-end):**
  `:cgetexpr`/`:cexpr {expr}` and `:cgetbuffer`/`:cbuffer [N]` to fill the list
  from a Lua string list or a buffer; `nx.fn.setqflist(list)` and
  `nx.fn.getqflist()` round-tripping the structured entries. (`:cexpr` opens the
  window in vim — defer that coupling to Phase 2; here it only fills + lets you
  read back.)

**Tests** (`crates/nxvim-server/tests/quickfix.rs`, black-box): feed canonical
compiler output via `setqflist({lines=…, efm=…})` / `:cgetbuffer`, assert the
parsed entries via `getqflist()` — gcc `file:line:col: error: msg`, a multiline
`%A/%C/%Z` Java-style trace, a `%D/%X` directory-change build, the `%t`/`%n`
type/number codes, and an **unsupported directive erroring loudly**.

**Deferred from this phase:** the window, navigation, `:make`, location list.

---

## Phase 2 — Quickfix window + navigation ✅

> **Done.** Chose the **real, persistent window** (vim's model), tracked by
> `Editor::qf_bufnr` (the display buffer's id) with no `Buffer` struct change.
> `:copen`/`:cclose`/`:cwindow` open/focus/close a horizontal split showing the
> rendered list (`file|lnum col N| text`, refreshed on every list change via
> `qf_refresh_window`). The window is an ordinary window onto a `nomodifiable`
> buffer: `modifiable()` is false for the quickfix buffer, so edits are refused
> with `E21` at the existing chokepoints (the same mechanism terminal buffers use),
> and every other normal-mode key — motions, search, `<C-w>…`, `:` — works
> natively. `Editor::input` special-cases only `<CR>`, which jumps to the entry on
> the cursor's line. (An earlier draft routed all keys through an explorer-style
> allowlist; that was replaced with this faithful `modifiable()` approach.)
> Navigation: `:cc [nr]`, `:cnext`/`:cprev` (count + `E553`, skipping invalid
> entries), `:cfirst`/`:clast`/`:crewind`, all updating `qf.idx` and landing the
> cursor via `jump_to` in the source window per `'switchbuf'` (`split`/`vsplit`
> honored; `newtab`/`usetab` not yet). Covered by 6 black-box tests in
> `tests/quickfix.rs` (render, `<CR>` jump, `:cc`/`:cnext`/`:cprev`, `E553`,
> `:copen`/`:cclose` window count, `nomodifiable`/`E21`, and bottom placement).
> `:copen` opens a full-width window at the bottom (vim's `botright`, via
> `open_bottom_window` which wraps the layout root), `10` rows by default and
> honoring `:copen [height]`. Deferred to Phase 3: `:cfile`/`:cgetfile` (file I/O
> belongs with the `:make`/job machinery so the core stays pure) and
> `:cnfile`/`:cpfile`.

**Goal:** `:copen` shows the list; `<CR>`/`:cc`/`:cnext`/`:cprev` navigate and jump.

**Design decision — window vs panel.** The diagnostics *panel* (`panel.rs`) is
transient (closes on jump, single floating/docked surface). The quickfix window is
a **persistent real window** with a special buffer: it stays open across jumps, you
`<C-w>` in and out of it, and `:cclose` dismisses it. **Recommendation:** model the
list as data on `Editor` and render it into a dedicated special buffer
(`buftype=quickfix`, unlisted, unmodifiable) shown in a normal split — reusing the
entry→target→`jump_to` mechanism proven in `panel.rs` but **not** the transient
panel lifecycle. (Confirm before building: this is the one real fork in the plan.)

**Work**

- `:copen [height]` / `:cclose` / `:cwindow` (open iff non-empty) / `:cc [nr]`
  (jump to entry) — registered in `ex.rs`.
- Render: one line per entry, vim's `file|lnum col| text` format; current entry
  highlighted; `<CR>` and `<2-LeftMouse>` jump via `jump_to`, honoring
  `'switchbuf'` (`useopen`/`split`/`vsplit`/`newtab`).
- Navigation: `:cnext`/`:cprev`/`:cnfile`/`:cpfile`/`:cfirst`/`:clast`, with
  count and wrap/`E553` (no more items) behavior; `:cc` re-jump to current.
- `:cfile`/`:cgetfile {file}` ingest (reads a file off disk through the parser).
- Re-render the qf buffer whenever the list is replaced.

**Tests:** `:cgetbuffer` then `:copen` → assert the rendered lines and that the qf
buffer is `buftype=quickfix`/nomodifiable; `:cnext`×N then `<CR>`/`:cc` lands the
cursor on the right `file:line:col`; `:switchbuf=split` opens a split; `E553` at
the end of the list.

**Deferred:** `:make`/`:grep`, location list, the multi-list stack.

---

## Phase 3 — `:make` / `:grep` (async producers) ✅

> **Done.** New options `'makeprg'` (default `make`), `'grepprg'` (default
> `grep -n $* /dev/null`), `'grepformat'` (default [`DFLT_GREPFORMAT`]) wired through
> `:set` *and* `vim.o`/`nx.o` (added to the prelude's `O_GLOBAL` + the `GoMirror`
> push so the values round-trip — this also closed the latent gap where
> `vim.o.errorformat`/`switchbuf` never reached the core). `:make[!]`/`:grep[!]` live
> server-side (`EditHost::ex_make`, `excmd.rs`): they expand `'makeprg'`/`'grepprg'`
> (`$*` → args, else appended), wrap the result in `sh -c '<cmd> 2>&1'` (vim's
> `'shellpipe'` stderr-merge, so the directory-stack / multi-line matchers see one
> ordered stream), and hand it to the Lua producer `nx._qf_make`. That dogfoods the
> existing job machinery (`nx._system_async`): on the child's exit its combined
> output is split and passed to a new `nx._qf_populate` Rust bridge, which queues a
> `QfSetOp` carrying two new post-populate flags (`open` → `:cwindow`, `goto_first`
> → `:cfirst`); the server applies them via `Editor::qf_post_populate` after parsing.
> A no-bang run jumps to the first valid entry; `!` parses + opens but doesn't jump.
> On the web build (no local spawn) the underlying spawn op fails loud, exactly like
> `vim.system`. `:vimgrep[!] /{pat}/[g][j] {file}…` (and `:vimgrepadd`) is the
> in-process path (`Editor::ex_vimgrep`, core): it compiles the pattern through the
> active `'regexsyntax'` engine (`SearchRegex`, honoring `'ignorecase'`/`'smartcase'`
> like `/`), reads each file (a loaded buffer's live contents if present, else off
> the host fs), adds one entry per matching line — or per match with `g` — and jumps
> to the first unless `j`; it needs no process, so it runs on every build. Covered by
> 8 black-box tests in `tests/quickfix.rs` (vimgrep match+jump, the `g` flag,
> `:vimgrepadd` append, glob-arg fail-loud; `:make` populate+open+jump via a `printf`
> makeprg, `:make!` no-jump, `:grep` via `'grepprg'`/`'grepformat'`).
> **Deferred to Phase 4:** the location-list twins (`:lmake`/`:lgrep`/`:lvimgrep`),
> which need the per-window list infrastructure that phase builds; and `:vimgrep`
> file globbing (`**/*.rs`), which fails loud today.

**Goal:** run a build/search command, capture output, parse, populate, jump to the
first error.

**Work**

- `'makeprg'` (default `make`), `'grepprg'` (default `grep -n $* /dev/null` /
  platform), `'grepformat'` options in `options.rs`.
- `:make[!] [args]` — expand `'makeprg'` (`$*` substitution), spawn via the
  existing `nx.spawn`/`jobstart` machinery in `effects.rs`, capture combined
  stdout+stderr, parse against `'errorformat'`, fill the list, `:copen` iff there
  are errors, and (without `!`) jump to the first valid entry.
- `:grep[!]`/`:lgrep[!]` and an internal `:vimgrep` (the latter uses `VimRegex`
  directly over buffers/files — no external process — and is the web-safe path).
- **Async + fail-loud on web.** External `:make`/`:grep` route through the
  job/effects pipeline; on the web build (no native spawn) they **error loudly**
  exactly like `vim.system` already does (`effects.rs:1196`) — never a silent
  no-op. `:vimgrep` works everywhere since it needs no process.

**Tests:** drive `:make` against a tiny fixture `makeprg` (a script echoing
gcc-style errors) — assert the list fills, the window opens, the cursor lands on
the first hit (native, possibly `#[ignore]`d if it needs a real shell per
`[[tests-must-be-hermetic]]`); `:vimgrep /pat/ file` populates and navigates with
no external process (runs everywhere).

**Deferred:** location list, list stack.

---

## Phase 4 — Location list + list stack + `nx.qf` API ✅

> **Done.** The single `Editor::quickfix` list became a `QfStack` (vim's up-to-10
> list history; `:colder`/`:cnewer` walk it via `Editor::ex_qf_history`, `E380`/`E381`
> at the ends), and `setqflist`'s `' '`/`'a'`/`'r'` actions gained their real divergent
> meaning (push-new / append / replace-current). **Location lists** are per-window:
> each [`Window`] carries an optional `loclist: QfStack` + `loclist_bufnr` (its display
> buffer). The whole window/navigation/populate surface was parameterized by a new
> `QfWhich { Quickfix, Location(WindowId) }` so every `:c*` shares one implementation
> with its `:l*` twin (`ex_qf_open`/`close`/`window`/`cc`/`step`/`first`/`last`/
> `history`); `:lopen` mints a per-window display buffer whose owner is found by the
> unique window holding that `loclist_bufnr`, and `<CR>`/`:ll` jump back into that owner
> (`qf_focus_target_window`). `:lvimgrep`/`:lgrep`/`:lmake` reuse the Phase 3 producers
> with a loclist target threaded through `QfSetOp::loclist_win` (`Some(0)` = current
> window at drain time). `:cfile`/`:cgetfile`/`:caddfile` (+ `:l*`) read a file off the
> host fs and parse it (`Editor::ex_cfile`). Location lists are **strictly per-window**
> (a split inherits a *clone*, not vim's shared-by-reference list — a documented
> divergence). The Lua surface: `nx.setloclist`/`nx.getloclist` (+ `vim.fn` aliases) over
> a per-window `nx._loclist` mirror, the canonical `nx.qf` namespace, and
> `nx.diagnostic.setloclist`/`setqflist`/`toqflist` rebuilt to fill a real, navigable
> loclist (bufnr-addressed entries now resolve their file at jump time). Covered by 7
> new black-box tests in `tests/quickfix.rs` (window-scoped set/getloclist,
> `:colder`/`:cnewer` + `E380`, the `'r'` action, `:lvimgrep`, the read-only loclist
> window + `<CR>` jump, `:lmake` loclist-scoping, and `nx.diagnostic.setloclist`
> navigability), plus a runnable `examples/quickfix/`.
>
> **Follow-ups since (also Phase 4):** `%`/`#` (current/alternate file) expansion in
> the `:vimgrep`/`:lvimgrep` and `:cfile`-family file arguments (the shared
> `expand_file_arg`); `:cnfile`/`:cpfile` (+ `:l*`) step-by-file navigation
> (`Editor::ex_qf_step_file`, grouping entries by a filename/bufnr key); and
> closing a loclist owner window now also closes its loclist window and drops the
> orphaned display buffer (`Editor::discard_loclist_display`, hooked in
> `remove_window`). **Deferred:** `:vimgrep` file *globbing* (`**/*.rs`, fails loud),
> `:cexpr`/`:cgetexpr`/`:caddexpr` (intentionally not added — nxvim's `:echo`-grade
> expression evaluator can't evaluate the `system()`/variable expressions that make
> `:cexpr` useful, and the Lua `setqflist({lines=…})` path already covers it more
> capably), and the out-of-scope items below.

**Goal:** window-local location lists, the 10-deep history stack, and the
dogfoodable Lua surface (`[[dogfood-the-nx-plugin-api]]`).

**Work**

- **Location list:** every `:c*` gets its `:l*` twin (`:lopen`/`:lnext`/`:lgrep`/
  `:lmake`/`:lfile`/…), scoped to the current window rather than global. Share the
  Phase 1–3 engine; the only difference is ownership (per-window vs per-tab-global)
  and that `<CR>` jumps in the window the loclist belongs to.
- **List stack:** vim keeps the last 10 quickfix lists; `:colder`/`:cnewer`
  (`:lolder`/`:lnewer`) walk it; `setqflist` `action` `'a'` (append) / `'r'`
  (replace) / `' '` (new list) and `what = { title, nr, items, … }`.
- **`nx.qf` surface** over `nx.fn.setqflist`/`getqflist`/`setloclist`/`getloclist`
  — the canonical API; `vim.fn`-style aliases (whitelist per ADR 0002) delegate to
  it. Wire `nx.diagnostic.setloclist` (`diagnostic.lua:130`, today a stub-ish
  bridge) to populate a real location list now that one exists.
- `examples/quickfix/` runnable config + sample erroring file, verified end-to-end
  (`[[example-config-for-testing]]`).

**Tests:** `setloclist`/`getloclist` round-trip is window-scoped (two windows hold
independent lists); `:colder`/`:cnewer` restore prior lists; `setqflist` `'a'`/`'r'`
actions; `nx.diagnostic.setloclist()` fills a navigable loclist from real
diagnostics.

---

## Out of scope (call out, don't silently omit)

- `quickfixtextfunc` / `quickfix` filetype autocommands, `:cdo`/`:cfdo`/`:ldo`,
  `:cexpr` expression-from-`:caddexpr` accumulation beyond the `action` flag,
  conceal-based qf formatting. Each, if requested later, extends this engine — none
  is load-bearing for the core workflow. Whatever ships unimplemented stays a
  **documented approximation that errors loudly**, never a silent no-op
  (`[[dont-conflate-loads-with-works]]`).
