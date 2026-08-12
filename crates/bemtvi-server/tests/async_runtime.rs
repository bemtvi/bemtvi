//! Behavior tests for bemtvi's async Lua runtime (the event loop):
//! `vim.schedule`, `vim.defer_fn`, and async `vim.system`. Black-box per the
//! project conventions — a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state.
//!
//! Two observation patterns (see docs/plans/2026-06-06-async-lua-runtime.md → testing
//! appendix):
//!   * deferred-within-a-tick (`vim.schedule`) — assert on *ordering*, since the
//!     effect lands at convergence in the same handler;
//!   * off-tick (timers / async `vim.system`) — the *two-barrier* pattern: a
//!     barrier right after the trigger sees the effect ABSENT (it didn't run
//!     inline), then a second barrier sees it PRESENT (the actor fired it off-tick
//!     and the server settled).
//!
//! How to wait for that second barrier depends on which way the assertion points,
//! and getting it backwards is how these tests go flaky or go vacuous:
//!
//!   * **Waiting for something to HAPPEN** — `poll_true` on the state itself. A
//!     fixed sleep has to be long enough for the slowest machine or it flakes, and
//!     every run then pays that worst case. Spawning a process is the common case
//!     here, and it is the one that actually flaked: a 250ms budget for `sh` to
//!     start, write and exit is fine idle and not fine under a loaded
//!     `cargo test --workspace`.
//!   * **Proving something will NOT happen** — `settle_ms`, a real wait. There is
//!     nothing to poll for: the assertion is that the window passed with the state
//!     unchanged, so the window has to genuinely pass.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, lua_bool, lua_u64, poll_true, settle_ms, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread (its runtime has timers enabled, so the
/// event-loop actor can sleep) and return a connected, UI-attached client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

// ----- Phase 1: vim.schedule (deferred within the tick) ----------------------

#[tokio::test]
async fn schedule_runs_after_direct_work_not_inline() {
    let (rpc, _incoming) = start().await;
    // The scheduled callback must run AFTER the direct work that follows it in the
    // chunk — proof it deferred to convergence rather than running nested inline.
    exec_lua(
        &rpc,
        "_G.order = {}\n\
         vim.schedule(function() table.insert(_G.order, 'scheduled') end)\n\
         table.insert(_G.order, 'direct')",
    )
    .await;
    let order = exec_lua(&rpc, "return table.concat(_G.order, ',')").await;
    assert_eq!(order.as_str(), Some("direct,scheduled"));
}

#[tokio::test]
async fn a_scheduled_callback_can_schedule_more_work() {
    let (rpc, _incoming) = start().await;
    // A callback that schedules another must see both run — proof the fixpoint
    // picks up work queued mid-drain.
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         vim.schedule(function()\n\
           vim.schedule(function() _G.n = _G.n + 1 end)\n\
           _G.n = _G.n + 1\n\
         end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(2));
}

#[tokio::test]
async fn a_throwing_scheduled_callback_does_not_stop_the_next() {
    let (rpc, _incoming) = start().await;
    // The first scheduled callback throws; the second must still run (error
    // isolation — the drain catches and echoes E5108, never aborts).
    exec_lua(
        &rpc,
        "_G.ran = false\n\
         vim.schedule(function() error('boom') end)\n\
         vim.schedule(function() _G.ran = true end)",
    )
    .await;
    assert_eq!(lua_bool(&rpc, "return _G.ran").await, Some(true));
}

// ----- Phase 2: timers (off the input tick) ----------------------------------

#[tokio::test]
async fn defer_fn_fires_after_the_delay_not_before() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.fired = false\n\
         vim.defer_fn(function() _G.fired = true end, 40)",
    )
    .await;
    // Barrier #1, immediate: the deferred fn has NOT run (it didn't run inline).
    assert_eq!(lua_bool(&rpc, "return _G.fired").await, Some(false));
    // Past the delay: the actor fired it off-tick and the server settled.
    poll_true(&rpc, "return _G.fired").await;
    assert_eq!(lua_bool(&rpc, "return _G.fired").await, Some(true));
}

