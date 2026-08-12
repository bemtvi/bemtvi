# User-experience fidelity tests

**Status:** approved design, pending implementation
**Date:** 2026-05-31
**Scope:** `bemtvi-tui`, `bemtvi` (bin) integration tests, `bemtvi-core`/`bemtvi-server` (`:sleep`), workspace dev-deps

## Problem

Today's tests (`crates/bemtvi-server/tests/editing.rs`) are good black-box
integration tests, but they stop at the **RPC + semantic `View`** boundary. They
send `nvim_input` and assert on `nvim_buf_get_lines`, the cursor, and the
`redraw` `View` map's fields (`lines`, `selection`, `mode_label`, …). They never
exercise:

1. **What's on screen.** The TUI client (`bemtvi-tui`) is entirely untested — the
   ratatui layout, the reserved chrome rows, status/command line content and
   styling, selection highlighting, and wide-char/tab alignment as actually
   *painted* into a cell grid. We assert the server *describes* the right screen,
   never that the client *paints* it.
2. **Real keypresses.** The crossterm `KeyEvent` → vim key-notation translation
   (`encode_key`) never runs in a test; tests inject pre-translated notation.
3. **The full binary via PTY.** Process startup, the positional file argument,
   the embedded server+client wiring over the duplex pipe, real crossterm
   decoding of terminal bytes, and the real terminal escape output a user's
   terminal receives are never driven end to end.
4. **Responsiveness.** The architecture's promise — the UI never blocks on the
   editor and the editor never blocks on the UI (separate threads, async I/O) —
   is asserted nowhere.

The architecture doc already names the gap: *"e2e tests (planned) will drive the
actual `bemtvi` binary through a PTY and assert on the terminal output a user
would really see."* This spec closes it.

## Goals

- Assert on the **actual painted cell grid** the user sees, not just the
  semantic `View`.
- Exercise the **real crossterm → key-notation** translation.
- Drive the **real `bemtvi` binary through a PTY** and assert on the terminal
  output it produces.
- Assert the **non-blocking** guarantee between the editor and the UI.
- Keep the fast, deterministic tiers broad and the slow/flaky PTY tier thin, so
  failures localize and CI stays fast and stable.
- Preserve the project's testing philosophy: black-box, behavior-only, tests
  live in `tests/` (no `#[test]` units inside crate `src/`).

## Non-goals

- Replacing or migrating the existing `editing.rs` suite — the new tiers sit
  *on top* of it.
- A neovim UI wire protocol / `ext_linegrid` — out of scope by design.
- Golden-file snapshot testing of full screens — explicit cell assertions are
  preferred over snapshot churn for now.
- Cross-platform PTY coverage beyond the developer/CI OS; the PTY tier targets
  Unix first (Windows PTY is a later concern, tracked separately).

## Architecture: three tiers, cheap → faithful

### Tier 1 — `crates/bemtvi-tui/tests/` — client paint + key translation

Pins the **client's rendering contract** with no process and no timing.
Fast and fully deterministic. Requires a small, deliberate public surface on
`bemtvi-tui` (currently all private):

- `pub fn encode_key(KeyEvent) -> Option<String>` — promote the existing fn.
- `pub struct View` + `pub fn from_redraw(params: &[Value]) -> View` — expose
  the existing `View::update` parsing as a constructor.
- `pub fn paint(view: &View, width: u16, height: u16) -> ratatui::buffer::Buffer`
  — internally drives `Terminal::new(TestBackend::new(width, height))`, calls
  the existing `render`, and returns a clone of the backend buffer. The ratatui
  plumbing stays inside the crate; tests receive a grid of cells.

Representative tests:

- **Key translation:** `Esc → "<Esc>"`, `Enter → "<CR>"`, `Ctrl+w → "<C-w>"`,
  `Alt+x → "<A-x>"`, char `'<' → "<lt>"`, and `KeyEventKind::Release` yields no
  input (filtered before `encode_key`, so this asserts the loop's contract via a
  thin helper or documents it as event-loop behavior covered in Tier 3).
