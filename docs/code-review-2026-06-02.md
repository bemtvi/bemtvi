# nxvim code review — refactoring, security & reliability (2026-06-02)

A full read of all 7 crates (~13k LOC). Each finding below is **self-contained
and implementation-ready**: file + line range, what's wrong, and a concrete fix.
Work them in any order; they are independent unless a "Depends on" note says
otherwise. Line numbers are from commit `f810ea1` and may drift — search for the
quoted code if they don't match.

## Ground rules for implementing these (from CLAUDE.md)

- **No unit tests.** Behavior is verified end-to-end. Add coverage to
  `crates/nxvim-server/tests/editing.rs` (helpers `start`, `feed`, `lines`,
  `cursor`) or `crates/nxvim-server/tests/buffers.rs`. Do **not** add `#[test]`
  unit tests inside the crates. For TUI/paint changes use
  `crates/nxvim-tui/tests/` and `crates/nxvim/tests/screen.rs`.
- **`nxvim-core` stays pure & synchronous** — no async, no I/O beyond
  `Buffer` file read/write, no transport types.
- **Build / lint / test** (never `--all-features` — it breaks `mlua-sys`):
  ```sh
  cargo build
  cargo clippy --all-targets -- -D warnings
  cargo test --workspace
  cargo test -p nxvim-server --test editing <name>   # single test
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
  integration test on the relevant crate (see `crates/nxvim-rpc/tests/transport.rs`).

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
**File:** `crates/nxvim-rpc/src/lib.rs` (`reader_task`)
**Severity:** High. **Trust:** decoded bytes are untrusted in the remote case.
**Status:** Implemented (commit pending). Tests: `crates/nxvim-rpc/tests/transport.rs`.

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
**File:** `crates/nxvim-server/src/lib.rs` (`run_pending`)
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
**File:** `crates/nxvim-tui/src/lib.rs` (`run`, `MouseCapture`)
**Severity:** High (user-visible terminal corruption).
**Status:** Implemented. Test: `crates/nxvim-tui/tests/mouse.rs`.

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
nxvim` (screen/e2e) still passes. Optionally a manual note in the PR.

---

# P1 — Trust-boundary hardening (TS worker)

## S1. TS worker: path traversal → arbitrary `dlopen` if `lang` becomes untrusted
**File:** `crates/nxvim-ts/src/loader.rs:71-88` (`parser_path`, `query_path`) and
`Grammar::load` (`:31-65`).
**Severity:** Medium (defense-in-depth; **not reachable today**).

**Problem:** `parser_path` does `dir.join(format!("{lang}.{ext}"))` and
`query_path` does `data_dir.join("queries").join(lang).join(file)` with `lang`
unvalidated. A `lang` of `../../../...` or an absolute path escapes `data_dir`,
and `libloading::Library::new` (`:37`) then executes the loaded object's
constructors = native code execution. Today `lang` only ever comes from the
server's fixed `filetype_of` table (`server lib.rs:920-949`), so it is not
attacker-reachable — but the worker is a separate trust boundary and must not
assume its caller.

**Fix:** At the top of `Grammar::load` (and/or in both path helpers), reject any
`lang` not matching a strict allowlist: non-empty and `^[a-z0-9_]+$` (after the
`-`→`_` normalization, or before — decide and be consistent with the symbol name
at `:40`). Bail with an `anyhow!` error on violation. Optionally also canonicalize
the resolved path and assert it starts with `data_dir`.

**Verify:** worker is process-isolated; add a `nxvim-ts`-level integration check
if practical, or just reason + `cargo test --workspace`.

## S2. TS worker: edit offsets from the wire aren't bounds/boundary-checked
**File:** `crates/nxvim-ts/src/engine.rs:128-133` (`edit`), with `parse_edits` at
`crates/nxvim-ts/src/lib.rs:162-185` and the `catch_unwind` at `lib.rs:75`.
**Severity:** Medium (per-buffer silent crash-loop).

**Problem:** `state.shadow.remove(e.start_byte..e.old_end_byte)` and
`insert(e.start_byte, …)` use offsets straight off the wire. An
`old_end_byte > shadow.len()`, `start_byte > len`, or a mid-codepoint offset
panics ropey. The surrounding `catch_unwind(AssertUnwindSafe(...))` swallows the
panic but leaves the shadow rope + parse tree **half-edited**, so every later
message for that buffer panics again → silent "no highlights forever" for that
buffer. Also `parse_edits` coerces any missing/non-int field to `0` via
`unwrap_or(0)` (`lib.rs:170`-ish), silently turning a garbled edit into a no-op
or an inconsistent `InputEdit` that desyncs the tree from the shadow.

**Fix:**
1. In `parse_edits`, return `Option<Edit>` per entry and **drop the whole edit
   batch (or `close` the buffer)** if any field is absent/non-integer, instead
   of coercing to `0`.