#[tokio::test]
async fn a_throwing_timer_callback_does_not_wedge_the_loop() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.ok = false\n\
         vim.defer_fn(function() error('boom') end, 20)\n\
         vim.defer_fn(function() _G.ok = true end, 50)",
    )
    .await;
    poll_true(&rpc, "return _G.ok").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.ok").await,
        Some(true),
        "the second timer ran even though the first threw"
    );
}

// ----- btv.utils.debounce (trailing-edge debounce over btv.timer) --------------------

#[tokio::test]
async fn debounce_collapses_a_burst_to_one_trailing_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = btv.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 40)\n\
         _G.d('a'); _G.d('b'); _G.d('c')",
    )
    .await;
    // Immediate: nothing ran inline — the burst is still pending.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
    // Past the delay: exactly one trailing call, with the MOST RECENT arguments.
    // Both halves need their own wait — poll for the call to arrive, then let a real
    // window pass, because "exactly one" is only proved by a SECOND one not showing up.
    poll_true(&rpc, "return _G.n >= 1").await;
    settle_ms(&rpc, 120).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("c"));
}

#[tokio::test]
async fn debounce_cancel_drops_the_pending_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.d = btv.utils.debounce(function() _G.n = _G.n + 1 end, 40)\n\
         _G.d(); _G.d:cancel()",
    )
    .await;
    // The pending call was cancelled — it never fires. Nothing to poll for: the whole
    // claim is that the delay elapses with the counter untouched, so it must elapse.
    settle_ms(&rpc, 150).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
}

#[tokio::test]
async fn debounce_flush_runs_the_pending_call_now() {
    let (rpc, _incoming) = start().await;
    // The delay has to sit in a window: far longer than an RPC round-trip, so the timer
    // provably cannot have fired on its own before the first assertion (that is what
    // makes it a test of `flush` rather than of elapsed time — the delay used to be
    // 5000ms for this reason); and short enough that the settle below OUTLIVES it, so a
    // leftover timer really does get a chance to fire. Against the old 5000ms delay the
    // second assertion could not fail no matter how broken `flush` was: 120ms of waiting
    // can observe nothing about a call scheduled 5 seconds out.
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = btv.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 150)\n\
         _G.d('z'); _G.d:flush()",
    )
    .await;
    // flush is synchronous: the call already ran, with its captured arguments.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("z"));
    // And flushing consumed the pending timer — no second, delayed fire.
    settle_ms(&rpc, 320).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
}

// ----- Phase 3: async vim.system ---------------------------------------------

// ----- btv.run / btv.run_stream (promise-only process API) ---------------------

#[tokio::test]
async fn btv_run_resolves_with_exit_result() {
    let (rpc, _incoming) = start().await;
    // btv.run is a promise of { code, stdout, stderr }: it does NOT resolve inline
    // (the child runs off-tick), then settles with the collected output.
    exec_lua(
        &rpc,
        "_G.res = nil\n\
         btv.run({ cmd = 'sh', args = { '-c', 'echo hello' } }):next(function(r) _G.res = r end)",
    )
    .await;
    // Barrier #1: still pending (no inline resolution).
    assert_eq!(lua_bool(&rpc, "return _G.res == nil").await, Some(true));
    // Past the run: resolved with stdout + a zero exit code.
    poll_true(&rpc, "return _G.res ~= nil").await;
    assert_eq!(
        exec_lua(&rpc, "return _G.res.stdout").await.as_str(),
        Some("hello\n")
    );
    assert_eq!(lua_u64(&rpc, "return _G.res.code").await, Some(0));
}

