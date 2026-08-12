# bemtvi code review — refactoring, security & reliability (2026-06-02)

A full read of all 7 crates (~13k LOC). Each finding below is **self-contained
and implementation-ready**: file + line range, what's wrong, and a concrete fix.
Work them in any order; they are independent unless a "Depends on" note says
otherwise. Line numbers are from commit `f810ea1` and may drift — search for the
quoted code if they don't match.

## Ground rules for implementing these (from CLAUDE.md)

- **No unit tests.** Behavior is verified end-to-end. Add coverage to
  `crates/bemtvi-server/tests/editing.rs` (helpers `start`, `feed`, `lines`,
  `cursor`) or `crates/bemtvi-server/tests/buffers.rs`. Do **not** add `#[test]`
  unit tests inside the crates. For TUI/paint changes use
  `crates/bemtvi-tui/tests/` and `crates/bemtvi/tests/screen.rs`.
- **`bemtvi-core` stays pure & synchronous** — no async, no I/O beyond
  `Buffer` file read/write, no transport types.
- **Build / lint / test** (never `--all-features` — it breaks `mlua-sys`):
  ```sh
  cargo build
  cargo clippy --all-targets -- -D warnings
  cargo test --workspace
  cargo test -p bemtvi-server --test editing <name>   # single test
  ```
- **Text model invariants:** byte-offset indexing; the rope **always** ends in a
  trailing `\n` (`line_count == rope.len_lines() - 1`, phantom last line never
  shown/edited); call `Buffer::normalize()` after mutations; snap byte ranges to
  char boundaries.
- Dependencies are pinned exactly (`=x.y.z`) in the root `Cargo.toml`
  `[workspace.dependencies]`; pull into a crate with `<dep>.workspace = true`.
- **Test-first for every fix.** Before changing the code, write the integration
  test that reproduces the issue. It **must fail (or hang/abort) before the fix
  and pass after** — verify both directions (e.g. stash the fix, run the test,
  confirm it fails, restore). Black-box only, through public APIs; a transport
  bug that can't be reached through the editor surface goes in a `tests/`
  integration test on the relevant crate (see `crates/bemtvi-rpc/tests/transport.rs`).

## Verified facts (don't re-investigate)

- `ropey` 2.0.0-beta.1 `get_char(byte_idx: usize)` is **byte-indexed** and
  returns `Err(NonCharBoundary)` for a mid-codepoint byte. `Buffer::normalize`
  / `ensure_trailing_newline` (`buffer.rs:255-285`) are therefore **correct**,
  including the multibyte-tail case. *This was the only potential-High in core
  and it is a non-issue — do not "fix" it.*
- `get_lines` slice clamping (`server lib.rs:531`) is safe.
- Motion count is never `Some(0)` (a leading `0` is a motion), so `n - 1`
  underflows in `resolve_motion` cannot happen.
- The `BufferStore` `expect`s (`editor.rs:230-240`) are upheld by invariants and
  unreachable from RPC input.

---

# P0 — Reliability/security, small and high-value

## R1. RPC reader: nested-input stack-overflow DoS, malformed-input hang, unbounded memory ✅ DONE
**File:** `crates/bemtvi-rpc/src/lib.rs` (`reader_task`)
**Severity:** High. **Trust:** decoded bytes are untrusted in the remote case.
**Status:** Implemented (commit pending). Tests: `crates/bemtvi-rpc/tests/transport.rs`.

**Problem (as found — worse than originally documented):**
1. **Process abort via nested input.** rmpv's decoder is recursive with a depth
   limit of 1024, but ~600 nested array markers (`0x91` repeated) recurse deep
   enough to **overflow the reader thread's stack and SIGABRT the whole process**
   *before* the depth guard fires. Verified: feeding 512 bytes of `0x91` aborts
   the pre-fix binary. This is untrusted-input → process death, not just a hang.
