//! Black-box tests for `:terminal` (Phase 3 — the native PTY transport).
//!
//! A real server over RPC spawns a real local PTY child; the test drives it with
//! `nvim_input` and asserts on the buffer the child's output is mirrored into. PTY
//! output is asynchronous (a reader thread streams it back and the server settles
//! off-tick), so assertions poll the buffer until the expected text lands rather
//! than reading once. Hermetic: only POSIX `cat` is spawned (present on macOS/Linux).

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, mode,
    serial_lock, spawn, start_attached, temp_dir, window0_field, write_temp, TestClock,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The current buffer's reported name (`nvim_buf_get_name`).
async fn buf_name(rpc: &Rpc) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .expect("get_name")
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Poll the current buffer until `pred` holds, or panic after ~10s. PTY output is
/// async, so the expected text may take a few ticks to stream in and settle.
async fn wait_lines(rpc: &Rpc, what: &str, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..200 {
        let ls = lines(rpc).await;
        if pred(&ls) {
            return ls;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "timed out waiting for {what}; last buffer lines: {:?}",
        lines(rpc).await
    );
}

fn has_line(ls: &[String], text: &str) -> bool {
    ls.iter().any(|l| l.trim_end() == text)
}

/// The buffer's line count (`nvim_buf_line_count`) — a cheap poll target that does
/// not transfer the whole (possibly huge) buffer the way `lines` does.
async fn line_count(rpc: &Rpc) -> usize {
    rpc.request("nvim_buf_line_count", vec![Value::from(0u64)])
        .await
        .expect("line_count")
        .as_u64()
        .unwrap_or(0) as usize
}

/// Lines `[start, end)` of the current buffer (`nvim_buf_get_lines`).
async fn lines_range(rpc: &Rpc, start: i64, end: i64) -> Vec<String> {
    let v = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(start),
                Value::from(end),
                Value::from(false),
            ],
        )
        .await
        .expect("get_lines");
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `:terminal cat` runs a real child: typed input is echoed back into the buffer,
/// and `<C-d>` (EOF) ends it with the `[Process exited 0]` notice, dropping us back
/// to Normal mode.
#[tokio::test]
async fn terminal_echoes_input_and_reports_exit() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    // `:terminal` enters terminal-job mode; keystrokes go to the child.
    assert_eq!(
        mode(&rpc).await,
        "t",
        "should be in terminal mode after :terminal"
    );

    // `cat` echoes each line back: typing "hello<CR>" makes "hello" appear.
    feed(&rpc, "hello<CR>");
    wait_lines(&rpc, "cat to echo 'hello'", |ls| has_line(ls, "hello")).await;

    // `<C-d>` at the start of a line sends EOF; `cat` exits 0.
    feed(&rpc, "<C-d>");
    wait_lines(&rpc, "the process-exit notice", |ls| {
        ls.iter().any(|l| l.contains("[Process exited 0]"))
    })
    .await;
    // A dead terminal drops back to Normal mode.
    assert_eq!(
        mode(&rpc).await,
        "n",
        "should leave terminal mode when the child exits"
    );
}

/// `<C-\><C-n>` leaves terminal-job mode for terminal-normal (the buffer reads as
/// ordinary text for scrolling / yanking) without killing the child.
#[tokio::test]
async fn ctrl_backslash_ctrl_n_leaves_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hi<CR>");
    wait_lines(&rpc, "cat to echo 'hi'", |ls| has_line(ls, "hi")).await;

    feed(&rpc, "<C-\\><C-n>");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "<C-\\><C-n> should return to normal mode"
    );

    // The echoed text is still there to navigate.
    assert!(
        has_line(&lines(&rpc).await, "hi"),
        "terminal output stays readable in normal mode"
    );

    // `i` re-enters terminal-job mode.
    feed(&rpc, "i");
    assert_eq!(mode(&rpc).await, "t", "`i` re-enters the terminal job");
}

/// The cursor sits at the child's next-write position (one past the last typed
/// char), not on top of it — terminal mode allows the cursor past end-of-line.
#[tokio::test]
async fn cursor_sits_after_the_last_typed_char() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello");
    // The PTY echoes typed chars immediately, so "hello" lands on row 1.
    let ls = wait_lines(&rpc, "the echoed 'hello'", |ls| has_line(ls, "hello")).await;
    let row = ls.iter().position(|l| l.trim_end() == "hello").unwrap();
    let (cline, ccol) = cursor(&rpc).await;
    assert_eq!(cline, row + 1, "cursor on the typed line");
    assert_eq!(
        ccol, 5,
        "cursor is after 'hello' (col 5), not on the 'o' (col 4)"
    );
}