// A spawned child must not share the editor's controlling terminal: an
// interactive tool (git/ssh asking for a password) would otherwise open /dev/tty
// directly — bypassing the stdout/stderr pipes — and scribble its prompt over the
// TUI while blocking on a read that never comes. The spawn seam puts every child
// in its own session (setsid), which makes it the leader of a fresh session *and*
// process group with no controlling terminal — so a session leader's process-group
// id equals its own pid. (We check pgid rather than sid because BSD/macOS `ps` has
// no `sid` keyword; `pgid` is portable, and setsid sets both equal to the pid.) We
// assert the child reports itself DETACHED.
#[cfg(unix)]
#[tokio::test]
async fn spawned_child_is_detached_from_the_controlling_terminal() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.sess = nil\n\
         btv.run({ cmd = 'sh', args = { '-c',\n\
           'pgid=$(ps -o pgid= -p $$ | tr -dc 0-9); [ \"$pgid\" = \"$$\" ] && echo DETACHED || echo ATTACHED' }\n\
         }):next(function(r) _G.sess = r.stdout end)",
    )
    .await;
    poll_true(&rpc, "return _G.sess ~= nil").await;
    assert_eq!(
        exec_lua(&rpc, "return _G.sess and _G.sess:gsub('%s+$', '')")
            .await
            .as_str(),
        Some("DETACHED"),
        "a spawned child must run in its own session (no controlling terminal)"
    );
}

#[tokio::test]
async fn btv_run_reports_spawn_failure_as_code_minus_one() {
    let (rpc, _incoming) = start().await;
    // A missing binary surfaces as code = -1 with empty output — it RESOLVES
    // (vim.system semantics), never rejects, so a `:catch` isn't needed for a
    // non-zero exit.
    exec_lua(
        &rpc,
        "_G.code = nil\n\
         btv.run({ cmd = 'definitely-not-a-real-binary-xyz' }):next(function(r) _G.code = r.code end)",
    )
    .await;
    poll_true(&rpc, "return _G.code ~= nil").await;
    assert_eq!(lua_bool(&rpc, "return _G.code == -1").await, Some(true));
}

#[tokio::test]
async fn btv_run_stream_iterates_batches_via_await_each() {
    let (rpc, _incoming) = start().await;
    // btv.run_stream + btv.await_each inside btv.async: the for-loop sees every
    // streamed line, then ends (the stream's :next resolves nil at exit) so the
    // async function completes.
    exec_lua(
        &rpc,
        "_G.lines = {}\n\
         _G.done = false\n\
         btv.async(function()\n\
           for batch in btv.await_each(btv.run_stream({ cmd = 'sh', args = { '-c', 'echo a; echo b; echo c' } })) do\n\
             for _, l in ipairs(batch) do _G.lines[#_G.lines + 1] = l end\n\
           end\n\
           _G.done = true\n\
         end)()",
    )
    .await;
    poll_true(&rpc, "return _G.done").await;
    // The collected lines, sorted to be order-independent of stdout batching.
    let joined = exec_lua(
        &rpc,
        "local t = {}; for _, l in ipairs(_G.lines) do if l ~= '' then t[#t+1] = l end end; table.sort(t); return table.concat(t, ',')",
    )
    .await;
    assert_eq!(joined.as_str(), Some("a,b,c"));
    assert_eq!(
        lua_bool(&rpc, "return _G.done").await,
        Some(true),
        "the async iterator terminated at end-of-stream"
    );
}

#[tokio::test]
async fn btv_run_stream_exit_carries_stderr() {
    let (rpc, _incoming) = start().await;
    // A streaming run's exit result must carry the child's whole stderr, like
    // `btv.run`'s does (a failing `rg` source's error message is how a picker
    // reports WHY it produced nothing). The child hands its stderr to a
    // background writer that outlives the shell, so the stream (stdout) ends
    // immediately while stderr EOF lands ~200ms later — the exit must wait for
    // stderr EOF (as `btv.run`'s `wait_with_output` does), not report whatever a
    // detached collector happened to have gathered at exit time.
    exec_lua(
        &rpc,
        "_G.exit = nil\n\
         btv.async(function()\n\
           local s = btv.run_stream({ cmd = 'sh', args = { '-c',\n\
             '( sleep 0.2; echo oops >&2 ) >/dev/null & exit 3' } })\n\
           for _ in btv.await_each(s) do end\n\
           _G.exit = s:exit()\n\
         end)()",
    )
    .await;
    // Poll on the EXIT landing, which is exactly the claim: whenever it lands, the
    // stderr assertion below must already hold. A detached collector reporting early
    // fails here just as it did against the fixed 700ms wait, and without budgeting
    // the slowest machine into every run.
    poll_true(&rpc, "return _G.exit ~= nil").await;
    assert_eq!(
        lua_u64(&rpc, "return _G.exit and _G.exit.code").await,
        Some(3)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.exit and _G.exit.stderr")
            .await
            .as_str(),
        Some("oops\n"),
        "the streaming exit result must carry the child's stderr"
    );
}

