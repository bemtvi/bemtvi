# User-experience fidelity tests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add three tiers of tests that exercise what a user actually experiences — the painted terminal grid, real key translation, the real binary through a PTY, and the editor/UI non-blocking guarantee — on top of the existing RPC/`View`-level suite.

**Architecture:** Tier 1 promotes a tiny public surface on `nxvim-tui` (`encode_key`, `View::from_redraw`, `paint`) and tests key translation + cell-grid painting from synthetic views. Tier 2 (`crates/nxvim/tests/screen.rs`) runs the real server in-process, captures the real `redraw`, paints it via `nxvim_tui::paint`, and asserts on cells — plus a deterministic "stalled UI never blocks the editor" test. Tier 3 (`crates/nxvim/tests/e2e.rs`) drives the real `nxvim` binary in a PTY with `portable-pty` + `vt100`. A real `:sleep {N}[m]` ex-command serves as the slow-op hook for the responsiveness tests.

**Tech Stack:** Rust, tokio, ratatui (`TestBackend`), crossterm, rmpv, `portable-pty`, `vt100`.

---

## File structure

- `crates/nxvim-tui/src/lib.rs` — *modify*: make `encode_key` public; make `View` public; add `View::from_redraw` and `paint`.
- `crates/nxvim-tui/tests/keys.rs` — *create*: Tier 1 key-translation tests.
- `crates/nxvim-tui/tests/paint.rs` — *create*: Tier 1 cell-grid paint tests.
- `crates/nxvim-core/src/editor.rs` — *modify*: `pending_sleep` field + `take_sleep()` + `:sleep` parsing.
- `crates/nxvim-server/src/lib.rs` — *modify*: make `handle` async and await any pending sleep.
- `crates/nxvim-server/tests/editing.rs` — *modify*: add the `:sleep` timing test.
- `crates/nxvim/Cargo.toml` — *modify*: add dev-dependencies.
- `Cargo.toml` — *modify*: pin `portable-pty` and `vt100` in `[workspace.dependencies]`.
- `crates/nxvim/tests/screen.rs` — *create*: Tier 2 full-stack screen tests + responsiveness A.
- `crates/nxvim/tests/e2e.rs` — *create*: Tier 3 PTY smoke + responsiveness B.

---

## Task 1: Tier 1 — key-translation tests + public `encode_key`

**Files:**
- Create: `crates/nxvim-tui/tests/keys.rs`
- Modify: `crates/nxvim-tui/src/lib.rs:328` (the `fn encode_key` declaration)

- [ ] **Step 1: Write the failing test**

Create `crates/nxvim-tui/tests/keys.rs`:

```rust
//! Tier 1: the crossterm `KeyEvent` -> vim key-notation translation, tested as
//! the public function the client uses. Black-box, no process, no timing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nxvim_tui::encode_key;

fn note(code: KeyCode, mods: KeyModifiers) -> Option<String> {
    encode_key(KeyEvent::new(code, mods))
}

#[test]
fn plain_char_is_itself() {
    assert_eq!(note(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(), Some("a"));
}

#[test]
fn special_keys_use_angle_notation() {
    assert_eq!(note(KeyCode::Esc, KeyModifiers::NONE).as_deref(), Some("<Esc>"));
    assert_eq!(note(KeyCode::Enter, KeyModifiers::NONE).as_deref(), Some("<CR>"));
    assert_eq!(note(KeyCode::Backspace, KeyModifiers::NONE).as_deref(), Some("<BS>"));
    assert_eq!(note(KeyCode::Tab, KeyModifiers::NONE).as_deref(), Some("<Tab>"));
}

#[test]
fn ctrl_and_alt_get_prefixed() {
    assert_eq!(note(KeyCode::Char('w'), KeyModifiers::CONTROL).as_deref(), Some("<C-w>"));
    assert_eq!(note(KeyCode::Char('x'), KeyModifiers::ALT).as_deref(), Some("<A-x>"));
}

#[test]
fn literal_less_than_is_escaped() {
    assert_eq!(note(KeyCode::Char('<'), KeyModifiers::NONE).as_deref(), Some("<lt>"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p nxvim-tui --test keys`