/// `<C-4>` is how macOS / xterm deliver Ctrl-\ (crossterm decodes byte 0x1c as
/// Ctrl+'4'), so `<C-4><C-n>` must also leave terminal mode.
#[tokio::test]
async fn ctrl_4_is_accepted_as_ctrl_backslash() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "<C-4><C-n>");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "<C-4><C-n> (macOS Ctrl-\\) should leave terminal mode"
    );
}

/// nxvim answers terminal status queries (like a real terminal) so apps that probe
/// the cursor position before drawing — fzf's inline finder, for one — don't stall.
/// Driving `cat`, we make it emit a real `ESC[6n` (DSR cursor-position request) on
/// its output; nxvim must write back a CPR (`ESC[row;colR`), which `cat` then echoes,
/// so a `;…R` reply lands in the buffer.
#[tokio::test]
async fn answers_cursor_position_report() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    // Type ESC[6n then <CR>: cat writes the line back raw, so the emulator sees a real
    // cursor-position request (not the caret-echoed input).
    feed(&rpc, "<Esc>[6n<CR>");
    wait_lines(&rpc, "a cursor-position-report reply", |ls| {
        ls.iter().any(|l| l.contains(';') && l.contains('R'))
    })
    .await;
}

/// Plain `<C-r>` reaches the child (shell reverse-search), while `<C-\><C-r>{reg}`
/// pastes a register into the terminal — the analogue of insert mode's `<C-r>{reg}`.
#[tokio::test]
async fn ctrl_backslash_ctrl_r_pastes_a_register() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    // Yank "hello" into register a (in the startup scratch buffer).
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "\"ayy");

    // Open a terminal and paste register a into it via the `<C-\><C-r>` chord.
    command(&rpc, "terminal cat").await;
    feed(&rpc, "<C-\\><C-r>a");
    wait_lines(&rpc, "register paste to reach cat", |ls| {
        has_line(ls, "hello")
    })
    .await;
}

/// Re-entering terminal mode (`i`) snaps the cursor back to the child's input
/// position, not wherever normal-mode navigation parked it.
#[tokio::test]
async fn reentering_snaps_cursor_to_the_input_position() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello");
    wait_lines(&rpc, "the echoed 'hello'", |ls| has_line(ls, "hello")).await;

    // Leave to terminal-normal and navigate up/to the start.
    feed(&rpc, "<C-\\><C-n>gg0");
    assert_eq!(mode(&rpc).await, "n");
    // Re-enter: the cursor jumps back to the input position (after "hello").
    feed(&rpc, "i");
    assert_eq!(mode(&rpc).await, "t");
    let (_cline, ccol) = cursor(&rpc).await;
    assert_eq!(
        ccol, 5,
        "re-entry snaps to the input position (col 5), not col 0"
    );
}

/// A terminal buffer's name is its window title: seeded from the spawned command,
/// then updated to the child's OSC title (`\e]2;…\a`) — what a desktop terminal
/// shows in its title bar.
#[tokio::test]
async fn terminal_buffer_name_tracks_the_window_title() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    assert_eq!(buf_name(&rpc).await, "cat", "name seeded from the command");

    // `cat` echoes its input, so feeding an OSC title sequence makes the emulator see
    // it (as if the child had emitted it): ESC ] 2 ; my-title BEL.
    feed(&rpc, "<Esc>]2;my-title<C-g><CR>");
    for _ in 0..200 {
        if buf_name(&rpc).await == "my-title" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        buf_name(&rpc).await,
        "my-title",
        "name follows the child's OSC window title"
    );
}

