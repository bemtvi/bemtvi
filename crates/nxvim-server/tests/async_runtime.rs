//! Behavior tests for nxvim's async Lua runtime (the event loop):
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
//!     inline), then after a real sleep a second barrier sees it PRESENT (the
//!     actor fired it off-tick and the server settled).

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, lua_u64, poll_true, start_attached};
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
    tokio::time::sleep(Duration::from_millis(150)).await;
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
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(lua_bool(&rpc, "return _G.ok").await, Some(true));
}

// ----- nx.utils.debounce (trailing-edge debounce over nx.timer) --------------------

#[tokio::test]
async fn debounce_collapses_a_burst_to_one_trailing_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = nx.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 40)\n\
         _G.d('a'); _G.d('b'); _G.d('c')",
    )
    .await;
    // Immediate: nothing ran inline — the burst is still pending.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
    // Past the delay: exactly one trailing call, with the MOST RECENT arguments.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("c"));
}

#[tokio::test]
async fn debounce_cancel_drops_the_pending_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.d = nx.utils.debounce(function() _G.n = _G.n + 1 end, 40)\n\
         _G.d(); _G.d:cancel()",
    )
    .await;
    // The pending call was cancelled — it never fires.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
}

#[tokio::test]
async fn debounce_flush_runs_the_pending_call_now() {
    let (rpc, _incoming) = start().await;
    // A long delay so only :flush() can make it fire promptly.
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = nx.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 5000)\n\
         _G.d('z'); _G.d:flush()",
    )
    .await;
    // flush is synchronous: the call already ran, with its captured arguments.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("z"));
    // And flushing consumed the pending timer — no second, delayed fire.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
}

// ----- Phase 3: async vim.system ---------------------------------------------

// ----- nx.run / nx.run_stream (promise-only process API) ---------------------

#[tokio::test]
async fn nx_run_resolves_with_exit_result() {
    let (rpc, _incoming) = start().await;
    // nx.run is a promise of { code, stdout, stderr }: it does NOT resolve inline
    // (the child runs off-tick), then settles with the collected output.
    exec_lua(
        &rpc,
        "_G.res = nil\n\
         nx.run({ cmd = 'sh', args = { '-c', 'echo hello' } }):next(function(r) _G.res = r end)",
    )
    .await;
    // Barrier #1: still pending (no inline resolution).
    assert_eq!(lua_bool(&rpc, "return _G.res == nil").await, Some(true));
    // Past the run: resolved with stdout + a zero exit code.
    tokio::time::sleep(Duration::from_millis(200)).await;
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
         nx.run({ cmd = 'sh', args = { '-c',\n\
           'pgid=$(ps -o pgid= -p $$ | tr -dc 0-9); [ \"$pgid\" = \"$$\" ] && echo DETACHED || echo ATTACHED' }\n\
         }):next(function(r) _G.sess = r.stdout end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.sess and _G.sess:gsub('%s+$', '')")
            .await
            .as_str(),
        Some("DETACHED"),
        "a spawned child must run in its own session (no controlling terminal)"
    );
}