2. In `Engine::edit`, before mutating: validate each delta against
   `shadow.len()` and `is_char_boundary` for `start_byte`/`old_end_byte`. On an
   invalid edit, `self.close(buffer)` (drop the buffer so the next `open`
   rebuilds it clean) rather than mutating partially.
3. In `lib.rs:75`, on a caught panic during `ts_edit`, `engine.close(buffer)` so
   the buffer is reset rather than left poisoned.

**Verify:** feed an out-of-range / mid-codepoint edit delta and assert the worker
keeps serving other buffers (no permanent silence). Likely a `nxvim/tests/syntax.rs`
or `nxvim-ts` integration test.

## S3. `vim.fn.mkdir` ignores its perms argument
**File:** `crates/nxvim-lua/src/lib.rs:395-406`
**Severity:** Low (cheap, concrete).

**Problem:** `mkdir(path, _flags)` discards the flags/perms arg and calls
`std::fs::create_dir_all`, producing `0777 & !umask` dirs. Data/state dirs that
should be private get umask-default perms.

**Fix:** Honor the perms argument; at minimum, for the data/state stdpaths use
`std::os::unix::fs::DirBuilderExt::mode(0o700)` via `fs::DirBuilder`. Keep it
cross-platform (`#[cfg(unix)]` for the mode call).

> The broader Lua surface (`unsafe_new_with`, `debug` stdlib, C-module loading,
> unvalidated FS/env paths) is **accepted** under the "config is user-trusted"
> model — matches neovim. No change beyond S3 and the existing code comments.

---

# P2 — Reliability (worker supervision, startup, shutdown)

## R4. Startup file-open failure is silently swallowed
**File:** `crates/nxvim-server/src/lib.rs:139`
**Severity:** Medium (data-loss footgun).

**Problem:** `Editor::open(path).unwrap_or_else(|_| Editor::new())` — `nxvim
file.txt` on a permission error (or a directory, etc.) silently opens a *blank,
unnamed* buffer; a later `:w` could clobber.

**Fix:** On failure, create a buffer *named after* `path` (neovim's
new-file-buffer behavior) and/or `echo` the error so the user sees it. Do not
fall through to an unnamed buffer.

**Verify:** `editing.rs`/`buffers.rs` test: start with an unreadable path, assert
the buffer name is set (or an error is echoed) rather than `[No Name]`.

## R5. TS worker supervision — three weaknesses
**File:** `crates/nxvim-server/src/syntax.rs`
**Severity:** Medium.

1. **Breaker resets too eagerly** (`:236-238`): on hitting `MAX_CRASHES`, it
   sleeps `COOLDOWN` then `crashes.clear()`. A permanently-poison grammar
   respawns ~3×/30s forever. **Fix:** escalating backoff (double the cooldown
   each trip) or a hard cap after which syntax stays down until the next
   buffer/language change.
2. **Spawn failures have no breaker** (`:176-189`): if `spawn()` keeps failing
   (binary missing, fork limit) the loop retries every 1s forever and emits no
   event, so the server never learns syntax is unavailable. **Fix:** apply the
   same breaker/backoff to spawn failures; emit a one-time "syntax unavailable"
   event. Note `current_exe()` falling back to bare `"nxvim"` (`:70`) compounds
   this when nxvim isn't on `$PATH`.
3. **Child not reaped deterministically** (`:222`): `let _ = child.start_kill()`
   sends SIGKILL but never `await child.wait()`; reaping relies on
   `kill_on_drop` + drop timing. **Fix:** add `let _ = child.wait().await;`
   after `start_kill()`. Also prefer `continue` over `return` when stdin/stdout
   pipes are unexpectedly `None` (`:187-189`) so syntax isn't permanently
   disabled.

## R6. RPC task death leaks peer task and hangs pending requests
**File:** `crates/nxvim-rpc/src/lib.rs:103-104` (`connect`), `:119-124`
(`writer_task`), `:128-160` (`reader_task`), `:184` (pending drain).
**Severity:** Medium.

**Problem:** When one task dies (`break`/`return` on I/O error or EOF) it doesn't
signal or abort the other, and — more importantly — entries in `pending` are
never drained, so every outstanding `request().await` (`:75`) waits on a
`oneshot::Receiver` whose `Sender` lingers in the map; in-flight requests hang
until the whole `Rpc` is dropped.

**Fix:** When `reader_task` exits, drain `pending` and drop all senders so each
awaiter resolves to `Err("connection closed")`. Have the two tasks share a
shutdown signal (a `tokio::sync::Notify`, a dropped channel, or `JoinHandle`
abort) so writer death stops the reader and vice versa.

**Pairs with R1** (structural-error teardown is the trigger for this drain).

