# `'showcmd'` and `'report'` — the two missing keystroke/edit indicators

*2026-08-15*

## The gap

bemtvi gives the user no feedback for two things vim/neovim show by default:

1. **Partially-typed commands.** Typing `2`, `d`, `"a3d`, `<Space>f` leaves the screen
   completely unchanged, so there is no way to tell the editor is mid-command
   (or that a stray count is armed). vim shows the typed run in the last 10
   columns of the last screen line (`'showcmd'`, default on), and — while a
   Visual selection is up — its *size* instead.
2. **Line-count / yank feedback.** `5dd`, `p` of a 9-line register and `10yy`
   are all silent. vim reports them on the message line when the affected line
   count exceeds `'report'` (default 2): `5 fewer lines`, `9 more lines`,
   `10 lines yanked`.

Both are plain options bemtvi never modelled. Nothing about the architecture
was in the way — the pending-command state and the yank/delete/put funnels all
already exist in `bemtvi-core`.

## Non-goals

- vim's **ruler** (`'ruler'`), which shares the same screen real estate. bemtvi
  puts the cursor position in the statusline; not touching it.
- The **undo** message (`3 fewer lines; before #4  2 seconds ago`). It is a
  different projection (undo-tree state + timestamps), and bemtvi's undo
  messages are their own family already. Worth doing separately.
- `'showcmdloc'` (vim's statusline/tabline placement for the showcmd area).
  bemtvi renders it in vim's default place — the last line.

## Phase 1 — `'report'`

### The option

`'report'` — number, global, default `2`, no abbreviation (vim has none).
"Report a message when this many lines are changed by one command." Wired the
usual way: `OPTIONS` table + `set_scalar`/`get_scalar` in `bemtvi-core/src/options.rs`,
the `apply_set_num` global slot in `editor/options.rs`, and the guard-test list in
`bemtvi-server/tests/options.rs`.

### The messages

One core helper mirroring vim's `msgmore()`:

```rust
fn report_line_delta(&mut self, before: usize)   // echo when |delta| > 'report'
```

- `+n` → `"1 more line"` / `"N more lines"`
- `-n` → `"1 line less"` / `"N fewer lines"` (vim's exact asymmetric wording)

and the yank message, `"N lines yanked"` (+ ` into "x` when the yank named a
register), fired when the yanked text spans more than `'report'` lines. A
charwise yank of a single line counts as 0 lines, so it never reports — vim's
rule.

Call sites (deliberately explicit, exactly as vim wires `msgmore()`, rather than
a generic per-command hook — a blanket hook would fire for `:g`, `:s` and insert
mode, which vim suppresses):

| site | message |
| --- | --- |
| `apply_operator_to_range` `'d'` | line delta |
| `visual_operate` `'d'` (+ the multi-cursor sweep) | line delta |
| `paste` / `paste_multi` / mouse paste | line delta |
| `yank_range` (the single yank funnel: `y{motion}`, `Y`, visual `y`, Helix `y`) | lines yanked |
| `ex_delete` (`:d`), `:copy`/`:t` | line delta |
| `shift_lines` (`>`/`<`) | `"N lines >ed 1 time"` |

`c`/`S`/`cc` deliberately do **not** report: vim computes the message and then
immediately paints `-- INSERT --` over it, so the user never sees it. bemtvi has
no `-- INSERT --` on the message line, so echoing it would leave a message vim's
user never gets, sitting there for the whole insert session.

## Phase 2 — `'showcmd'`

### The option

`'showcmd'` / `'sc'` — bool, global, default `true`.

### The projection

`Editor::showcmd() -> String`, in `editor/command.rs` next to the existing
`command_pending()` (which already documents its `keys` as "mirrors vim's
showcmd"):

- Visual / Select: the selection *size* — `N` lines for linewise or a multi-line
  charwise selection, `N` characters for a single-line charwise one, `LxC` for
  blockwise (vim's `clear_showcmd`).
- Otherwise: the pending command run — register, pre-operator count, operator,
  post-operator count, then the armed stage's trigger key. This is exactly the
  `keys` string `pending_hint()` already builds, so both share one
  `pending_keys()` helper. The one case `pending_hint()` returns `None` for but
  showcmd must show is a **bare count** (`2` with nothing else armed) — the
  literal thing the user reported missing.
- Empty when `'showcmd'` is off.

### Wire + clients

`View::showcmd` (core) → `redraw`'s `showcmd` key (server, which **appends the
keymap matcher's withheld prefix** — a half-typed `<Space>f` lives in the server's
trie, not in core's `PendingCommand`) → `bemtvi-view`'s client-side `View` →
painted right-aligned on the command-line row by the TUI, the GUI and the web
client. Truncated to vim's 10 columns.

Right-aligned so it coexists with a message on the same row (vim's layout: the
showcmd area is its own column band, independent of the message area).

## Testing

Black-box through the harness, as always.

- `crates/bemtvi-server/tests/editing/report.rs` — the `'report'` messages: each
  operator at/over/under the threshold, `:set report=0`, the register suffix, the
  no-report-on-`c` rule.
- `crates/bemtvi-server/tests/editing/showcmd.rs` — the redraw `showcmd` field
  through a count, an operator, a register, a find-char, a mapped prefix, each
  Visual shape, and `:set noshowcmd`.

## Outcome

Both phases shipped as described. Three notes where reality differed from the
sketch:

- **No blockwise Visual.** bemtvi has no `Mode::VisualBlock`, so vim's
  `{lines}x{cols}` size form has nothing to describe and is not implemented.
- **`:global`'s report is gated on silence.** The whole `:g` run reports its net
  line change once, but only when its commands left no message of their own —
  vim's `do_sub_msg()`-first precedence. bemtvi does not yet *accumulate* the
  substitutions of a `:g/…/s/…`, so today nothing reaches that gate; it is there
  so an accumulated substitute report keeps priority when one lands.
- **A charwise selection end needed a grapheme snap.** `visual_range_lw` ends a
  charwise range at `cursor + 1` byte, which can land mid-cluster; every operator
  path snapped it via `snap_range`, so nothing had noticed. The showcmd size
  slices the range directly, so it snaps too.

Verified in all three clients: the TUI (a real PTY paints `2d` in the corner and
`3 lines yanked` on the message line), the web edit-host (headless Chromium: the
corner element is right-aligned and the report messages land), and the GUI by
construction — the same projection, painted with the shared `push_plain` path.