- **Paint, from synthetic `View`s** (the contract is "given this View, paint
  this grid", so synthetic inputs are exactly right here):
  - text appears on the correct rows; the bottom two rows are exactly the
    status + command chrome.
  - the status row is `REVERSED` and reads e.g. `NORMAL  file.txt [+]` on the
    left and `line,col` on the right.
  - command mode renders `:` + cmdline and parks the cursor at the right cell.
  - a `selection` span paints `REVERSED` over exactly the right cells,
    including the trailing newline / linewise fill.
  - a wide char (e.g. `日`) occupies two cells and the cursor screen-column
    lands correctly; a leading tab expands to the next tabstop.

### Tier 2 — `crates/bemtvi/tests/screen.rs` — full in-process stack → real paint

The **workhorse** for "what the user sees", deterministic and PTY-free. The
`bemtvi` bin crate is the natural home: it already depends on both
`bemtvi-server` and `bemtvi-tui`.

Harness (mirrors `editing.rs`): start a real server over a `tokio::io::duplex`
pipe, `nvim_ui_attach`, feed vim key-notation via `nvim_input`. Then capture the
latest real `redraw` map and feed it through `bemtvi_tui::View::from_redraw` +
`bemtvi_tui::paint`, and assert on the resulting cell grid. Determinism comes for
free from the existing `lines()`-as-barrier trick (awaiting a request guarantees
all prior input was processed and its redraw emitted) — **no sleeps**.

This proves the *real server → real View → real client paint* path agrees end to
end, e.g.: typing `ihello<Esc>` paints `hello` on row 0 with the status row
showing `NORMAL`; a visual selection lights up the right cells; `日本` aligns on
screen.

**Responsiveness test A (editor not blocked by a slow UI), deterministic:**
a raw RPC client sends a burst of `nvim_input` but deliberately does **not**
drain incoming `redraw` notifications (a stalled/slow UI). Because the server's
reader/writer tasks are independent and buffered and the server runs on its own
thread, the editor keeps processing. After the burst, drain and assert the
buffer reflects every keystroke — i.e. a UI that isn't keeping up never blocks
the editor.

### Tier 3 — `crates/bemtvi/tests/e2e.rs` — thin PTY smoke of the real binary

The only tier that proves the **real experience**: real crossterm decode, real
terminal escape output, real process startup/args. Kept to a handful of tests
because it is the slow/flaky surface.

New pinned dev-dependencies (in root `Cargo.toml` `[workspace.dependencies]`,
pulled into the `bemtvi` crate as dev-deps):

- `portable-pty` — spawn the built `bemtvi` binary in a fixed-size PTY.
- `vt100` — parse the PTY output stream into an inspectable screen grid.

Harness (all timing localized here):

- `spawn(args: &[&str], cols: u16, rows: u16) -> Session` — launches the binary
  (resolved via `CARGO_BIN_EXE_bemtvi`) attached to a PTY parser.
- `Session::send(bytes: &[u8])` — writes raw bytes, including `\x1b` (Esc) and
  `\r` (Enter), exactly as a terminal would.
- `Session::wait_until(pred: impl Fn(&vt100::Screen) -> bool, timeout) -> bool`
  — the **one** place timing lives: poll-read the PTY, feed `vt100`, re-check the
  predicate until it holds or the deadline trips. No fixed sleeps.
- `Session::screen() -> &vt100::Screen` — current parsed grid.
- Teardown sends `:q!<CR>` (and kills on timeout) so the child always exits.

Representative tests (small set):

- **Startup:** `bemtvi <file>` shows the file's contents and a status line.
- **Real keystroke round trip:** send `ihi\x1b`, `wait_until` the screen shows
  `hi` and the status flips `INSERT → NORMAL`.
- **Wide-char alignment** on the real emulator.

**Responsiveness test B (UI not frozen by a slow editor), via the slow-op hook:**
launch the binary, send `:sleep 1000m\r`, then immediately send `ihi\x1b` while
the editor is mid-sleep. `wait_until` (deadline > the sleep) asserts `hi`
eventually appears — proving the client never froze and the wire kept buffering
input typed during the slow editor operation, which is applied once the editor
is free.

## The slow-op hook: a real `:sleep {N}[m]` ex-command

Rather than a test-only RPC, implement the genuine vim/neovim ex-command
`:sleep {N}` (seconds) / `:sleep {N}m` (milliseconds) in the editor. It doubles
as the responsiveness test hook and is a real feature.

- Parsing/dispatch lives where other ex-commands live (`bemtvi-core` editor /
  `bemtvi-server` dispatch). The command yields a duration the server awaits
  (`tokio::time::sleep`), parking the dispatch loop for that span — exactly the
  "slow editor operation" the responsiveness test needs.
- Minimal scope: a non-interruptible blocking sleep. Real vim's
  keypress-interruptible `:sleep` is **future work**, noted here, not built now.

## Testing

This spec *is* testing infrastructure; its own verification is that the new
tiers compile and pass and that `cargo test --workspace` stays green and fast
(the PTY tier kept small). The existing `editing.rs` suite is unchanged.

## Risks / mitigations

- **PTY flakiness/slowness.** Mitigated by keeping Tier 3 thin, localizing all
  timing in `wait_until` (predicate-polling, not fixed sleeps), and putting the
  bulk of fidelity coverage in the deterministic Tiers 1–2.
- **`bemtvi-tui` public surface.** The promotions (`encode_key`, `View`,
  `from_redraw`, `paint`) are a deliberate, minimal test surface; documented as
  such so they aren't mistaken for a general client API.
- **`:sleep` parking the loop.** Acceptable and intended (models a slow op);
  non-interruptibility is explicitly deferred.