/// A live terminal buffer is read-only in terminal-normal mode: editing commands
/// (`x`, `dd`, …) are refused so they can't corrupt the mirrored screen. Once the
/// child exits, the buffer becomes an ordinary editable scratch buffer.
#[tokio::test]
async fn live_terminal_buffer_is_read_only() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello<CR>");
    wait_lines(&rpc, "cat to echo 'hello'", |ls| has_line(ls, "hello")).await;

    // Into terminal-normal, to the top, and try to delete — must be refused.
    feed(&rpc, "<C-\\><C-n>gg");
    let before = lines(&rpc).await;
    feed(&rpc, "ddx");
    assert_eq!(
        lines(&rpc).await,
        before,
        "edits must be refused while the terminal is live"
    );

    // End the child (EOF); the buffer becomes editable.
    feed(&rpc, "i<C-d>");
    wait_lines(&rpc, "the process-exit notice", |ls| {
        ls.iter().any(|l| l.contains("[Process exited 0]"))
    })
    .await;
    let before_dead = lines(&rpc).await;
    feed(&rpc, "ggdd");
    assert_ne!(
        lines(&rpc).await,
        before_dead,
        "a dead terminal buffer is editable"
    );
}

/// Start a clocked terminal session so the triple-`<Esc>` chord window is driven
/// deterministically (no wall-clock timing in the test).
async fn start_clocked() -> (Rpc, TestClock, UnboundedReceiver<Incoming>) {
    let clock = TestClock::new();
    let init = ServerInit {
        mouse_clock: Some(clock.handle()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, clock, incoming)
}

/// Triple-`<Esc>` in quick succession (each press within the chord window) leaves
/// terminal mode; the first two are still forwarded to the child.
#[tokio::test]
async fn triple_esc_quick_succession_leaves() {
    let _guard = serial_lock().lock().await;
    let (rpc, clock, _incoming) = start_clocked().await;

    clock.set_ms(1000);
    command(&rpc, "terminal cat").await;
    clock.set_ms(1000);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "t", "one escape stays");
    clock.set_ms(1100);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "t", "two escapes stay");
    clock.set_ms(1200);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "n", "three quick escapes leave");
}

/// Escapes spaced further apart than the chord window never leave — so a TUI program
/// inside the terminal still gets each lone `<Esc>`.
#[tokio::test]
async fn slow_escapes_stay_in_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, clock, _incoming) = start_clocked().await;

    clock.set_ms(0);
    command(&rpc, "terminal cat").await;
    for t in [1000u64, 2000, 3000] {
        clock.set_ms(t);
        feed(&rpc, "<Esc>");
        assert_eq!(
            mode(&rpc).await,
            "t",
            "a spaced-out escape stays in the job"
        );
    }
}

/// Phase 5 — the `nx.terminal` Lua control surface. `nx.terminal.open{ cmd = ... }`
/// opens a terminal job programmatically (the API twin of `:terminal`): the editor
/// enters terminal mode and the child's output mirrors into the buffer.
#[tokio::test]
async fn nx_terminal_open_runs_a_command_programmatically() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    exec_lua(&rpc, "nx.terminal.open{ cmd = 'cat' }").await;
    assert_eq!(
        mode(&rpc).await,
        "t",
        "nx.terminal.open enters terminal mode"
    );
    feed(&rpc, "hi<CR>");
    wait_lines(&rpc, "cat to echo 'hi'", |ls| has_line(ls, "hi")).await;
}

/// A list `cmd` is taken as argv verbatim, so an argument keeps its spaces — unlike
/// a string `cmd`, which whitespace-splits. `printf` echoes its format, so the
/// single argument `hello world` lands as one line.
#[tokio::test]
async fn nx_terminal_open_list_cmd_preserves_argument_spaces() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        r"nx.terminal.open{ cmd = {'printf', 'hello world\n'} }",
    )
    .await;
    wait_lines(&rpc, "printf to emit 'hello world'", |ls| {
        has_line(ls, "hello world")
    })
    .await;
}

/// `nx.terminal.open{ cwd = ... }` starts the child in the requested directory.
#[tokio::test]
async fn nx_terminal_open_respects_an_explicit_cwd() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    let dir = temp_dir("nx_term_cwd");
    let want = std::fs::canonicalize(&dir)
        .expect("canonicalize temp dir")
        .to_string_lossy()
        .into_owned();
    exec_lua(
        &rpc,
        &format!("nx.terminal.open{{ cmd = 'pwd', cwd = {dir:?} }}"),
    )
    .await;
    // A long path wraps across grid rows at the terminal width, so rejoin the rows
    // before matching (the wrap splits mid-path with no separator).
    wait_lines(&rpc, "pwd to print the requested cwd", |ls| {
        let joined: String = ls.iter().map(|l| l.trim_end()).collect();
        joined.contains(&want)
    })
    .await;
}