#[tokio::test]
async fn nx_run_reports_spawn_failure_as_code_minus_one() {
    let (rpc, _incoming) = start().await;
    // A missing binary surfaces as code = -1 with empty output — it RESOLVES
    // (vim.system semantics), never rejects, so a `:catch` isn't needed for a
    // non-zero exit.
    exec_lua(
        &rpc,
        "_G.code = nil\n\
         nx.run({ cmd = 'definitely-not-a-real-binary-xyz' }):next(function(r) _G.code = r.code end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(lua_bool(&rpc, "return _G.code == -1").await, Some(true));
}

#[tokio::test]
async fn nx_run_stream_iterates_batches_via_await_each() {
    let (rpc, _incoming) = start().await;
    // nx.run_stream + nx.await_each inside nx.async: the for-loop sees every
    // streamed line, then ends (the stream's :next resolves nil at exit) so the
    // async function completes.
    exec_lua(
        &rpc,
        "_G.lines = {}\n\
         _G.done = false\n\
         nx.async(function()\n\
           for batch in nx.await_each(nx.run_stream({ cmd = 'sh', args = { '-c', 'echo a; echo b; echo c' } })) do\n\
             for _, l in ipairs(batch) do _G.lines[#_G.lines + 1] = l end\n\
           end\n\
           _G.done = true\n\
         end)()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
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
async fn nx_run_stream_exit_carries_stderr() {
    let (rpc, _incoming) = start().await;
    // A streaming run's exit result must carry the child's whole stderr, like
    // `nx.run`'s does (a failing `rg` source's error message is how a picker
    // reports WHY it produced nothing). The child hands its stderr to a
    // background writer that outlives the shell, so the stream (stdout) ends
    // immediately while stderr EOF lands ~200ms later — the exit must wait for
    // stderr EOF (as `nx.run`'s `wait_with_output` does), not report whatever a
    // detached collector happened to have gathered at exit time.
    exec_lua(
        &rpc,
        "_G.exit = nil\n\
         nx.async(function()\n\
           local s = nx.run_stream({ cmd = 'sh', args = { '-c',\n\
             '( sleep 0.2; echo oops >&2 ) >/dev/null & exit 3' } })\n\
           for _ in nx.await_each(s) do end\n\
           _G.exit = s._exit\n\
         end)()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
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
async fn nx_run_stream_pid_resolves_while_running_and_dies_with_the_child() {
    let (rpc, _incoming) = start().await;
    // `stream:pid()` reads the pid the event-loop actor reports asynchronously
    // after the spawn: nil at first (single-threaded runtime — it can't be known
    // inline), then the real OS pid while the child runs, then nil again once the
    // exit lands (the registry entry dies with the child, so it can't grow
    // unboundedly across spawns).
    exec_lua(
        &rpc,
        "_G.s = nx.run_stream({ cmd = 'sh', args = { '-c', 'sleep 30' } })\n\
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
async fn nx_spawn_is_removed() {
    let (rpc, _incoming) = start().await;
    // The callback-shaped nx.spawn is gone — nx is promise-only.
    assert_eq!(lua_bool(&rpc, "return nx.spawn == nil").await, Some(true));
    assert_eq!(
        lua_bool(&rpc, "return type(nx.run) == 'function'").await,
        Some(true)
    );
    assert_eq!(
        lua_bool(&rpc, "return type(nx.run_stream) == 'function'").await,
        Some(true)
    );
}

// ----- nx.socket (duplex TCP client) -----------------------------------------

#[tokio::test]
async fn nx_socket_round_trips_over_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (rpc, _incoming) = start().await;
    // A tiny echo server on an ephemeral port: nx.socket connects, writes "ping" on
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
             _G.s = nx.socket.connect({{\n\
               host = '127.0.0.1', port = {port},\n\
               on_connect = function() _G.connected = true; _G.s:write('ping') end,\n\
               on_data = function(d) _G.got = _G.got .. d end,\n\
               on_close = function(e) _G.closed = e or 'clean' end,\n\
             }})"
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        lua_bool(&rpc, "return _G.connected").await,
        Some(true),
        "on_connect fired"
    );
    assert_eq!(exec_lua(&rpc, "return _G.got").await.as_str(), Some("ping"));
    // Closing fires on_close (clean — no error).
    exec_lua(&rpc, "_G.s:close()").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.closed").await.as_str(),
        Some("clean"),
        "on_close fired with no error after a requested close"
    );
}

#[tokio::test]
async fn nx_socket_connect_failure_is_loud() {
    let (rpc, _incoming) = start().await;
    // Connecting to a closed port fails LOUD: on_close fires with an error string,
    // never a silent hang. Port 1 is privileged + unbound — connect refuses fast.
    exec_lua(
        &rpc,
        "_G.connected = false\n\
         _G.err = nil\n\
         nx.socket.connect({\n\
           host = '127.0.0.1', port = 1,\n\
           on_connect = function() _G.connected = true end,\n\
           on_close = function(e) _G.err = e end,\n\
         })",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lua_bool(&rpc, "return _G.connected").await, Some(false));
    assert_eq!(
        lua_bool(&rpc, "return type(_G.err) == 'string' and #_G.err > 0").await,
        Some(true),
        "the connect failure surfaced as an on_close error"
    );
}

// ----- nx.process (duplex, bidirectional child) ------------------------------

#[tokio::test]
async fn nx_process_round_trips_stdin_to_stdout() {
    let (rpc, _incoming) = start().await;
    // `cat` echoes its stdin to stdout: open it duplex, write two lines on the
    // still-open stdin, and the persistent on_stdout must receive them back — proof
    // the channel is bidirectional and long-lived (neither nx.run nor nx.run_stream
    // can do this, both close stdin at spawn).
    exec_lua(
        &rpc,
        "_G.got = ''\n\
         _G.code = nil\n\
         _G.proc = nx.process.open({\n\
           cmd = 'cat',\n\
           on_stdout = function(chunk) _G.got = _G.got .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })\n\
         _G.proc:write('hello\\n')\n\
         _G.proc:write('world\\n')",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
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
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        lua_bool(&rpc, "return _G.code ~= nil").await,
        Some(true),
        "on_exit fired after the kill"
    );
}

#[tokio::test]
async fn nx_process_streams_stderr_and_natural_exit_code() {
    let (rpc, _incoming) = start().await;
    // A child that writes stderr then exits non-zero: on_stderr collects the bytes,
    // on_exit reports the real code.
    exec_lua(
        &rpc,
        "_G.err = ''\n\
         _G.code = nil\n\
         nx.process.open({\n\
           cmd = 'sh',\n\
           args = { '-c', 'printf oops 1>&2; exit 3' },\n\
           on_stderr = function(chunk) _G.err = _G.err .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(exec_lua(&rpc, "return _G.err").await.as_str(), Some("oops"));
    assert_eq!(lua_u64(&rpc, "return _G.code").await, Some(3));
}

#[tokio::test]
async fn nx_process_spawn_failure_is_loud_not_silent() {
    let (rpc, _incoming) = start().await;
    // A missing binary fails LOUD: stderr carries the cause and on_exit fires with
    // code -1 (never a silent hang) — the no-silent-stubs discipline.
    exec_lua(
        &rpc,
        "_G.err = ''\n\
         _G.code = nil\n\
         nx.process.open({\n\
           cmd = 'this-binary-does-not-exist-xyz',\n\
           on_stderr = function(chunk) _G.err = _G.err .. chunk end,\n\
           on_exit = function(c) _G.code = c end,\n\
         })",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
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
    // A long run of one-shot schedules must leave nx._cb_fns empty (each is
    // dropped after firing).
    exec_lua(&rpc, "for _ = 1, 64 do vim.schedule(function() end) end").await;
    let remaining = lua_u64(
        &rpc,
        "local n = 0; for _ in pairs(nx._cb_fns) do n = n + 1 end; return n",
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
async fn nx_hash_new_hashes_a_stream_via_await_each() {
    let (rpc, _incoming) = start().await;
    // The streaming-data use case: feed a subprocess's stdout into an incremental
    // hasher chunk by chunk with nx.await_each, never buffering the whole output.
    // `printf abc` emits no newline, so the single batch carries one line "abc" — the
    // digest must equal sha256("abc") (proving the streamed feed reconstructs the data).
    exec_lua(
        &rpc,
        "_G.sum = nil\n\
         nx.async(function()\n\
           local h = nx.hash.new('sha256')\n\
           for batch in nx.await_each(nx.run_stream({ cmd = 'sh', args = { '-c', 'printf abc' } })) do\n\
             for _, line in ipairs(batch) do h:update(line) end\n\
           end\n\
           _G.sum = h:hexdigest()\n\
         end)()",
    )
    .await;
    // Off-tick stream: poll until the async chain sets _G.sum.
    let mut got = None;
    for _ in 0..150 {
        let v = exec_lua(&rpc, "return _G.sum").await;
        if let Some(s) = v.as_str() {
            got = Some(s.to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        got.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "hashing a stream's chunks via await_each yields sha256 of the concatenated data"
    );
}
