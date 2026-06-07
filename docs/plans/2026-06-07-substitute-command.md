# Implementing `:substitute` (`:s`)

> **Status: Phases 0–4 DONE.** The ex-range parser
> (Phase 0), the core substitute command (Phase 1: `g`/`i`/`I`/`n` flags, count,
> `\r` line-split, capture refs, count reporting, single-undo), pattern/replacement
> reuse (Phase 2: empty-pattern reuse, bare `:s` / `:&` / `:&&` repeat with the
> flag-reset-vs-keep distinction, `~` replacement recall, trailing count,
> alternate delimiters), the interactive `c` confirm flag (Phase 3:
> `y`/`n`/`a`/`l`/`q`/`<Esc>` per-match prompting), and `:global` / `:vglobal`
> (Phase 4, with the `:delete` / `:print` it drives) are all implemented and
> tested. Phased and TDD-driven per the project workflow.
>
> **Note on file paths below.** This plan was written against the pre-refactor
> single-file `crates/nxvim-core/src/editor.rs`. That file has since been split
> into per-concern modules under `crates/nxvim-core/src/editor/`; the substitute
> code now lives in `editor/ex.rs` (range parsing, `ex_substitute`,
> `run_substitute`, and the `SubstConfirm` confirm walk) with the confirm-key
> intercept in `editor/mod.rs`. Line numbers in the prose are historical.

## Why this document exists

nxvim has `/` and `?` search (canonical-regex, line-by-line — see
`crates/nxvim-core/src/search.rs`) but no `:s`. Today a `:1,5s/foo/bar/`
doesn't even parse: `execute_ex` (`editor.rs:6205`) treats a bare number as a
"jump to line N" and otherwise splits off an *alphabetic* command name, so a
leading range like `1,5s…` produces an empty command name and is silently
deferred to the server as an unknown command. So `:s` needs two things that
don't exist yet:

1. **Ex range parsing** — `.`, `$`, `%`, line numbers, `+N`/`-N` offsets, marks,
   and `lo,hi` pairs, resolved against the current cursor. This is *foundational*
   infrastructure that every range-taking command (`:d`, `:m`, `:t`, `:g`, `:>`)
   will reuse, so it's built as its own layer, not inline in `:s`.
2. **The substitute command itself** — parse `/pat/rep/flags [count]`, match each
   line in the range, replace, report the count, set up `&`-repeat and the search
   register, place the cursor like vim does.

## Design decisions

**Dialect: canonical regex, matching `/` search — *not* vim magic.** nxvim made a
deliberate choice (documented at the top of `search.rs`) that `/` speaks the
`regex` crate's PCRE-style dialect: `+ ? * ( ) | { } [ ] ^ $ .` are operators,
a leading `\` escapes to literal, inline `(?i)`/`(?-i)` for case. `:s` shares
the **same engine and dialect** by compiling its pattern through the existing
`SearchRegex::compile`. The vim-magic translation layer in
`crates/nxvim-lua/src/vimregex.rs` is a `vim.fn.substitute` Lua-compat shim and
stays walled off there — it does **not** back the editor's `:s`.

**Replacement syntax: regex-crate `$`-captures + a backslash-escape pass.**
Since the pattern is canonical regex, capture references are too — PCRE-style
`$`-expansion. On top of that we run a backslash-escape pass so a replacement can
insert control characters (most importantly a newline, to *split* a line):

| Syntax | Meaning |
|---|---|
| `$0` / `${0}` | whole match |
| `$1`, `${1}`, `$name` | capture group (numbered or named) |
| `${1}` form | needed to disambiguate `${1}foo` from `$1foo` |
| `$$` | a literal `$` |
| `\r` | **newline** — splits the line in two (vim's convention; this is the useful one) |
| `\n` | newline (we map it to newline too, not vim's NUL — more intuitive, consistent divergence) |
| `\t` | tab |
| `\\` | a literal backslash |

Because `$`-captures and `\`-escapes don't collide, `substitute_line` does a
**single custom expansion loop** over the replacement (handling both) rather than
calling `Captures::expand`, which knows only `$`. The `\r`/`\n` cases emit a real
newline into the spliced text; the `Buffer` insert + `normalize()` handle the
resulting extra lines (the rope doesn't care that one "line edit" introduced
newlines).

This is a **deliberate divergence from vim**, exactly parallel to the search
divergence already documented in `search.rs`: vim users would type `\1`/`&` for
captures (we use `$1`/`$0`) and rely on `\n`→NUL (we use `\n`→newline). The `:s`
implementation gets a module-level doc comment stating this, mirroring
`search.rs`.

**Fail loud — no silent errors (project rule).** Every malformed or unsupported
input errors with a named message via `echo`, never silently no-ops or guesses:
an invalid pattern (`E383`), an unknown mark in a range (`E20`), a `lo > hi`
range (error — we do **not** silently swap the way an interactive vim prompt
would), an unterminated/empty delimiter spec, an **unrecognized flag**, and the
not-yet-built `c` flag (until Phase 3, it errors that confirm is unimplemented
rather than being ignored). A `:s` that can't do exactly what was asked says so.

**Undo: one snapshot per command, not per line.** `:%s/…` is a single `u`. Push
one `push_undo()` at the start of the substitute, then apply all line edits.

**Edits go through the `Buffer` API.** Replace a line (or a matched span) via
`Buffer::remove(range)` + `Buffer::insert(byte, text)`, then `normalize()` to
keep the trailing-`\n` invariant. `changedtick`/`modified` are bumped by those
calls; the treesitter/LSP edit journal is fed automatically.

## Grammar targeted

```
:[range]s[ubstitute]/{pat}/{rep}/[flags] [count]
```

- **range** (Phase 0): `.` `$` `%` `N` `'m` `+N` `-N` and `lo,hi`; default = current line.
- **delimiter**: any non-alphanumeric, non-blank char after `s` (`/`, `#`, `,`, …).
  The first char after `s` chooses it; vim allows `:s#a#b#`. `\` and `"` excluded.