/// `:terminal` spawns the child with the editor's environment inherited, so a
/// variable exported in the parent is visible to the shell. Verified with
/// `printenv`, which prints a set variable and exits 0.
#[tokio::test]
async fn terminal_inherits_the_parent_environment() {
    let _guard = serial_lock().lock().await;
    // SAFETY: terminal tests hold `serial_lock`, so no other test in this binary
    // races this process-global mutation; separate suites are separate processes.
    std::env::set_var("NXVIM_TERM_ENV_TEST", "hello-from-parent");
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal printenv NXVIM_TERM_ENV_TEST").await;
    wait_lines(&rpc, "the child to see the inherited env var", |ls| {
        has_line(ls, "hello-from-parent")
    })
    .await;
}

/// `:terminal` starts the child in the editor's working directory, not `$HOME`.
/// `portable-pty` defaults a `None` cwd to the home directory, so without the
/// server filling in the process cwd the shell would open in the wrong place.
/// Verified by running `pwd` and matching its output to `current_dir()`.
#[tokio::test]
async fn terminal_starts_in_the_working_directory() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    let expect = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonicalize cwd")
        .to_string_lossy()
        .into_owned();

    command(&rpc, "terminal pwd").await;
    // `pwd` prints the working directory then exits; match it against the
    // editor's cwd (canonicalizing both, so a symlinked path still compares).
    wait_lines(&rpc, "pwd to print the editor's cwd", |ls| {
        ls.iter().any(|l| {
            let t = l.trim();
            !t.is_empty()
                && std::fs::canonicalize(t)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .as_deref()
                    == Some(expect.as_str())
        })
    })
    .await;
}

/// Phase 4 — color projection. A child that paints `red` in ANSI red
/// (`\e[31m` = the SGR foreground for the indexed color 1) must reach the client
/// as a `highlights` span whose resolved `styles` entry carries a red `fg`, the
/// same span shape treesitter emits — so every UI renders terminal color through
/// its existing styling path. We feed the ANSI through a file `cat`s out and then
/// keep `cat` blocked on stdin (`cat <file> -`), so the terminal stays *live* —
/// a dead terminal drops its emulator (and colors) and becomes plain text.
#[tokio::test]
async fn terminal_projects_ansi_color_into_highlight_spans() {
    let _guard = serial_lock().lock().await;
    let (rpc, mut incoming) = start().await;

    // Raw ANSI red `red`; the file path has no spaces, so the whitespace-split
    // argv (`cat <path> -`) stays three words. The trailing `-` makes `cat` read
    // stdin after the file, so it blocks open instead of exiting.
    let path = write_temp("term_color", "txt", "\x1b[31mred\x1b[0m\n");
    command(&rpc, &format!("terminal cat {path} -")).await;
    // The escapes are consumed by the emulator, so the buffer text is just `red`.
    wait_lines(&rpc, "cat to emit 'red'", |ls| has_line(ls, "red")).await;

    // xterm's canonical ANSI red (index 1) is #cd0000.
    const RED: u64 = 0x00cd_0000;
    for _ in 0..200 {
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            if first_red_fg_span(&map) == Some(RED) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for a red-fg terminal highlight span");
}

/// Phase 6 — scrollback. Output taller than the screen scrolls the earliest rows
/// off the live vt100 grid; they must be preserved in the buffer's scrollback so
/// terminal-normal navigation still reaches them. `cat <file> -` prints the lines
/// then blocks on stdin, keeping the terminal live (a dead terminal drops its
/// emulator and history).
#[tokio::test]
async fn scrollback_preserves_lines_that_scrolled_off_the_screen() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await; // 80x24

    let body: String = (1..=50).map(|i| format!("line{i}\n")).collect();
    let path = write_temp("term_scroll", "txt", &body);
    command(&rpc, &format!("terminal cat {path} -")).await;
    wait_lines(&rpc, "the last line to print", |ls| has_line(ls, "line50")).await;

    // The earliest line scrolled off the 24-row grid, but scrollback keeps it as
    // the buffer's first line (without scrollback the top would be ~line27).
    let ls = lines(&rpc).await;
    assert_eq!(
        ls.first().map(String::as_str),
        Some("line1"),
        "the scrolled-off first line is preserved at the top of the buffer"
    );
    assert!(has_line(&ls, "line50"), "the live bottom is present too");

    // `gg` reaches the earliest history line.
    feed(&rpc, "<C-\\><C-n>gg");
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "gg lands on the first (scrolled-off) line"
    );
}