Expected: compile error — `encode_key` is private / unresolved import `nxvim_tui::encode_key`.

- [ ] **Step 3: Make `encode_key` public**

In `crates/nxvim-tui/src/lib.rs`, change the declaration at line 328 from:

```rust
/// Translate a crossterm key event into vim key-notation.
fn encode_key(ev: KeyEvent) -> Option<String> {
```

to:

```rust
/// Translate a crossterm key event into vim key-notation.
///
/// Public so the key-translation contract can be tested directly (Tier 1).
pub fn encode_key(ev: KeyEvent) -> Option<String> {
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nxvim-tui --test keys`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/nxvim-tui/tests/keys.rs crates/nxvim-tui/src/lib.rs
git commit -m "test(tui): cover crossterm->key-notation translation"
```

---

## Task 2: Tier 1 — cell-grid paint tests + public `View`/`from_redraw`/`paint`

**Files:**
- Create: `crates/nxvim-tui/tests/paint.rs`
- Modify: `crates/nxvim-tui/src/lib.rs` (make `View` public; add `from_redraw` and `paint`)

- [ ] **Step 1: Write the failing test**

Create `crates/nxvim-tui/tests/paint.rs`:

```rust
//! Tier 1: render a known `View` into a cell grid via ratatui's test backend
//! and assert on exactly what a user would see. Synthetic views are the right
//! input here — this pins the *client's painting contract*, not server logic.

use nxvim_tui::{paint, View};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use rmpv::Value;

/// Build a `redraw` params vec (a one-element array holding the view map),
/// matching what the server sends and `View::from_redraw` consumes.
fn redraw(fields: Vec<(&str, Value)>) -> Vec<Value> {
    let mut map: Vec<(Value, Value)> = vec![
        (Value::from("lines"), Value::Array(vec![])),
        (Value::from("cursor_row"), Value::from(0u64)),
        (Value::from("cursor_col"), Value::from(0u64)),
        (Value::from("cursor_screen_col"), Value::from(0u64)),
        (Value::from("mode_label"), Value::from("NORMAL")),
        (Value::from("command_mode"), Value::from(false)),
        (Value::from("cmdline"), Value::from("")),
        (Value::from("message"), Value::from("")),
        (Value::from("file_name"), Value::from("")),
        (Value::from("modified"), Value::from(false)),
        (Value::from("cursor_line"), Value::from(1u64)),
    ];
    for (k, v) in fields {
        // Replace the default if present, else append.
        if let Some(slot) = map.iter_mut().find(|(mk, _)| mk.as_str() == Some(k)) {
            slot.1 = v;
        } else {
            map.push((Value::from(k), v));
        }
    }
    vec![Value::Map(map)]
}

fn view(fields: Vec<(&str, Value)>) -> View {
    View::from_redraw(&redraw(fields))
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
        .collect()
}

fn reversed(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .map(|c| c.style().add_modifier.contains(Modifier::REVERSED))
        .unwrap_or(false)
}

fn lines(strs: &[&str]) -> Value {
    Value::Array(strs.iter().map(|s| Value::from(*s)).collect())
}