#[tokio::test]
async fn stream_exit_separates_a_missing_binary_from_a_nonzero_exit() {
    let (rpc, _incoming) = start().await;
    // `stream:exit()` is the signal a fallback chain branches on, so the two ways of
    // producing NO output must stay distinguishable: a binary that isn't there
    // (`code = -1`) versus one that ran and answered "nothing" (`grep`'s `1`). It is
    // nil while the child is still running — it only means something after the
    // `btv.await_each` loop ends.
    exec_lua(
        &rpc,
        "_G.missing, _G.nomatch, _G.inflight = nil, nil, nil\n\
         btv.async(function()\n\
           local a = btv.run_stream({ cmd = 'definitely-not-a-real-binary-xyz' })\n\
           _G.inflight = a:exit()\n\
           for _ in btv.await_each(a) do end\n\
           _G.missing = a:exit().code\n\
           local b = btv.run_stream({ cmd = 'sh', args = { '-c', 'exit 1' } })\n\
           for _ in btv.await_each(b) do end\n\
           _G.nomatch = b:exit().code\n\
         end)()",
    )
    .await;
    poll_true(&rpc, "return _G.nomatch ~= nil").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.inflight == nil").await,
        Some(true),
        "exit() is nil until the child has actually exited"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.missing == -1").await,
        Some(true),
        "a binary that isn't there reports code -1"
    );
    assert_eq!(
        lua_u64(&rpc, "return _G.nomatch").await,
        Some(1),
        "a tool that ran and found nothing reports its own status, not -1"
    );
}

#[tokio::test]
async fn btv_run_stream_pid_resolves_while_running_and_dies_with_the_child() {
    let (rpc, _incoming) = start().await;
    // `stream:pid()` reads the pid the event-loop actor reports asynchronously
    // after the spawn: nil at first (single-threaded runtime — it can't be known
    // inline), then the real OS pid while the child runs, then nil again once the
    // exit lands (the registry entry dies with the child, so it can't grow
    // unboundedly across spawns).
    exec_lua(
        &rpc,
        "_G.s = btv.run_stream({ cmd = 'sh', args = { '-c', 'sleep 30' } })\n\
         _G.pid_inline = _G.s:pid()",
    )
    .await;
    assert_eq!(
        lua_bool(&rpc, "return _G.pid_inline == nil").await,
        Some(true),
        "the pid cannot be known synchronously at spawn"
    );
    assert!(
        poll_true(
            &rpc,
            "return type(_G.s:pid()) == 'number' and _G.s:pid() > 0"
        )
        .await,
        "the actor's spawn report must land a real pid"
    );
    exec_lua(&rpc, "_G.s:kill()").await;
    assert!(
        poll_true(&rpc, "return _G.s:pid() == nil").await,
        "the registry entry must be cleared when the child exits"
    );
}

#[tokio::test]
async fn btv_spawn_is_removed() {
    let (rpc, _incoming) = start().await;
    // The callback-shaped btv.spawn is gone — btv is promise-only.
    assert_eq!(lua_bool(&rpc, "return btv.spawn == nil").await, Some(true));
    assert_eq!(
        lua_bool(&rpc, "return type(btv.run) == 'function'").await,
        Some(true)
    );
    assert_eq!(
        lua_bool(&rpc, "return type(btv.run_stream) == 'function'").await,
        Some(true)
    );
}