/// A scrolled-off row keeps its color: history rows project from the per-cell
/// styles captured when they scrolled, not from live vt100 cells (which no longer
/// hold them). `gg` brings the red first line into the viewport so its highlight
/// is in the frame.
#[tokio::test]
async fn scrollback_rows_keep_their_color() {
    let _guard = serial_lock().lock().await;
    let (rpc, mut incoming) = start().await;

    let mut body = String::from("\x1b[31mredline\x1b[0m\n");
    for i in 2..=40 {
        body.push_str(&format!("line{i}\n"));
    }
    let path = write_temp("term_scroll_color", "txt", &body);
    command(&rpc, &format!("terminal cat {path} -")).await;
    wait_lines(&rpc, "the last line to print", |ls| has_line(ls, "line40")).await;
    assert_eq!(
        lines(&rpc).await.first().map(String::as_str),
        Some("redline"),
        "the red line scrolled into history at the top"
    );

    // Scroll the viewport to the top so the history row is rendered this frame.
    feed(&rpc, "<C-\\><C-n>gg");
    const RED: u64 = 0x00cd_0000;
    for _ in 0..200 {
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            if first_red_fg_span(&map) == Some(RED) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for the scrolled-off red line's color");
}

/// A flood of output — more lines than the scrollback cap — must not freeze the
/// editor: each PTY burst splices only the changed tail + evicts the front, so the
/// per-burst cost is bounded (not a full-buffer rebuild, which made `rg` dumping
/// thousands of matches quadratic). The buffer stays capped near the scrollback
/// limit, keeps the most recent output, and drops the oldest. Completing this at
/// all (well within the poll budget) is the regression check.
#[tokio::test]
async fn heavy_output_stays_bounded_and_does_not_freeze() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await; // 80x24

    // 12000 lines — past the 10000-row scrollback cap, so eviction + the saturated
    // capture path both run. `cat <file> -` prints them then blocks on stdin.
    const N: usize = 12_000;
    const CAP: usize = 10_000; // mirrors terminal.rs SCROLLBACK_CAP
    let body: String = (1..=N).map(|i| format!("line{i}\n")).collect();
    let path = write_temp("term_flood", "txt", &body);
    command(&rpc, &format!("terminal cat {path} -")).await;

    // Poll cheaply (line count + a small tail) until the last line lands.
    for _ in 0..200 {
        let lc = line_count(&rpc).await;
        if lc > 100 {
            let tail = lines_range(&rpc, lc as i64 - 40, lc as i64).await;
            if tail.iter().any(|l| l.trim_end() == "line12000") {
                // Capped, not grown to 12000: the buffer holds ~cap + one screen.
                assert!(
                    lc <= CAP + 64,
                    "buffer should stay capped near the scrollback limit, got {lc} lines"
                );
                // Oldest evicted, most recent kept.
                let head = lines_range(&rpc, 0, 1).await;
                assert_ne!(
                    head.first().map(String::as_str),
                    Some("line1"),
                    "the oldest line is evicted once output passes the cap"
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out streaming {N} lines — heavy output should never stall");
}

/// The `fg` color (`0xRRGGBB`) of the first highlight span in the focused
/// window's `highlights`, resolved through the frame's `styles` palette, or
/// `None` if there is no styled span yet.
fn first_red_fg_span(map: &[(Value, Value)]) -> Option<u64> {
    let styles = map_get(map, "styles")?.as_array()?;
    let rows = window0_field(map, "highlights")?.as_array()?;
    for row in rows {
        for span in row.as_array()?.iter() {
            let span = span.as_array()?;
            let style_id = span.get(3)?.as_u64()? as usize;
            let Value::Map(entry) = styles.get(style_id)? else {
                continue;
            };
            if let Some(fg) = entry
                .iter()
                .find(|(k, _)| k.as_str() == Some("fg"))
                .and_then(|(_, v)| v.as_u64())
            {
                return Some(fg);
            }
        }
    }
    None
}