- **pat**: canonical regex; empty `pat` reuses the last search/substitute pattern.
- **rep**: native `$`-expansion; empty `rep` = delete the match. `~` = previous rep
  (Phase 2, optional).
- **flags**: `g` (every match on the line, not just first), `i`/`I` (force ignore /
  match case for this command), `n` (report match count, make no changes),
  `c` (confirm — prompt per match, Phase 3), `&` (keep previous flags — optional).
- **count**: trailing number → apply to `count` lines starting at the range's
  *last* line (vim semantics).
- **bare `:s`** with no pattern → repeat the last substitute on the current line.

## Phase 0 — Ex range parsing (foundational, reusable)

Add a range parser in `editor.rs` (near `split_ex`, `editor.rs:7573`). Signature
sketch:

```rust
/// A resolved, 0-based, inclusive line range plus the rest of the command.
struct ExRange { lo: usize, hi: usize, explicit: bool }

/// Consume any leading range from `cmd`, resolving it against the cursor and
/// buffer. Returns the resolved range (defaulting to the current line when none
/// is present) and the remaining command text (name + args). `None`/Err on a
/// malformed range or an out-of-range mark.
fn parse_ex_range(&self, cmd: &str) -> Result<(ExRange, &str), String>;
```

Address atoms to support now: `N` (absolute, 1-based in source), `.` (current
line), `$` (last line), `'m` (mark — resolve via the existing mark store; error
`E20: Mark not set` if absent), and trailing `+N` / `-N` offsets on any atom.
A range is `addr` or `addr,addr` (also `addr;addr`, where `;` moves the cursor to
the first address before resolving the second — can defer the `;` form). `%`
expands to `1,$`. Clamp results to `[0, last_line()]`; if `lo > hi`, vim prompts
to swap — for now swap silently or error (decide in review).