## R7. Worker event channel unbounded + redraw per notification
**File:** `crates/nxvim-server/src/syntax.rs:67` (`unbounded_channel`),
`crates/nxvim-server/src/lib.rs:667-685` (`on_syntax_event` → `redraw()`).
**Severity:** Medium.

**Problem:** A flooding/buggy worker can grow the event channel without bound and
starve the editor `select!` loop; each `ts_highlights` reply triggers a full
`redraw()` (re-projecting the whole view).

**Fix:** Bound the channel (`channel(N)` with drop-oldest/`try_send`) **or**
coalesce: set a `syntax_dirty` flag on each event and call `redraw()` once per
loop turn instead of per-notification. Also clamp `store_spans` line keys to
`line_count` (`lib.rs:776-812`) so a bogus `line: u64::MAX` reply can't seed a
junk map entry.

## R8. `reparse` discards the last good tree on a `None` result
**File:** `crates/nxvim-ts/src/engine.rs:54-60`
**Severity:** Low.

**Problem:** `parser.parse(...)` returning `None` (timeout/cancel) sets
`self.tree = None`, throwing away the previous good tree and all incremental
reuse until a full re-open.

**Fix:** `if let Some(t) = parser.parse(...) { self.tree = Some(t); }` — keep the
prior tree on `None`.

## R9. Server-thread panic looks like a clean quit
**File:** `crates/nxvim/src/main.rs:57`
**Severity:** Medium.

**Problem:** `let _ = server_thread.join()` discards the panic payload; the exit
code stays `0`, so a server crash is indistinguishable from a normal quit.

**Fix:** Inspect `join()`'s `Result`; on `Err`, print a diagnostic (downcast the
payload to `&str`/`String` if possible) and return a non-zero exit code.

## R10. `--__ts-worker` matched anywhere in argv
**File:** `crates/nxvim/src/main.rs:21`
**Severity:** Low.

**Problem:** `std::env::args().any(|a| a == TS_WORKER_FLAG)` turns the editor into
a worker if the flag appears as *any* argument (e.g. a file literally named that).

**Fix:** Check only the first argument:
`args().nth(1).as_deref() == Some(TS_WORKER_FLAG)` — matches how the worker is
actually spawned.

## R11. Zero-duration scroll animation divides by zero → NaN
**File:** `crates/nxvim-tui/src/lib.rs:522` (and `arm_animation` `:303`)
**Severity:** Low (one-frame glitch, not a panic — `as usize` saturates NaN→0).

**Fix:** Guard `if a.duration.is_zero() { t = 1.0 } else { ... }`, or skip arming
a zero-duration animation in `arm_animation`.

---

# P3 — Refactoring (maintainability; no behavior change)

These are larger and best done as separate, focused passes with the test suite
green before and after. None should change observable behavior.

## F1. `editor.rs` (3206 lines) — extract duplicated logic
**File:** `crates/nxvim-core/src/editor.rs`
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

## F2. `nxvim-server` dispatch & redraw boilerplate
**File:** `crates/nxvim-server/src/lib.rs`
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

## F3. `nxvim-tui/src/lib.rs` (1216 lines) — split into submodules
**File:** `crates/nxvim-tui/src/lib.rs`
Five independent concerns in one module: event loop/transport (`:42-142`),
`View` model + msgpack parsing (`:160-424`), scroll-animation state machine
(`:228-321`), renderer (`:485-975`), key encoding (`:1177-1216`). Split into
`view.rs`/`parse.rs`, `render.rs`, `anim.rs`, `keys.rs` and re-export the public
surface (`run`, `paint`, `View`, `encode_key`, `close_button`, `ScrollHarness`)
from `lib.rs`. Mechanical but large. Minor while in here:
- `undercurl` aliases to `Modifier::UNDERLINED` (`:1130-1141`), colliding with
  `underline` (ratatui has no undercurl). Add a clarifying comment — `HlSet`
  (`nxvim-lua/src/lib.rs:36-37`) carries them as distinct.
- ~20 repeated `map_u64(...) as u16` truncating casts (`:336-414`); add a
  `map_u16`/`map_usize` helper that documents the saturation. Add
  `map_str_array(map, key) -> Vec<String>` for the `lines`-array idiom
  duplicated at `:328-335, 377-384, 398-405`.

## F4. Wire-format duplication between server and TS worker
**Files:** `crates/nxvim-server/src/lib.rs:954` (`edits_value`),
`crates/nxvim-ts/src/lib.rs:162` (`parse_edits`), and the span tuple shape in
both. The 10-tuple edit encoding and the span tuple are hand-mirrored across two
crates with only doc-comments tying them; drift = silent wrong highlights.
**Fix:** define the wire layout once — a shared struct (serde) or a single shared
`encode`/`decode` pair in a common location (e.g. `nxvim-rpc` or a small shared
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