2. **Hang on structural error.** `read_value` returning `Err` was treated as
   "truncated, read more". A non-EOF decode error (e.g. `DepthLimitExceeded`)
   never drains, so the loop spins forever — connection hangs, `buf` unbounded.
3. **No max-buffer cap.** `buf.extend_from_slice` grew unbounded; a peer
   streaming bytes that never complete a value (e.g. a `str32` header claiming
   4 GiB followed by filler) grows the buffer without limit. (Note: rmpv 1.3.1
   already caps str/bin preallocation at 64 KiB and never preallocates
   arrays/maps per issue #151, so the *length prefix* alone can't OOM — only the
   buffer holding streamed bytes can.)

**Fix applied:**
- Decode via `rmpv::decode::read_value_with_max_depth(&mut cur, MAX_DEPTH)` with
  `MAX_DEPTH = 128` — well below stack-overflow territory, well above any real
  payload. Over-nesting now surfaces as a clean `DepthLimitExceeded`.
- Distinguish incomplete from corrupt by `Error::kind()`: `UnexpectedEof` ⇒ wait
  for more bytes (`Ok(None)`, break inner loop); any other error ⇒ `return` from
  `reader_task` (tear the connection down).
- `MAX_FRAME = 64 MiB` cap: if `buf` grows past it without producing a value,
  `return`.

**Tests (verified fail-before / pass-after):**
- `malformed_frame_closes_connection_instead_of_hanging` — feeds 512 nested
  `0x91`; pre-fix SIGABRTs (stack overflow), post-fix tears down cleanly so
  `incoming.recv()` returns `None` within a timeout.
- `split_frame_is_reassembled_and_dispatched` — guard that a genuinely truncated
  frame (sent in two halves) is still reassembled, i.e. the teardown doesn't
  over-react to short reads.

**Still open / follow-up:** pairs with **R6** (on teardown, also drain `pending`
so in-flight `request().await` callers get `Err` instead of hanging). The
per-chunk full re-parse is O(n²) over a large buffered frame — not fixed here;
worth a separate streaming-decode pass.