`execute_ex` changes: run `parse_ex_range` first, then `split_ex` on the
remainder. The existing "bare number jumps to line" shortcut becomes a *range
with an empty command* (range present, no command name → move cursor to `hi`,
preserving today's behavior). Commands that don't take a range ignore the parsed
range as they do today; range-aware commands (`:s` now, others later) read it.

**Tests (Phase 0)** — drive through `:` so they're black-box, e.g. assert that
`:3<CR>` and `:$<CR>` and `:.+2<CR>` move the cursor correctly (these exercise
the parser without needing `:s` yet).

## Phase 1 — Substitute core (single line + range, `g` / `i` / `I` / `n`)

1. Add `"s" | "su" | "sub" | "subs" | … | "substitute"` arms to the `execute_ex`
   match, dispatching to `fn ex_substitute(&mut self, range: ExRange, args: &str)`.
2. Parse `args`: read the delimiter, split into `pat` / `rep` / `flags` (an
   unescaped delimiter separates fields; a trailing delimiter is optional, so
   `:s/foo/bar` works). Then split a trailing ` count`. **Validate flags**: any
   character outside the known set (`g i I n c &`) errors loudly
   (`E486`-style "Trailing characters" / "unknown flag") — never ignored.
3. Resolve `pat`: empty → last pattern (see Phase 2); compile via
   `SearchRegex::compile(pat, self.search_ignorecase(pat))`, honoring `i`/`I`
   overrides. On error, `echo` the `E383`-style message and stop.
4. Add a substitute method to `SearchRegex` so the `Regex` stays encapsulated in
   `search.rs`:
   ```rust
   /// Replace matches in `line` using regex-crate `$`-expansion. With `global`
   /// false, only the first match. Returns the new line and the match count.
   pub(crate) fn substitute_line(&self, line: &str, rep: &str, global: bool)
       -> (String, usize);
   ```
   (Implemented with `Regex::replacen` / `replace_all` + a count, or a manual
   `captures_iter` + `expand` loop.)
5. `push_undo()` once. For each line in `[lo, hi]`: compute the new text; if it
   changed, splice it in via `remove`+`insert`; tally substitutions and changed
   lines. `normalize()` at the end. If zero matches across the whole range,
   `echo("E486: Pattern not found: {pat}")` and (vim) leave the buffer untouched
   — no undo entry should linger, so only `push_undo()` *after* confirming ≥1
   match, or roll back.
6. **Report**: more than one change → `echo(format!("{subs} substitutions on
   {lines} lines"))`; vim stays silent for a single substitution on one line.
   The `n` flag counts and reports but makes **no** edits and pushes no undo.
7. **Cursor**: vim leaves the cursor on the **last line changed**, at its first
   non-blank. Set `self.cursor.line = last_changed; col = first_non_blank(...)`,
   then `clamp_cursor()`.
8. Record this substitute as the last one (pattern + rep + flags) for `&` /
   bare-`:s` repeat, and set `last_search` to the pattern so `n` and `hlsearch`
   pick it up (matches vim: `:s` sets the search register).

**Tests (Phase 1)** — in `crates/nxvim-server/tests/editing.rs`:
- current-line `:s/foo/baz<CR>`;
- `:%s/foo/bar/g` across multiple lines;
- range `:1,2s/…`;
- `g` flag (all matches on a line) vs default (first only);
- capture replacement `:s/\(\w\+\) \(\w\+\)/$2 $1/` → confirms `$1`/`$2` (and the
  documented divergence from vim's `\1`);
- empty replacement deletes the match;
- `\r` in the replacement splits one line into two (e.g. `:s/, /\r/g` turns a
  comma-list into separate lines) and the line count grows accordingly;
- an unknown flag (`:s/a/b/z`) errors loudly and changes nothing;
- `i`/`I` case override;
- `n` flag reports count and leaves the buffer unchanged;
- no-match reports `E486` and leaves buffer + cursor put;
- the `N substitutions on M lines` message (assert via `redraw_after` + `message`
  field, using `drain_to_latest_redraw` per the harness race note);
- one `u` undoes a whole `:%s`.

## Phase 2 — Pattern reuse, repeat, count, delimiters — DONE

- **Empty pattern** `:s//rep/` reuses the last search/substitute pattern
  (`last_search`); `E35` when none exists.
- **Bare `:s` / `:&` / `:&&`** repeat the last substitute on the range (default:
  current line). `:s` and `:&` **reset** the flags (only freshly typed flags
  apply); `:&&` **keeps** the previous flags (then layers on any new ones) — the
  vim-faithful distinction, not the looser "same flags" this doc first sketched.
  `:s g` / `:& g` / `:&& 3` all accept fresh flags/count. `E33` when there is no
  previous substitute. Stored as `last_substitute = (pattern, replacement, flag
  letters)`; the substitute is recorded even when it matched nothing, so a
  following `:&` still repeats it (matches vim). `:&`/`:&&` are dispatched off the
  raw command remainder (they have no alphabetic name for `split_ex`).
- **`~` in replacement** = the previous replacement string (`\~` is a literal
  tilde); `E33` when there is no previous replacement. Expanded by `expand_tilde`
  in `editor.rs` *before* the new substitute overwrites `last_substitute`; other
  backslash escapes pass through to `substitute_line`'s expansion pass untouched.
- **Trailing count** `:s/a/b/ 3` → apply to 3 lines from the range's last line.
- **Alternate delimiters** `:s#a#b#`, `:s,a,b,` — handled by the delimiter-agnostic
  Phase-1 parser; covered by explicit tests now.

Implemented as a three-way split in `editor.rs`: `ex_substitute` dispatches a
literal `/pat/rep/flags` spec, a bare/flag-only repeat, or (via `execute_ex`) the
`&`/`&&` forms; `repeat_substitute` rebuilds the spec from `last_substitute`; and
`run_substitute` is the shared engine both call.

## Phase 3 — Confirm flag `c` — DONE

The `c` flag walks the matches one at a time, prompting `replace with {rep}
(y/n/a/l/q/^E/^Y)?` before each. Rather than reuse `Mode::Command` (the cursor
stays *in the buffer*, on the match, not on a command line), it adds a small
paused-walk state to the editor:

- **`SubstConfirm`** (in `editor/ex.rs`) holds the compiled regex, the expanded
  replacement, the `g` flag, the live `[.., hi]` bounds, the current scan
  position (`line`/`byte`), the match awaiting an answer (`cur`), the running
  `subs`/`nlines` tallies, `last_changed`, and a lazy `pushed` (the single undo
  snapshot is taken on the *first* applied match — an all-skipped confirm pushes
  no undo, so a later `u` reverts the prior edit, matching vim).
- **`Editor::subst_confirm: Option<SubstConfirm>`** (in `editor/mod.rs`). While
  `Some`, `Editor::input` routes every key to `subst_confirm_key` *ahead of* the
  mode dispatch (the same pattern the focused panel uses), so the answer never
  reaches normal-mode handling and the prompt message survives between keys.
- The prompt is set straight onto `Editor::message` (not via `echo`), keeping the
  transient question off the `:messages` history, like vim.

Answer keys: `y` substitute + advance, `n` skip + advance, `a` substitute this
and all remaining with no further prompts, `l` substitute this one then stop,
`q`/`<Esc>` stop without it, and `^E`/`^Y` scroll the window one line to peek
around the match (leaving the prompt up, the match unconsumed). `n` (count-only)
still takes precedence over `c` (a counting pass never prompts); any other key
leaves the prompt up (vim beeps).

The `^E`/`^Y` peek reuses the normal-mode one-line scroll (`<C-e>`/`<C-y>`, added
alongside this — `Editor::scroll_line` in `editor/cursor.rs`). One nxvim-specific
wrinkle: nxvim re-runs `ensure_visible` on every redraw, so it can't let the
cursor scroll off the match the way vim does. Instead the cursor rides the view
to stay on screen during the peek — harmless, because the pending match lives in
the confirm state (`cur`), not in the cursor, so the eventual `y`/`n` still lands
on the right match.

`SearchRegex::match_replacement(line, from, rep)` (in `search.rs`) is the single
primitive the walk steps on: the next non-overlapping match at-or-after `from`
plus its expanded replacement.

**Cursor with a line-splitting replacement.** When `\r` splits a line, later
lines shift down. Applying a match bumps `hi` and the continuation `line` by the
number of newlines the replacement introduced, and sets the continuation `byte`
to just past the replacement's final segment — so a global confirm keeps matching
the rest of the original line on its new (pushed-down) row. The cursor lands on
the first non-blank of the last line produced by the last substitution. A
zero-width match is force-stepped one grapheme so the walk can't spin.

## Phase 4 — `:global` / `:vglobal` (and the `:delete` / `:print` it drives) — DONE

`:[range]g[!]/{pat}/{cmd}` runs an ex command on every line of the range matching
`{pat}`; `:g!` and `:v` / `:vglobal` invert to the non-matching lines. The
**range defaults to the whole file** (not the current line), an empty pattern
reuses the last search, and an empty command defaults to `:print` (vim).

The engine (`ex_global` in `editor/ex.rs`) is vim's two-pass **mark then execute**:

1. **First pass** collects the target line indices (matching, or non-matching for
   the inverted forms) up front, so edits in pass two can't disturb the selection.
2. **Second pass** runs `{cmd}` on each target via the normal `execute_ex`
   dispatch (so `:g/re/s/…/…/`, `:g/re/d`, `:g/re/p` all just work), with the
   cursor pre-positioned on the line. A running `offset` tracks the buffer's
   line-count change after each command and shifts the remaining targets, so a
   command that deletes or splits lines keeps the rest aligned. This handles the
   line-wise commands (`:d`, `:s`, `:p`) that cover the overwhelming majority of
   `:g` use; a command that non-locally restructures many lines at once is a known
   edge the simple offset model doesn't perfectly track.

The whole `:g` is **one undo step**: force a fresh `push_undo`, set `snapshot_taken`
to suppress the per-command snapshots through the batch, then clear it so the next
edit snapshots on its own. A nested `:g`/`:v` in `{cmd}` fails loud (`E147`) via an
`in_global` re-entrancy flag. `:g` with no match reports `E486` (like `:s`); `:v`
with no non-matching lines is a silent no-op (the pattern was found, so it's not a
miss). `:g` sets the last-search pattern, like `:s`.

Two range commands were added as the per-line targets (independently useful):
`:[range]d[elete]` removes the range's lines (default current line; lands the
cursor on the line that replaces them), and `:[range]p[rint]` echoes them (the
default `:g` command). Both go through `push_undo`, so they coalesce under `:g`.

## Out of scope / known gaps (state them, don't hide them)

- vim-magic patterns (`\(`, `\+`, `\zs`, …) in `:s` — intentionally unsupported;
  `:s` is canonical regex like `/`.
- `:s` with the `e` (no-error) flag — later. (Note: `e` would *suppress* the
  no-match error; until it exists, no-match always reports `E486`, consistent
  with fail-loud.)
- `:g` driving `:normal`, `:m`/`:t` (move/copy), or `:>`/`:<` — those ex commands
  don't exist yet, so `:g/re/normal …` defers and reports unknown-command per
  line. `:d` takes no register/count argument yet (`:d x`, `:d 3`).
- Normal-mode `&` / `g&` (repeat the last `:s`) — the ex `:&` / `:&&` exist; the
  normal-mode keys don't.

## Touch list (as built)

- `crates/nxvim-core/src/editor/ex.rs` — `parse_ex_range`, `execute_ex` wiring,
  `ex_substitute` / `repeat_substitute` / `run_substitute`, and the Phase-3
  confirm walk (`SubstConfirm`, `begin_subst_confirm`, `subst_confirm_seek` /
  `_key` / `_apply` / `_skip` / `_finish`, `next_char_boundary`).
- `crates/nxvim-core/src/editor/mod.rs` — the `last_substitute` / `subst_confirm`
  state fields and the `subst_confirm_key` input intercept in `Editor::input`.
- `crates/nxvim-core/src/editor/cursor.rs` + `editor/command.rs` — the normal-mode
  one-line scroll `<C-e>`/`<C-y>` (`scroll_line` / `scroll_view_line`,
  `NormalCmd::ScrollLine`), which the confirm `^E`/`^Y` peek reuses.
- `crates/nxvim-core/src/editor/ex.rs` (Phase 4) — `ex_global` / `split_global`,
  the `:g`/`:v`/`:d`/`:p` `execute_ex` arms, and `ex_delete` / `ex_print`; the
  `in_global` re-entrancy flag lives on `Editor` in `editor/mod.rs`.
- `crates/nxvim-core/src/search.rs` — `SearchRegex::substitute_line` and
  `match_replacement` (the Phase-3 single-match primitive) + the replacement-dialect
  module note.
- `crates/nxvim-server/tests/editing.rs` — the `substitute_*` /
  `substitute_confirm_*` (Phases 0–3) and `global_*` / `ex_delete_*` (Phase 4) tests.
- `docs/architecture.md` / `docs/known-approximations.md` — `:s` is implemented;
  the replacement dialect shares `/` search's canonical regex.

## Resolved decisions

- **Replacement syntax** — native regex-crate `$1`/`${name}`/`$0`/`$$` for
  captures, plus a `\r`/`\n`→newline, `\t`→tab, `\\`→backslash escape pass.
  `\r` (line split) was specifically requested as useful.
- **`lo > hi` range** — error loudly (no silent swap), per the fail-loud rule.
- **Unknown flags** — error, never ignore. The `c` confirm flag is implemented
  (Phase 3); `n` (count-only) takes precedence over it.

## To verify before Phase 0

- How marks are stored today (for `'m` ranges in Phase 0) — check the existing
  mark API before wiring range marks; if marks aren't implemented yet, `'m`
  ranges error loudly rather than resolving to a bogus line.
