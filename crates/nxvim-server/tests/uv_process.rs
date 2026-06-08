//! The `vim.uv` / `vim.loop` libuv **process** surface — `uv.spawn`, `uv.new_pipe`
//! (with `:read_start`/`:write`/`:close`), `uv.new_check`, and the handle
//! lifecycle (`is_closing`/`is_active`/`close`). This is what `plenary.job` (and
//! `plenary.curl`, built on it) binds directly to run subprocesses, separate from
//! the batch `vim.system` API.
//!
//! Server-level black-box, because a spawned process completes off the input tick
//! (in the event-loop actor) and its output + exit are delivered back through the
//! same convergence machinery `vim.system` / timers use. The two-barrier shape:
//! kick off the spawn, see it pending, then after the child exits see the result
//! settled.

use std::path::PathBuf;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a headless server with an 80×24 UI attached.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Poll `_G.done` until the spawned child has exited and settled, or time out.
async fn await_done(rpc: &Rpc) -> bool {
    for _ in 0..100 {
        if lua_bool(rpc, "return _G.done == true").await == Some(true) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn uv_spawn_streams_stdout_to_a_pipe_and_fires_on_exit() {
    let (rpc, _incoming) = start().await;
    // Raw libuv shape: new_pipe for stdout, spawn wired to it, read_start to
    // collect, on_exit to record the code. `printf` is a tiny, portable child.
    exec_lua(
        &rpc,
        r#"
_G.done = false
_G.out = ""
_G.code = nil
local uv = vim.uv
local stdout = uv.new_pipe(false)
local stderr = uv.new_pipe(false)
local handle, pid = uv.spawn("printf", {
  args = { "hello world" },
  stdio = { nil, stdout, stderr },
}, function(code, signal)
  _G.code = code
  _G.done = true
end)
assert(handle, "spawn should return a handle")
stdout:read_start(function(err, data)
  assert(not err, tostring(err))
  if data then _G.out = _G.out .. data end
end)
"#,
    )
    .await;
    // Immediately it has not finished (the child runs off-tick).
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(false));
    // After it exits and settles: stdout collected, exit code 0.
    assert!(await_done(&rpc).await, "child should exit and settle");
    assert_eq!(
        exec_lua(&rpc, "return _G.out").await.as_str(),
        Some("hello world")
    );
    assert_eq!(exec_lua(&rpc, "return _G.code").await.as_i64(), Some(0));
}

#[tokio::test]
async fn uv_spawn_feeds_stdin_through_a_write_pipe() {
    let (rpc, _incoming) = start().await;
    // stdin pipe written before the child runs; `cat` echoes it back on stdout.
    exec_lua(
        &rpc,
        r#"
_G.done = false
_G.out = ""
local uv = vim.uv
local stdin = uv.new_pipe(false)
local stdout = uv.new_pipe(false)
local stderr = uv.new_pipe(false)
local handle = uv.spawn("cat", {
  stdio = { stdin, stdout, stderr },
}, function(code) _G.done = true end)
assert(handle, "spawn should return a handle")
stdout:read_start(function(err, data)
  if data then _G.out = _G.out .. data end
end)
stdin:write("piped input\n")
stdin:close()
"#,
    )
    .await;
    assert!(await_done(&rpc).await, "cat should exit after stdin closes");
    assert_eq!(
        exec_lua(&rpc, "return _G.out").await.as_str(),
        Some("piped input\n")
    );
}

/// The real `plenary.job`, if installed, run end-to-end: spawn a command,
/// collect its stdout, and read the result back through `:sync()`-style result
/// accumulation. Skips when plenary isn't present (like the uv_fs real-plenary
/// tests).
#[tokio::test]
async fn real_plenary_job_collects_stdout() {
    let plenary = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/nvim/lazy/plenary.nvim");
    if !plenary.join("lua/plenary/job.lua").is_file() {
        eprintln!("skipping: plenary.nvim not installed");
        return;
    }
    let rtp = plenary.to_string_lossy().replace('\\', "/");
    let (rpc, _incoming) = start().await;
    // Put plenary on package.path, then run a Job that prints two lines and
    // records the collected stdout table on exit.
    exec_lua(
        &rpc,
        &format!(
            r#"
package.path = package.path .. ";{rtp}/lua/?.lua;{rtp}/lua/?/init.lua"
_G.done = false
_G.result = nil
local Job = require("plenary.job")
Job:new({{
  command = "printf",
  args = {{ "one\ntwo\n" }},
  on_exit = function(j, code)
    _G.result = j:result()
    _G.code = code
    _G.done = true
  end,
}}):start()
"#
        ),
    )
    .await;
    assert!(await_done(&rpc).await, "plenary Job should exit and settle");
    assert_eq!(exec_lua(&rpc, "return _G.code").await.as_i64(), Some(0));
    assert_eq!(
        exec_lua(&rpc, "return table.concat(_G.result, ',')")
            .await
            .as_str(),
        Some("one,two")
    );
}