## R2. Server can be wedged by its own Lua — unbounded fixpoint loop ✅ DONE
**File:** `crates/bemtvi-server/src/lib.rs` (`run_pending`)
**Severity:** High (DoS via user's own config/plugin).
**Status:** Implemented. Test: `editing.rs::recursive_user_command_does_not_wedge_the_server`.

**Problem:** The loop drains `lua_queue`, `deferred_commands`, and
`panel_selects` until all three are empty. A user command or an `on_select`
callback that re-queues a command/lua/panel-op every round makes this loop
forever, freezing the single-threaded server — no RPC message is processed, the
client appears dead. Neovim bounds this (`maxfuncdepth`, E132).

**Fix applied:** Added `const MAX_ROUNDS: usize = 100;` and a round counter.
After draining, if work remains and the count hits the cap, clear the three
queues, `echo("E132: command recursion limit exceeded")`, and `break`. The
early-exit when all queues are empty is unchanged, so any legitimate finite
chain (< 100 levels) converges normally.

**Test (verified fail-before / pass-after):** registers a user command
`Loop` whose callback runs `vim.cmd('Loop')`, triggers `:Loop<CR>` wrapped in a
5s timeout. Pre-fix the request never returns (server spins) → timeout → fail;
post-fix it returns with the E132 message and a follow-up edit
(`ihi<Esc>` → `nvim_buf_get_lines`) still works, proving the loop stays
responsive.

## R3. TUI does not restore mouse mode on panic ("bricks the terminal") ✅ DONE
**File:** `crates/bemtvi-tui/src/lib.rs` (`run`, `MouseCapture`)
**Severity:** High (user-visible terminal corruption).
**Status:** Implemented. Test: `crates/bemtvi-tui/tests/mouse.rs`.

**Problem:** ratatui's panic hook restores raw mode + alternate screen, but
`EnableMouseCapture` was enabled *outside* that hook and only disabled on the
normal return path. A panic mid-render (or any unwind in the event loop) skipped
`DisableMouseCapture`, leaving the terminal emitting mouse escape sequences. The
old "OS resets the terminal anyway" comment was wrong for an unwind that returns
to `main.rs` and joins the thread.

**Fix applied:** Added a `pub struct MouseCapture<W: Write>` RAII guard:
`enable()` turns mouse capture on, `Drop` turns it off — so it fires on the
normal return *and* the panic-unwind path. `run()` holds the guard across the
event loop and drops it before `ratatui::restore()`. Generic over the writer so
it's testable against an in-memory sink; production passes `std::io::stdout()`.

**Test (verified fail-before / pass-after):** `mouse.rs` drives the guard over a
shared in-memory `Write` and asserts the exact `DisableMouseCapture` byte
sequence is emitted on (a) normal scope exit and (b) a `catch_unwind` panic
inside the guarded scope. Verified both fail when the `Drop` body is neutered
(the pre-fix "disable skipped on panic" behavior) and pass with the guard.

**Verify:** hard to unit-test; reason it through and confirm `cargo test -p
bemtvi` (screen/e2e) still passes. Optionally a manual note in the PR.

---

# P1 — Trust-boundary hardening (TS worker)

## S1. TS worker: path traversal → arbitrary `dlopen` if `lang` becomes untrusted ✅ DONE
**File:** `crates/bemtvi-ts/src/loader.rs` (`Grammar::load`, `is_valid_language`).
**Severity:** Medium (defense-in-depth; **not reachable today**).
**Status:** Implemented. Test: `crates/bemtvi-ts/tests/worker.rs::rejects_language_names_that_escape_the_data_dir`.

**Problem:** `parser_path` does `dir.join(format!("{lang}.{ext}"))` and
`query_path` does `data_dir.join("queries").join(lang).join(file)` with `lang`
unvalidated. A `lang` of `../../../...` or an absolute path escapes `data_dir`,
and `libloading::Library::new` then executes the loaded object's constructors =
native code execution. Today `lang` only ever comes from the server's fixed
`filetype_of` table, so it is not attacker-reachable — but the worker is a
separate trust boundary and must not assume its caller.

**Fix applied:** `Grammar::load` rejects any `lang` failing `is_valid_language`
(non-empty, only `[A-Za-z0-9_-]`) with `anyhow!("invalid language name '{lang}'")`
*before* any path join or `dlopen`. Excluding `.`, `/`, `\` makes traversal and
absolute-path escapes impossible; real grammar names (`rust`, `c_sharp`, `tsx`)
pass.

**Test (verified fail-before / pass-after):** drives `run_worker` over a pipe,
sends `ts_open` with `language: "../../../../etc/passwd"`, asserts the `ts_error`
reply message contains "invalid language name". Pre-fix the message was the
generic "no parser for …" (it still hit the filesystem); post-fix it's rejected
up front.

## S2. TS worker: edit offsets from the wire aren't bounds/boundary-checked ✅ DONE
**File:** `crates/bemtvi-ts/src/engine.rs` (`edit`) and `parse_edits` in
`crates/bemtvi-ts/src/lib.rs`.
**Severity:** Medium (per-buffer silent crash-loop).
**Status:** Implemented. Test: `crates/bemtvi-ts/tests/worker.rs::malformed_edit_neither_crashes_nor_silences_the_buffer`.

**Problem:** `state.shadow.remove(e.start_byte..e.old_end_byte)` and
`insert(e.start_byte, …)` used offsets straight off the wire. An
`old_end_byte > shadow.len()`, `start_byte > len`, or a mid-codepoint offset
panics ropey. The surrounding `catch_unwind` swallows the panic but the handler's
follow-up `send_highlights` never runs, so the buffer goes silently dark (and a
partially-applied batch leaves the shadow/tree desynced). `parse_edits` also
coerced any missing/non-int field to `0`, turning a garbled edit into an
inconsistent `InputEdit`.

**Fix applied:**
1. `parse_edits` now reads each numeric field as `Option` and `?`-drops the
   whole delta if any field is absent/non-integer (no more `unwrap_or(0)`).
2. `Engine::edit` validates each delta against the live `shadow.len()` and
   `is_char_boundary(start/old_end)` and `continue`s past an invalid one; the
   actual mutations go through ropey's fallible `try_remove`/`try_insert` as a
   second guard, so a mutation can never panic. The buffer stays alive and keeps
   highlighting.

**Test (verified fail-before / pass-after):** opens a real rust buffer (compiled
tree-sitter-rust fixture), drains its highlights, then sends a `ts_edit` whose
offsets run far past the text and asserts a `ts_highlights` reply (tick=2) still
arrives. Pre-fix the worker panicked (`index is out of bounds`), was caught by
`catch_unwind`, and sent no reply → the test timed out.

**Note:** the review also suggested `engine.close(buffer)` on a caught panic in
`lib.rs` as a third layer; with the offsets now validated the panic no longer
occurs, so that belt-and-suspenders reset was left out to keep the change
minimal. Revisit if other panic sources in `ts_edit` surface.

## S3. `vim.fn.mkdir` ignores its perms argument ✅ DONE
**File:** `crates/bemtvi-lua/src/lib.rs` (`mkdir`, `parse_mode`, `create_dir_all_mode`).
**Severity:** Low (cheap, concrete).
**Status:** Implemented. Test: `editing.rs::mkdir_honors_the_permissions_argument`.

**Problem:** `mkdir(path, _flags)` discarded the perms arg and called
`std::fs::create_dir_all`, producing `0777 & !umask` dirs. Data/state dirs that
should be private got umask-default perms.

**Fix applied:** `mkdir` takes neovim's third `prot` argument (octal string like
`"0700"` or a numeric mode); `parse_mode` resolves it (default `0o755`) and
`create_dir_all_mode` applies it via `std::os::unix::fs::DirBuilderExt::mode` on
Unix (`std::fs::create_dir_all` elsewhere).

**Test (verified fail-before / pass-after):** an init.lua calling
`vim.fn.mkdir(path, "p", "0700")` runs at startup; the test asserts the created
dir's mode is `0o700`. Pre-fix it was `0o755` (umask default); post-fix `0o700`.

> The broader Lua surface (`unsafe_new_with`, `debug` stdlib, C-module loading,
> unvalidated FS/env paths) is **accepted** under the "config is user-trusted"
> model — matches neovim. No change beyond S3 and the existing code comments.

---

# P2 — Reliability (worker supervision, startup, shutdown)

## R4. Startup file-open failure is silently swallowed ✅ DONE
**File:** `crates/bemtvi-server/src/lib.rs:139`
**Severity:** Medium (data-loss footgun).
**Status:** Implemented. Test:
`editing.rs::unreadable_startup_file_keeps_its_name_and_echoes_the_error`.

**Problem:** `Editor::open(path).unwrap_or_else(|_| Editor::new())` — `bemtvi
file.txt` on a permission error (or a directory, etc.) silently opens a *blank,
unnamed* buffer; a later `:w` could clobber.

**Fix applied:** New `Editor::open_or_named(path)` (replaces the
`open(...).unwrap_or_else(Editor::new)` call in `run()`): on a read failure it
builds a buffer *bound to* `path` via the new `Buffer::named(path)` (an empty
buffer with the path set, no FS touch) and `echo`s
`E484: Can't open file {path}: {err}`. The buffer keeps its name, so a later
`:w` targets the intended file; a *missing* file is still the non-error
new-file-buffer path inside `from_file`.

**Test (verified fail-before / pass-after):** starts the server with a
*directory* as `init.file` (reads fail with EISDIR — portable, no permission
fiddling), asserts `nvim_buf_get_name(0)` equals the path (pre-fix it was `""`)
and that the startup message names the file.

## R5. TS worker supervision — three weaknesses ✅ DONE
**File:** `crates/bemtvi-server/src/syntax.rs`
**Severity:** Medium.
**Status:** Implemented. Test:
`crates/bemtvi/tests/syntax.rs::an_unspawnable_worker_disables_syntax_instead_of_looping_forever`.

1. **Breaker resets too eagerly** (`:236-238`): on hitting `MAX_CRASHES`, it
   slept `COOLDOWN` then `crashes.clear()`. A permanently-poison grammar
   respawned ~3×/30s forever.
2. **Spawn failures have no breaker** (`:176-189`): if `spawn()` kept failing
   (binary missing, fork limit) the loop retried every 1s forever and emitted no
   event, so the server never learned syntax was unavailable.
3. **Child not reaped deterministically** (`:222`): `let _ = child.start_kill()`
   sent SIGKILL but never `await child.wait()`; reaping relied on `kill_on_drop`
   + drop timing. Missing stdio pipes `return`ed, permanently killing
   supervision.

**Fix applied:** The supervisor now drives one worker lifetime through
`run_worker_once` and feeds *every* failure — spawn error, missing stdio pipe,
or crash of a live child — into a single breaker over a sliding `WINDOW`
(`10s`):
- **(1, 2)** Escalating backoff (`200ms` doubling per windowed failure, capped at
  `5s`) replaces the eager `clear()`; once failures within the window reach
  `GIVE_UP = 5`, the supervisor emits a new one-shot `SyntaxEvent::Disabled`,
  stops respawning, and idles draining commands (so the client's `send`s never
  error) until the server exits. The server turns `Disabled` into a single
  user-facing echo ("treesitter: syntax worker unavailable, highlighting
  disabled"). Spawn failures flow through the same path, so a missing binary is
  now bounded and surfaced instead of retried forever.
- **(3)** `run_worker_once` always reaps with `start_kill()` **then**
  `child.wait().await` (deterministic, no zombie/`kill_on_drop` reliance), and a
  missing stdio pipe now returns a *failure* (→ breaker retry) instead of
  `return`ing out of supervision.

**Test (verified fail-before / pass-after):** points `BEMTVI_TS_WORKER` at a
non-existent binary (saved/restored under the suite lock), opens a `.rs` buffer,
and polls redraws for the "highlighting disabled" message. Pre-fix (spawn
failures retry forever, no event) the message never arrives and the test times
out (~10s); post-fix the breaker gives up after ~3s of escalating backoff and
the message surfaces.

## R6. RPC task death leaks peer task and hangs pending requests ✅ DONE
**File:** `crates/bemtvi-rpc/src/lib.rs` (`connect`).
**Severity:** Medium.
**Status:** Implemented. Test:
`crates/bemtvi-rpc/tests/transport.rs::in_flight_request_fails_when_the_connection_drops`.

**Problem:** When one task died (`break`/`return` on I/O error or EOF) it didn't
signal or abort the other, and — more importantly — entries in `pending` were
never drained, so every outstanding `request().await` waited on a
`oneshot::Receiver` whose `Sender` lingered in the map; in-flight requests hung
until the whole `Rpc` was dropped.

**Fix applied:** `connect` now spawns a small coordinator task that `select!`s on
both the reader and writer `JoinHandle`s. When either ends, it `abort()`s the
survivor (so a dropped reader can't leave the writer parked on `out_rx`, and vice
versa — the `JoinHandle`-abort shutdown signal the review suggested) and then
`pending.lock().clear()`s, dropping every in-flight `oneshot::Sender` so each
`request().await` resolves to `Err("rpc connection closed")`. Draining in the
coordinator (rather than inside `reader_task`) covers teardown initiated from
*either* side, including the writer-first case where the reader is aborted before
it could drain.

**Test (verified fail-before / pass-after):** fires a request the peer never
answers, then drops both peer halves so the reader hits EOF. Pre-fix the spawned
`request()` task blocks forever (sender lingers in `pending`) and the 2s timeout
fires; post-fix the request resolves to an error promptly.

**Pairs with R1** (structural-error teardown is the trigger for this drain).

## R7. Worker event channel unbounded + redraw per notification ✅ DONE
**File:** `crates/bemtvi-server/src/syntax.rs` (event channel),
`crates/bemtvi-server/src/lib.rs` (`run` loop, `on_syntax_event`, `store_spans`).
**Severity:** Medium.
**Status:** Implemented. Test:
`crates/bemtvi/tests/syntax.rs::an_edit_proactively_repaints_coalesced_highlights`.

**Problem:** A flooding/buggy worker could grow the event channel without bound;
each `ts_highlights` reply triggered a full `redraw()` (re-projecting the whole
view).

**Fix applied (all three):**
- **Bounded channel.** The worker→editor event channel is now
  `mpsc::channel(EVENT_CAPACITY = 1024)`; the supervisor `send().await`s, so past
  the cap it *backpressures* (throttling the worker through its stdout pipe)
  instead of buffering without limit. The command channel stays unbounded (the
  editor never floods it).
- **Coalesced redraws.** `on_syntax_event` now sets a `syntax_dirty` flag instead
  of redrawing; the `run` loop drains every queued event with `try_recv()` and
  `redraw()`s at most once per turn. A burst of replies costs one re-projection,
  not one each.
- **Clamped line keys.** `store_spans` looks up the target buffer's line count
  (new cheap `Editor::line_count_of`) and skips any span whose `line >=
  line_count`, so a bogus `line` (e.g. `u64::MAX`) can't seed a junk cache entry.

**Test (verified fail-before / pass-after):** after an edit, waits for an
*unsolicited* redraw (no `barrier` polling, which would itself trigger a
client-path redraw and mask the bug) carrying the new row 1's `fn` keyword.
With the coalesced `redraw()` removed the async repaint never arrives and the
test times out; with it, the proactive repaint lands. (The bounded-channel and
line-clamp parts are non-behavioral hardening — not separately observable through
the editor surface — and are covered by the full green syntax suite.)

## R8. `reparse` discards the last good tree on a `None` result ✅ DONE
**File:** `crates/bemtvi-ts/src/engine.rs` (`BufferState::reparse`)
**Severity:** Low.
**Status:** Implemented (defensive; see note on testability).

**Problem:** `parse_with_options(...)` returning `None` (timeout/cancel) set
`self.tree = None`, throwing away the previous good tree and all incremental
reuse until a full re-open.

**Fix applied:** `if let Some(tree) = parse_with_options(...) { self.tree =
Some(tree); }` — keep the prior tree on `None`.

**Testability:** the `None` branch is **unreachable today** — `reparse` passes
`None` for the parse options, so no timeout or cancellation is configured and
`parse_with_options` always returns `Some`. There is therefore no fail-before /
pass-after path through the public worker surface; this is a defensive guard for
a future where a parse timeout is added. The existing worker + syntax suites
(which reparse on every `open`/`edit`) cover that reparse still yields a tree.

## R9. Server-thread panic looks like a clean quit ✅ DONE
**File:** `crates/bemtvi/src/main.rs`
**Severity:** Medium.
**Status:** Implemented. Test: `crates/bemtvi/tests/e2e.rs::a_server_thread_panic_exits_nonzero`.

**Problem:** `let _ = server_thread.join()` discarded the panic payload; the exit
code stayed `0`, so a server crash was indistinguishable from a normal quit.

**Fix applied:** `main` now inspects `server_thread.join()`; on `Err` it prints
`bemtvi: server thread panicked: <message>` (via a `panic_message` helper that
downcasts the payload to `&str`/`String`) and `std::process::exit(101)` (Rust's
conventional panic code). This check takes precedence over the client's
`result`, since a crashed server is the more important failure to surface.

**Test (verified fail-before / pass-after):** a debug-only, env-gated
fault-injection hook (`BEMTVI_PANIC_TEST`, behind `#[cfg(debug_assertions)]` so it
is compiled out of release builds) forces the server thread to panic at startup.
The Tier-3 PTY test spawns the real binary with that env set and asserts the
process exits with code `101`. Pre-fix the process exited `0` (the panic was
swallowed); post-fix it exits `101`.

## R10. `--__ts-worker` matched anywhere in argv ✅ DONE
**File:** `crates/bemtvi/src/main.rs`
**Severity:** Low.
**Status:** Implemented. Test: `crates/bemtvi/tests/e2e.rs::ts_worker_flag_past_argv1_still_opens_the_editor`.

**Problem:** `std::env::args().any(|a| a == TS_WORKER_FLAG)` turned the editor into
a worker if the flag appeared as *any* argument (e.g. a file literally named that).

**Fix applied:** Check only the first argument:
`std::env::args().nth(1).as_deref() == Some(TS_WORKER_FLAG)` — exactly how the
server spawns the worker (the flag is always argv[1]).

**Test (verified fail-before / pass-after):** the Tier-3 PTY test spawns
`bemtvi <file> --__ts-worker` (flag as a trailing positional) and asserts the
editor opens the file. Pre-fix the `any()` match ran the headless worker (blank
screen, stdin read as RPC) → timeout; post-fix the file's contents render.

## R11. Zero-duration scroll animation divides by zero → NaN ✅ DONE
**File:** `crates/bemtvi-tui/src/lib.rs` (`arm_animation`, `render`)
**Severity:** Low (one-frame glitch, not a panic — `as usize` saturates NaN→0).
**Status:** Implemented. Test:
`crates/bemtvi-tui/tests/paint.rs::a_zero_duration_scroll_gesture_does_not_arm_an_animation`.

**Fix applied (both):** `arm_animation` returns `None` for a scroll gesture whose
`duration.is_zero()` — a degenerate slide is never armed; the redraw already
carries the static destination viewport, so it's shown directly. As a belt-and-
suspenders guard, `render` also computes progress as `1.0` when
`a.duration.is_zero()` instead of dividing, so progress can never become
NaN/inf even if an animation were armed some other way.

**Test (verified fail-before / pass-after):** drives a synthetic scroll redraw
with `duration_ms = 0` through `ScrollHarness` and asserts `!animating()` (no
degenerate animation armed) and that the static destination paints. Pre-fix the
zero-duration gesture armed an animation (`animating()` was `true`); post-fix it
doesn't.

---

# P3 — Refactoring (maintainability; no behavior change)

These are larger and best done as separate, focused passes with the test suite
green before and after. None should change observable behavior.

## F1. `editor.rs` (3206 lines) — extract duplicated logic
**File:** `crates/bemtvi-core/src/editor.rs`
- Duplicated **linewise change** body at `:1271-1288` and `:1318-1334`, and the
  **linewise delete cursor-settle** at `:1263-1266` vs `:1300-1303`. Extract
  `fn linewise_change(&mut self, lo, hi, first_line)` and
  `fn settle_after_linewise_delete(&mut self, first_line)`; call from both
  `apply_operator` and `visual_operate`.
- `resolve_motion` (`:992-1135`) is a 140-line match rebuilding
  `MotionResult { target: self.buffer().byte_at(line, col), kind, axis }` in
  nearly every arm. Add `MotionResult::horizontal/linewise/inclusive`
  constructors. Pull the `w`/`W` `cw`-acts-like-`ce` special case (`:1083-1109`)
  into a named helper.
- Three identical `Snapshot { text: self.buffer().text.clone(), cursor:
  self.cursor }` sites in `push_undo` (`:2871`), `undo` (`:2885`), `redo`
  (`:2902`). Extract `fn snapshot(&self) -> Snapshot`; fold `undo`/`redo` into a
  shared `fn restore(&mut self, from_undo: bool)`.
- The `self.buffer().line_count().saturating_sub(1)` "last line" idiom recurs
  ~12× (`:994, 1041, 1264, 1277, 1301, 1323, 2236, 3041, 3067, 3099`). Add
  `fn last_line(&self) -> usize`.

## F2. `bemtvi-server` dispatch & redraw boilerplate
**File:** `crates/bemtvi-server/src/lib.rs`
- `redraw()` (`:543-661`) is ~120 lines; the scroll-band map (`:566-588`) and
  the main map (`:606-658`) duplicate lines/selection/numbers/highlights
  projection. Extract `fn project_band(...) -> Value` and `fn project_panel(p)
  -> Value`.
- `store_spans` (`:780-793`) and several dispatch arms hand-roll
  `map.iter().find(|(k,_)| k.as_str() == Some("...")) ...` even though `map_get`
  exists (`:1074`). Reuse it; add typed `u64_at(map, key, default)` /
  `str_at(...)` helpers and collapse the `.and_then(...).unwrap_or(...)` chains
  in `dispatch` (`:209-336`). Consider splitting panel/hl groups into
  `dispatch_panel` / `dispatch_hl`.

## F3. `bemtvi-tui/src/lib.rs` (1216 lines) — split into submodules
**File:** `crates/bemtvi-tui/src/lib.rs`
Five independent concerns in one module: event loop/transport (`:42-142`),
`View` model + msgpack parsing (`:160-424`), scroll-animation state machine
(`:228-321`), renderer (`:485-975`), key encoding (`:1177-1216`). Split into
`view.rs`/`parse.rs`, `render.rs`, `anim.rs`, `keys.rs` and re-export the public
surface (`run`, `paint`, `View`, `encode_key`, `close_button`, `ScrollHarness`)
from `lib.rs`. Mechanical but large. Minor while in here:
- `undercurl` aliases to `Modifier::UNDERLINED` (`:1130-1141`), colliding with
  `underline` (ratatui has no undercurl). Add a clarifying comment — `HlSet`
  (`bemtvi-lua/src/lib.rs:36-37`) carries them as distinct.
- ~20 repeated `map_u64(...) as u16` truncating casts (`:336-414`); add a
  `map_u16`/`map_usize` helper that documents the saturation. Add
  `map_str_array(map, key) -> Vec<String>` for the `lines`-array idiom
  duplicated at `:328-335, 377-384, 398-405`.

## F4. Wire-format duplication between server and TS worker
**Files:** `crates/bemtvi-server/src/lib.rs:954` (`edits_value`),
`crates/bemtvi-ts/src/lib.rs:162` (`parse_edits`), and the span tuple shape in
both. The 10-tuple edit encoding and the span tuple are hand-mirrored across two
crates with only doc-comments tying them; drift = silent wrong highlights.
**Fix:** define the wire layout once — a shared struct (serde) or a single shared
`encode`/`decode` pair in a common location (e.g. `bemtvi-rpc` or a small shared
module) so both sides agree by construction.

## F5. Minor core polish
- `find_buffer_by_path` (`editor.rs:599-609`) calls `std::fs::canonicalize` —
  **filesystem I/O in the pure core**, an architecture-rule violation and a
  blocking syscall on every `:e`. Move path canonicalization to the server
  layer, or compare normalized `PathBuf`s without touching disk.
- `messages` history (`echo`, `editor.rs:716-722`) grows unbounded. Cap it with
  a bounded ring (vim caps `:messages`).
- `unicode.rs:38-50` `floor_grapheme` is an O(line-length) scan per call on the
  cursor hot path; add an `if line.is_ascii() { return byte.min(line.len()); }`
  fast path.

---

## Suggested implementation order

1. **P0** (R1, R2, R3) — small, high-value reliability fixes. R1 + R6 together.
2. **P1** (S1, S2, S3) — cheap trust-boundary hardening.
3. **P2** (R4–R11) — supervision/shutdown robustness.
4. **P3** (F1–F5) — refactors, each its own PR with green tests before/after.

After every change: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
`cargo test --workspace`.