// ----- btv.socket (duplex TCP client) -----------------------------------------

#[tokio::test]
async fn btv_socket_round_trips_over_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (rpc, _incoming) = start().await;
    // A tiny echo server on an ephemeral port: btv.socket connects, writes "ping" on
    // connect, and the bytes come back on on_data — proof the TCP transport is duplex.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    exec_lua(
        &rpc,
        &format!(
            "_G.got = ''\n\
             _G.connected = false\n\
             _G.closed = nil\n\
             _G.s = btv.socket.connect({{\n\
               host = '127.0.0.1', port = {port},\n\
               on_connect = function() _G.connected = true; _G.s:write('ping') end,\n\
               on_data = function(d) _G.got = _G.got .. d end,\n\
               on_close = function(e) _G.closed = e or 'clean' end,\n\
             }})"
        ),
    )
    .await;
    poll_true(&rpc, "return _G.got ~= ''").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.connected").await,
        Some(true),
        "on_connect fired"
    );
    assert_eq!(exec_lua(&rpc, "return _G.got").await.as_str(), Some("ping"));
    // Closing fires on_close (clean — no error).
    exec_lua(&rpc, "_G.s:close()").await;
    poll_true(&rpc, "return _G.closed ~= nil").await;
    assert_eq!(
        exec_lua(&rpc, "return _G.closed").await.as_str(),
        Some("clean"),
        "on_close fired with no error after a requested close"
    );
}

#[tokio::test]
async fn btv_socket_connect_failure_is_loud() {
    let (rpc, _incoming) = start().await;
    // Connecting to a closed port fails LOUD: on_close fires with an error string,
    // never a silent hang. Port 1 is privileged + unbound — connect refuses fast.
    exec_lua(
        &rpc,
        "_G.connected = false\n\
         _G.err = nil\n\
         btv.socket.connect({\n\
           host = '127.0.0.1', port = 1,\n\
           on_connect = function() _G.connected = true end,\n\
           on_close = function(e) _G.err = e end,\n\
         })",
    )
    .await;
    // Poll for the failure report. A connect that wrongly SUCCEEDS sets no error, so
    // this times out — and the `connected` assertion below is what then fails, naming
    // the real defect rather than a timeout.
    poll_true(&rpc, "return _G.err ~= nil").await;
    assert_eq!(lua_bool(&rpc, "return _G.connected").await, Some(false));
    assert_eq!(
        lua_bool(&rpc, "return type(_G.err) == 'string' and #_G.err > 0").await,
        Some(true),
        "the connect failure surfaced as an on_close error"
    );
}

// ----- btv.process (duplex, bidirectional child) ------------------------------

#[tokio::test]
async fn btv_process_round_trips_stdin_to_stdout() {
    let (rpc, _incoming) = start().await;
    // `cat` echoes its stdin to stdout: open it duplex, write two lines on the
    // still-open stdin, and the persistent on_stdout must receive them back — proof
    // the channel is bidirectional and long-lived (neither btv.run nor btv.run_stream
    // can do this, both close stdin at spawn).
    exec_lua(
        &rpc,
        "_G.got = ''\n\
         _G.code = nil\n\
         _G.proc = btv.process.open({\n\
           cmd = 'cat',\n\
           on_stdout = function(chunk) _G.got = _G.got .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })\n\
         _G.proc:write('hello\\n')\n\
         _G.proc:write('world\\n')",
    )
    .await;
    poll_true(&rpc, "return _G.got == 'hello\\nworld\\n'").await;
    let got = exec_lua(&rpc, "return _G.got").await;
    assert_eq!(
        got.as_str(),
        Some("hello\nworld\n"),
        "the child echoed both writes back over the live pipe"
    );
    // Still running (cat waits for EOF) — no exit yet.
    assert_eq!(lua_bool(&rpc, "return _G.code == nil").await, Some(true));
    // Kill it; the exit callback fires.
    exec_lua(&rpc, "_G.proc:kill()").await;
    poll_true(&rpc, "return _G.code ~= nil").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.code ~= nil").await,
        Some(true),
        "on_exit fired after the kill"
    );
}