#[test]
fn text_is_painted_on_the_top_rows() {
    let v = view(vec![("lines", lines(&["hello"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
}

#[test]
fn bottom_two_rows_are_status_and_command_chrome() {
    // 5 rows: text on 0..3, status on row 3, command on row 4.
    let v = view(vec![("lines", lines(&["abc"])), ("file_name", Value::from("f.txt"))]);
    let buf = paint(&v, 20, 5);
    assert!(row_text(&buf, 3).contains("NORMAL"), "status: {:?}", row_text(&buf, 3));
    assert!(row_text(&buf, 3).contains("f.txt"), "status: {:?}", row_text(&buf, 3));
    assert_eq!(row_text(&buf, 4).trim_end(), ""); // empty command line (no message)
}

#[test]
fn status_row_is_reversed() {
    let v = view(vec![("lines", lines(&["abc"]))]);
    let buf = paint(&v, 20, 5);
    assert!(reversed(&buf, 0, 3), "status row should be reverse-video");
}

#[test]
fn a_selection_span_highlights_exactly_its_cells() {
    // Select screen columns [0, 3) on row 0.
    let sel = Value::Array(vec![Value::Array(vec![Value::from(0u64), Value::from(3u64)])]);
    let v = view(vec![("lines", lines(&["hello"])), ("selection", sel)]);
    let buf = paint(&v, 20, 5);
    assert!(reversed(&buf, 0, 0));
    assert!(reversed(&buf, 1, 0));
    assert!(reversed(&buf, 2, 0));
    assert!(!reversed(&buf, 3, 0), "cell past the span must not be highlighted");
}

#[test]
fn wide_chars_occupy_two_cells_each() {
    let v = view(vec![("lines", lines(&["日本"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "本");
}

#[test]
fn command_mode_renders_the_colon_line() {
    let v = view(vec![
        ("command_mode", Value::from(true)),
        ("cmdline", Value::from("w")),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 4).trim_end(), ":w");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p nxvim-tui --test paint`
Expected: compile error — `View`, `View::from_redraw`, and `paint` are not public / do not exist.

(If it instead fails on `cell(...)`/`style()` not existing, the installed ratatui differs; confirm the current cell/style accessors with the find-docs skill for ratatui 0.30 and adjust `row_text`/`reversed` accordingly. This is the only API-shape risk in the task.)

- [ ] **Step 3: Make `View` public and add `from_redraw` + `paint`**

In `crates/nxvim-tui/src/lib.rs`, change the struct declaration (line 112) from:

```rust
/// The server's view, mirrored client-side for rendering.
#[derive(Default)]
struct View {
```

to:

```rust
/// The server's view, mirrored client-side for rendering.
#[derive(Default)]
pub struct View {
```

Then, immediately after the existing `impl View { ... }` block (which ends at line 172, just before `/// Lay out the three regions ...`), add a second impl block and the `paint` function:

```rust
impl View {
    /// Build a view from a `redraw` notification's params — the client's own
    /// parsing path — so tests and tools can paint a known view.
    pub fn from_redraw(params: &[Value]) -> Self {
        let mut view = View::default();
        view.update(params);
        view
    }
}

/// Render `view` into a `width`x`height` cell grid using ratatui's test backend
/// and return the painted buffer. This drives the *same* `render` the live
/// client uses, so tests assert on exactly what a user would see.
pub fn paint(view: &View, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal.draw(|frame| render(frame, view)).expect("draw");
    terminal.backend().buffer().clone()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nxvim-tui --test paint`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/nxvim-tui/tests/paint.rs crates/nxvim-tui/src/lib.rs
git commit -m "test(tui): assert on the painted cell grid via TestBackend"
```

---

## Task 3: `:sleep` ex-command + async server await (the slow-op hook)

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs` (struct field, constructor, `take_sleep`, `:sleep` parse, `parse_sleep` helper)
- Modify: `crates/nxvim-server/src/lib.rs` (async `handle`, await pending sleep)
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/nxvim-server/tests/editing.rs`:

```rust
#[tokio::test]
async fn sleep_blocks_the_editor_for_the_requested_duration() {
    let (rpc, _incoming) = start(None).await;
    // The command is acknowledged promptly; the server then sleeps. The next
    // request can only be handled once the sleep finishes, so its round-trip
    // time is a reliable *lower bound* on the sleep (lower bounds never flake).
    rpc.request("nvim_command", vec![Value::from("sleep 150m")])
        .await
        .expect("sleep command");
    let begin = std::time::Instant::now();
    let _ = lines(&rpc).await;
    assert!(
        begin.elapsed() >= std::time::Duration::from_millis(120),
        "follow-up returned too soon: {:?}",
        begin.elapsed()
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p nxvim-server --test editing sleep_blocks`
Expected: FAIL — `:sleep` is unrecognized (no delay), `begin.elapsed()` is ~0ms, assertion fails.

- [ ] **Step 3a: Add the `pending_sleep` field and `take_sleep` to the editor**

In `crates/nxvim-core/src/editor.rs`, in the `Editor` struct, change (line 123-124):

```rust
    /// Lua chunks queued by `:lua`, drained by the server's Lua runtime.
    pub lua_queue: Vec<String>,
}
```

to:

```rust
    /// Lua chunks queued by `:lua`, drained by the server's Lua runtime.
    pub lua_queue: Vec<String>,

    /// Milliseconds the server should block after the current command, set by
    /// `:sleep` and drained via [`Editor::take_sleep`]. Models a slow editor
    /// operation; the server awaits it without freezing the UI.
    pending_sleep: Option<u64>,
}
```

In `with_buffer` (the constructor), change (line 163):

```rust
            lua_queue: Vec::new(),
        }
```

to:

```rust
            lua_queue: Vec::new(),
            pending_sleep: None,
        }
```

In the `// ----- public API used by the server -----` region (just after the `resize` doc comment area near line 167-172 is fine; place it right after the `with_buffer` fn closes, before `resize`), add:

```rust
    /// Take any pending `:sleep` duration in milliseconds, clearing it. The
    /// server awaits this between message handling, so a slow editor operation
    /// never blocks the client (a separate thread/process).
    pub fn take_sleep(&mut self) -> Option<u64> {
        self.pending_sleep.take()
    }
```

- [ ] **Step 3b: Parse `:sleep` in `execute_ex`**

In `crates/nxvim-core/src/editor.rs`, in the `execute_ex` match (line 1187-1190), change:

```rust
            "lua" => self.lua_queue.push(args.to_string()),
            "set" | "se" => self.message = format!("(set {args} — not yet implemented)"),
            "noh" | "nohlsearch" => {}
```

to:

```rust
            "lua" => self.lua_queue.push(args.to_string()),
            "sleep" | "sl" => self.pending_sleep = Some(parse_sleep(args)),
            "set" | "se" => self.message = format!("(set {args} — not yet implemented)"),
            "noh" | "nohlsearch" => {}
```

Then add this free function next to the existing `split_ex` helper (search the file for `fn split_ex` and place `parse_sleep` directly after it):

```rust
/// Parse a `:sleep` argument: `{n}` = seconds, `{n}m` = milliseconds, empty =
/// 1 second (matching vim). Unparseable input sleeps zero.
fn parse_sleep(args: &str) -> u64 {
    let a = args.trim();
    if a.is_empty() {
        return 1000;
    }
    match a.strip_suffix('m') {
        Some(ms) => ms.trim().parse::<u64>().unwrap_or(0),
        None => a.parse::<u64>().map(|secs| secs * 1000).unwrap_or(0),
    }
}
```

- [ ] **Step 3c: Make the server await the pending sleep**

In `crates/nxvim-server/src/lib.rs`, change the run loop (line 54-60) from:

```rust
    while let Some(message) = incoming.recv().await {
        server.handle(message);
        if server.editor.should_quit {
            server.rpc.notify("nxvim_exit", vec![]);
            break;
        }
    }
```

to:

```rust
    while let Some(message) = incoming.recv().await {
        server.handle(message).await;
        if server.editor.should_quit {
            server.rpc.notify("nxvim_exit", vec![]);
            break;
        }
    }
```

Then change `handle` (line 65-79) from:

```rust
    fn handle(&mut self, message: Incoming) {
        match message {
            Incoming::Request { id, method, params } => {
                match self.dispatch(&method, &params) {
                    Ok(value) => self.rpc.respond(id, Ok(value)),
                    Err(err) => self.rpc.respond(id, Err(Value::from(err))),
                }
                self.redraw();
            }
            Incoming::Notification { method, params } => {
                let _ = self.dispatch(&method, &params);
                self.redraw();
            }
        }
    }
```

to:

```rust
    async fn handle(&mut self, message: Incoming) {
        match message {
            Incoming::Request { id, method, params } => {
                match self.dispatch(&method, &params) {
                    Ok(value) => self.rpc.respond(id, Ok(value)),
                    Err(err) => self.rpc.respond(id, Err(Value::from(err))),
                }
                self.redraw();
            }
            Incoming::Notification { method, params } => {
                let _ = self.dispatch(&method, &params);
                self.redraw();
            }
        }
        // A `:sleep` parks the editor for the requested span. Awaiting (not
        // blocking) keeps the RPC reader/writer tasks alive, so input typed
        // during the sleep is buffered and applied once we wake.
        if let Some(ms) = self.editor.take_sleep() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nxvim-server --test editing sleep_blocks`
Expected: PASS.

Also run the full server suite to confirm the async-`handle` change broke nothing:
Run: `cargo test -p nxvim-server`
Expected: PASS (all existing tests + the new one).

- [ ] **Step 5: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/src/lib.rs crates/nxvim-server/tests/editing.rs
git commit -m "feat: :sleep ex-command, awaited by the server"
```

---

## Task 4: Tier 2 — full-stack screen tests + responsiveness A

**Files:**
- Modify: `crates/nxvim/Cargo.toml` (add `[dev-dependencies]`)
- Create: `crates/nxvim/tests/screen.rs`

- [ ] **Step 1: Add dev-dependencies to the `nxvim` crate**

In `crates/nxvim/Cargo.toml`, after the `[dependencies]` block (line 15-19), add:

```toml
[dev-dependencies]
nxvim-rpc.workspace = true
rmpv.workspace = true
ratatui.workspace = true
```

(`nxvim-server`, `nxvim-tui`, and `tokio` are already normal dependencies and are usable from integration tests; only `nxvim-rpc`, `rmpv`, and `ratatui` need adding.)

- [ ] **Step 2: Write the failing test**

Create `crates/nxvim/tests/screen.rs`:

```rust
//! Tier 2: the full in-process stack — real server -> real `View` -> real
//! client paint — asserted on the painted cell grid. Deterministic: the
//! `barrier`/`lines` request guarantees all prior input was processed and its
//! redraw emitted before we read the screen. No sleeps.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_tui::{paint, View};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24; // text area is ROWS - 2 chrome rows = 22

/// Start a server and attach with a text-area height matching the paint grid
/// (ROWS - 2 chrome rows), so the captured `View` fills the grid exactly.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, ServerInit { file }));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(COLS as u64),
            Value::from((ROWS - 2) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Barrier: awaiting this guarantees the server processed all prior input.
async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

/// The most recent `redraw` params currently buffered on the connection.
fn latest_redraw(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<Value>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = Some(params);
        }
    }
    latest
}

/// Drive input, then capture and paint the resulting real view.
async fn screen(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Buffer {
    barrier(rpc).await;
    let params = latest_redraw(incoming).expect("a redraw");
    paint(&View::from_redraw(&params), COLS, ROWS)
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
        .collect()
}

fn reversed(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .map(|c| c.style().add_modifier.contains(Modifier::REVERSED))
        .unwrap_or(false)
}

#[tokio::test]
async fn typed_text_is_painted_with_the_mode_in_the_status_line() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
    // Status line is the row just above the command line (ROWS - 2 == row 22).
    assert!(row_text(&buf, ROWS - 2).contains("NORMAL"), "status: {:?}", row_text(&buf, ROWS - 2));
}

#[tokio::test]
async fn a_visual_selection_is_highlighted_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0vll"); // select h,e,l -> screen cols [0,3)
    let buf = screen(&rpc, &mut incoming).await;
    assert!(reversed(&buf, 0, 0));
    assert!(reversed(&buf, 1, 0));
    assert!(reversed(&buf, 2, 0));
    assert!(!reversed(&buf, 3, 0));
}

#[tokio::test]
async fn wide_chars_align_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "本");
}

#[tokio::test]
async fn editor_keeps_processing_when_the_ui_never_drains_redraws() {
    // Never read `incoming` — a stalled/slow UI consumer. The server's outbound
    // queue is unbounded and runs as its own task, so the editor must keep
    // processing input regardless. Deterministic.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i");
    for _ in 0..200 {
        feed(&rpc, "x");
    }
    feed(&rpc, "<Esc>");
    // Read only the response to this request; never drain the redraws.
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    let line = match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    assert_eq!(line, vec!["x".repeat(200)]);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p nxvim --test screen`
Expected: first run fails to compile until the dev-deps from Step 1 are in place; once compiling, all four tests should pass (this task is mostly test-only — the production code it exercises already exists after Tasks 1–3). If any assertion fails, treat it as a real defect surfaced by the new tier and debug with superpowers:systematic-debugging.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nxvim --test screen`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/nxvim/Cargo.toml crates/nxvim/tests/screen.rs
git commit -m "test(nxvim): full-stack screen paint + stalled-UI responsiveness"
```

---

## Task 5: Tier 3 — PTY smoke harness + responsiveness B

**Files:**
- Modify: `Cargo.toml` (pin `portable-pty`, `vt100` in `[workspace.dependencies]`)
- Modify: `crates/nxvim/Cargo.toml` (add the two as dev-deps)
- Create: `crates/nxvim/tests/e2e.rs`

- [ ] **Step 1: Add and pin the PTY dev-dependencies**

Add the crates to the `nxvim` package (this resolves the latest stable versions):

Run: `cargo add -p nxvim --dev portable-pty vt100`

Then enforce the project's exact-pin convention. Note the two versions `cargo add` wrote into `crates/nxvim/Cargo.toml` (e.g. `portable-pty = "0.X.Y"`, `vt100 = "0.A.B"`). Move them to the root `Cargo.toml` `[workspace.dependencies]` block (after `unicode-segmentation` on line 39), pinned with `=`:

```toml
portable-pty = "=0.X.Y"
vt100 = "=0.A.B"
```

(Replace `0.X.Y` / `0.A.B` with the exact versions `cargo add` resolved.)

Then in `crates/nxvim/Cargo.toml`, change the two lines `cargo add` inserted under `[dev-dependencies]` to use the workspace versions:

```toml
portable-pty.workspace = true
vt100.workspace = true
```

The `[dev-dependencies]` block should now read:

```toml
[dev-dependencies]
nxvim-rpc.workspace = true
rmpv.workspace = true
ratatui.workspace = true
portable-pty.workspace = true
vt100.workspace = true
```

Verify it resolves: Run `cargo build -p nxvim --tests` — Expected: builds.

- [ ] **Step 2: Write the failing test**

Create `crates/nxvim/tests/e2e.rs`:

```rust
//! Tier 3: drive the real `nxvim` binary in a pseudo-terminal and assert on the
//! terminal output a user would actually see. This is the only tier that proves
//! real crossterm decode, real terminal escapes, and process startup/args. Kept
//! thin: it is the slow/flaky surface, so the bulk of coverage lives in Tiers 1–2.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A spawned `nxvim` process attached to a PTY, with a background thread feeding
/// all output into a `vt100` parser.
struct Session {
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Session {
    fn spawn(args: &[&str], cols: u16, rows: u16) -> Session {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_nxvim"));
        for a in args {
            cmd.arg(a);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn nxvim");
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let sink = parser.clone();
        // Continuously drain the PTY so the deadline logic in `wait_until` never
        // blocks on a read. The thread ends when the child closes the PTY.
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().process(&buf[..n]),
                }
            }
        });

        Session { writer, parser, _child: child, _master: pair.master }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write");
        self.writer.flush().expect("flush");
    }

    /// Poll the parsed screen until `pred` holds or `timeout` elapses.
    fn wait_until(&self, timeout: Duration, pred: impl Fn(&vt100::Screen) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let guard = self.parser.lock().unwrap();
                if pred(guard.screen()) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                let guard = self.parser.lock().unwrap();
                return pred(guard.screen());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn screen_text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }
}

#[test]
fn startup_shows_the_file_contents() {
    let path = std::env::temp_dir().join(format!("nxvim_e2e_startup_{}.txt", std::process::id()));
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let mut s = Session::spawn(&[path.to_str().unwrap()], 80, 24);
    let ok = s.wait_until(Duration::from_secs(5), |scr| {
        let t = scr.contents();
        t.contains("alpha") && t.contains("beta")
    });
    assert!(ok, "screen never showed the file:\n{}", s.screen_text());

    s.send(b":q!\r");
    std::fs::remove_file(&path).ok();
}

#[test]
fn typing_appears_on_screen_and_mode_flips() {
    let mut s = Session::spawn(&[], 80, 24);
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr.contents().contains("NORMAL")),
        "no NORMAL status at startup:\n{}",
        s.screen_text()
    );

    s.send(b"ihi");
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| {
            let t = scr.contents();
            t.contains("INSERT") && t.contains("hi")
        }),
        "after typing 'ihi':\n{}",
        s.screen_text()
    );

    s.send(b"\x1b"); // Esc
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr.contents().contains("NORMAL")),
        "did not return to NORMAL:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
}