#[tokio::test]
async fn btv_process_streams_stderr_and_natural_exit_code() {
    let (rpc, _incoming) = start().await;
    // A child that writes stderr then exits non-zero: on_stderr collects the bytes,
    // on_exit reports the real code.
    exec_lua(
        &rpc,
        "_G.err = ''\n\
         _G.code = nil\n\
         btv.process.open({\n\
           cmd = 'sh',\n\
           args = { '-c', 'printf oops 1>&2; exit 3' },\n\
           on_stderr = function(chunk) _G.err = _G.err .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    // Poll rather than sleep a fixed span: this waits on a *spawned process*, and 250ms
    // is not enough for `sh` to start, write and exit when the machine is loaded (it
    // flaked in a full `cargo test --workspace` run while passing in isolation). The
    // assertions below still do the verifying — polling only decides when to look.
    poll_true(&rpc, "return _G.code ~= nil and _G.err ~= ''").await;
    assert_eq!(exec_lua(&rpc, "return _G.err").await.as_str(), Some("oops"));
    assert_eq!(lua_u64(&rpc, "return _G.code").await, Some(3));
}

/// A child's stderr is delivered **before** its `on_exit` — the exit is the last event,
/// not a cutoff that strands whatever the stderr pump had not drained yet.
///
/// `btv._proc_exit` forgets the handler entry and `btv._proc_recv` returns silently for an
/// unknown id, so a chunk that arrives after the exit is dropped on the floor: the client
/// sees a child that failed with no diagnostic at all. That is the failure mode this
/// guards — a debug adapter (bemtvi-dap runs on `btv.process`) dying with its reason
/// swallowed.
///
/// The ordering is a scheduler race for an ordinary `printf`-and-exit child (it lost
/// stderr in ~15% of *loaded* runs and none of the idle ones), which is no basis for a
/// regression guard. So the window is made structural instead: the child forks a
/// grandchild that inherits the stderr pipe (but *not* stdout — held open, that pipe
/// would delay the exit on its own and hide the bug), exits immediately, and only then
/// does the grandchild write. The exit status is available at once while the bytes do not
/// exist yet — so nothing but waiting for the stream itself can deliver them, on any
/// machine at any load.
#[tokio::test]
async fn btv_process_delivers_stderr_written_after_the_child_exits() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.err = ''\n\
         _G.code = nil\n\
         _G.after_exit = 0\n\
         btv.process.open({\n\
           cmd = 'sh',\n\
           args = { '-c', '(sleep 0.3; printf oops 1>&2) >/dev/null & exit 3' },\n\
           on_stderr = function(chunk)\n\
             _G.err = _G.err .. chunk\n\
             if _G.code ~= nil then _G.after_exit = _G.after_exit + #chunk end\n\
           end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    poll_true(&rpc, "return _G.code ~= nil").await;
    // A straggler would land here, so "arrived late" reads differently from "lost".
    settle_ms(&rpc, 400).await;

    assert_eq!(
        lua_u64(&rpc, "return _G.code").await,
        Some(3),
        "the child's real exit code still reaches on_exit"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.err").await.as_str(),
        Some("oops"),
        "stderr written while the child was already gone still reaches on_stderr"
    );
    assert_eq!(
        lua_u64(&rpc, "return _G.after_exit").await,
        Some(0),
        "and it arrives BEFORE on_exit — the exit stays the last event"
    );
}

/// `:kill()` on a child that never closes its own pipes still fires `on_exit` promptly —
/// the exit is not held behind the stderr drain.
///
/// The natural-exit path waits for that stream (above), but a killed child is only
/// terminated by `kill_on_drop` once its task returns, so one that ignores stdin EOF is
/// still holding the pipe open at that moment: waiting would trade a lost chunk for a
/// hang, and a kill is owed a prompt exit rather than trailing output. `sleep` is the
/// child precisely because it ignores stdin (`cat`, which the duplex test kills, exits on
/// EOF by itself and so would pass either way).
#[tokio::test]
async fn btv_process_kill_of_a_child_that_ignores_stdin_exits_promptly() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.code = nil\n\
         _G.proc = btv.process.open({\n\
           cmd = 'sleep',\n\
           args = { '30' },\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    exec_lua(&rpc, "_G.proc:kill()").await;

    // Well under the 2s stderr-drain bound, so a regression that waits on it fails here
    // rather than passing slowly.
    let started = std::time::Instant::now();
    poll_true(&rpc, "return _G.code ~= nil").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.code ~= nil").await,
        Some(true),
        "on_exit fired after the kill"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "the kill's on_exit must not wait on the stderr drain (took {:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn btv_process_spawn_failure_is_loud_not_silent() {
    let (rpc, _incoming) = start().await;
    // A missing binary fails LOUD: stderr carries the cause and on_exit fires with
    // code -1 (never a silent hang) — the no-silent-stubs discipline.
    exec_lua(
        &rpc,
        "_G.err = ''\n\
         _G.code = nil\n\
         btv.process.open({\n\
           cmd = 'this-binary-does-not-exist-xyz',\n\
           on_stderr = function(chunk) _G.err = _G.err .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    poll_true(&rpc, "return _G.code ~= nil and #_G.err > 0").await;
    assert_eq!(
        lua_bool(&rpc, "return _G.code == -1").await,
        Some(true),
        "spawn failure surfaces as code -1"
    );
    assert_eq!(
        lua_bool(&rpc, "return #_G.err > 0").await,
        Some(true),
        "the failure reason rode the stderr stream"
    );
}

// ----- Phase 4: robustness (leaks, schedule_wrap) ----------------------------

#[tokio::test]
async fn one_shot_callbacks_do_not_leak_the_registry() {
    let (rpc, _incoming) = start().await;
    // A long run of one-shot schedules must leave btv._cb_fns empty (each is
    // dropped after firing).
    exec_lua(&rpc, "for _ = 1, 64 do vim.schedule(function() end) end").await;
    let remaining = lua_u64(
        &rpc,
        "local n = 0; for _ in pairs(btv._cb_fns) do n = n + 1 end; return n",
    )
    .await;
    assert_eq!(remaining, Some(0), "scheduled one-shots must not leak");
}

#[tokio::test]
async fn schedule_wrap_defers_its_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         local w = vim.schedule_wrap(function(x) _G.got = x end)\n\
         w(42)\n\
         _G.during = _G.got",
    )
    .await;
    // During the chunk the wrapped call had not run yet (it scheduled); by the
    // time we read back it has (at convergence).
    assert_eq!(lua_bool(&rpc, "return _G.during == nil").await, Some(true));
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(42));
}

#[tokio::test]
async fn btv_hash_new_hashes_a_stream_via_await_each() {
    let (rpc, _incoming) = start().await;
    // The streaming-data use case: feed a subprocess's stdout into an incremental
    // hasher chunk by chunk with btv.await_each, never buffering the whole output.
    // `printf abc` emits no newline, so the single batch carries one line "abc" — the
    // digest must equal sha256("abc") (proving the streamed feed reconstructs the data).
    exec_lua(
        &rpc,
        "_G.sum = nil\n\
         btv.async(function()\n\
           local h = btv.hash.new('sha256')\n\
           for batch in btv.await_each(btv.run_stream({ cmd = 'sh', args = { '-c', 'printf abc' } })) do\n\
             for _, line in ipairs(batch) do h:update(line) end\n\
           end\n\
           _G.sum = h:hexdigest()\n\
         end)()",
    )
    .await;
    // Off-tick stream: poll until the async chain sets _G.sum (the hand-rolled loop
    // this replaces was `poll_true` spelled out).
    poll_true(&rpc, "return _G.sum ~= nil").await;
    let got = exec_lua(&rpc, "return _G.sum").await;
    assert_eq!(
        got.as_str(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "hashing a stream's chunks via await_each yields sha256 of the concatenated data"
    );
}