#[test]
fn client_stays_responsive_while_the_editor_sleeps() {
    let mut s = Session::spawn(&[], 80, 24);
    assert!(s.wait_until(Duration::from_secs(5), |scr| scr.contents().contains("NORMAL")));

    // Put the editor to sleep, then immediately type. The client never freezes
    // and the wire buffers the input, which is applied once the editor wakes.
    s.send(b":sleep 800m\r");
    s.send(b"ihi\x1b");
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr.contents().contains("hi")),
        "input typed during :sleep never applied:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
}
```

- [ ] **Step 3: Run test to verify it fails (then passes)**

Run: `cargo test -p nxvim --test e2e`
Expected: with the production code from Tasks 1–3 in place, the tests should PASS. If the harness mis-compiles against the resolved `portable-pty`/`vt100` API (method names like `openpty`, `try_clone_reader`, `take_writer`, `spawn_command`, `vt100::Parser::new`, `Screen::contents`), confirm the current signatures with the find-docs skill for those crates and adjust the harness. This is the task's only API-shape risk.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p nxvim --test e2e`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/nxvim/Cargo.toml crates/nxvim/tests/e2e.rs
git commit -m "test(nxvim): PTY smoke of the real binary + sleep responsiveness"
```

---

## Task 6: Whole-workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS — all existing tests plus the new `keys`, `paint`, `screen`, `e2e`, and `sleep_blocks...` tests.

- [ ] **Step 2: Format check**

Run: `cargo fmt --all -- --check`
Expected: no diff. (If it reports changes, run `cargo fmt --all` and re-stage.)

- [ ] **Step 3: Clippy (the pre-commit gate)**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. Fix any that appear (the new test files and the async-`handle` change are the likely sources).

- [ ] **Step 4: Update the architecture doc's testing section**

In `docs/architecture.md`, the *Testing philosophy* section lists e2e PTY tests as "(planned)" and the roadmap lists "PTY-driven e2e tests of the binary". Update both: describe the now-implemented three tiers (Tier 1 client paint/key tests in `nxvim-tui/tests/`, Tier 2 full-stack screen tests in `nxvim/tests/screen.rs`, Tier 3 PTY smoke in `nxvim/tests/e2e.rs`) and remove the PTY item from the "Not yet implemented" roadmap list. Keep it to a short, accurate paragraph.

- [ ] **Step 5: Commit**

```bash
git add docs/architecture.md
git commit -m "docs: describe the implemented three-tier UX test suite"
```

---

## Notes for the implementer

- **Determinism discipline:** Tiers 1–2 must have *no* sleeps and *no* wall-clock assertions. The only timing lives in Tier 3's `wait_until` (predicate-polling with a generous deadline) and the single `:sleep` lower-bound assertion in Task 3 (lower bounds on a sleep never flake; never assert an upper bound).
- **Why `:sleep` awaits after `respond`:** the responsiveness tests only require that the editor be busy *after* acknowledging the command and that the client/wire stay alive meanwhile — both hold with the await at the end of `handle`. Do not try to make `dispatch` async; that change would ripple through the whole RPC surface for no benefit.
- **ratatui / PTY API drift** is the only real risk and is flagged inline in Tasks 2 and 5. If a method name differs from what's shown, verify with the find-docs skill rather than guessing.
- **Non-goals carried from the spec:** no golden-file snapshots, no Windows PTY, no interruptible `:sleep` (a real-vim feature deliberately deferred).
