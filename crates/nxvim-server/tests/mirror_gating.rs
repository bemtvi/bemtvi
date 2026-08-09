//! Black-box tests for the **gated** per-tick Rust→Lua mirrors: the quickfix /
//! location lists and the register file. Both used to be rebuilt in full on every
//! tick — i.e. on every keystroke — so a `:vimgrep` with thousands of hits, or a
//! yanked file, made typing cost O(list) / O(register bytes). See
//! `docs/plans/2026-08-08-per-keystroke-costs-round-2.md`.
//!
//! Two things are checked, and a gate needs both:
//!
//!  * **The gate opens** — every door that mutates the state must land in the
//!    mirror. A gate that never opens is a silently stale mirror, which is worse
//!    than the cost it saves.
//!  * **The gate closes** — a keystroke that changes nothing must not republish.
//!    Each mirror setter replaces its Lua table wholesale, so table *identity*
//!    across a keystroke tells a skipped push from a redundant one; without this
//!    the perf fix could regress and every correctness test would still pass.

use nxvim_rpc::Rpc;
use nxvim_test_harness::{command, exec_lua, feed, lines, start_with_file as open};
use rmpv::Value;

fn sample(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

/// Register a handler on a per-keystroke event, so the mirrors are pushed on every
/// key the way a loaded config makes them — not only when a chunk reaches Lua.
async fn push_every_key(rpc: &Rpc) {
    exec_lua(rpc, "nx.on('CursorMoved', function() end)").await;
}

/// Pin the identity of a mirror's Lua table, for [`same_table`].
async fn pin(rpc: &Rpc, expr: &str) {
    exec_lua(rpc, &format!("_G.__pinned = {expr}")).await;
}

/// Whether `expr` is still the exact table pinned by [`pin`] — false when the
/// server re-pushed the mirror (every setter installs a fresh table).
async fn same_table(rpc: &Rpc, expr: &str) -> bool {
    matches!(
        exec_lua(rpc, &format!("return {expr} == _G.__pinned")).await,
        Value::Boolean(true)
    )
}

fn as_int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(-1)
}

fn as_str(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Fill the quickfix list with `n` synthetic entries.
async fn set_qf(rpc: &Rpc, n: usize, title: &str) {
    exec_lua(
        rpc,
        &format!(
            r#"
            local items = {{}}
            for i = 1, {n} do
              items[i] = {{ filename = "/tmp/f" .. i .. ".rs", lnum = i, col = 1,
                            text = "hit " .. i, type = "E" }}
            end
            nx.setqflist({{}}, " ", {{ title = "{title}", items = items }})
            "#
        ),
    )
    .await;
}

// ---------------------------------------------------------------- the gate opens

#[tokio::test]
async fn the_qf_mirror_follows_every_list_mutation() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;

    set_qf(&rpc, 3, "first").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 3);
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getqflist({ title = 0 }).title").await),
        "first"
    );

    // Append (action 'a') into the current list.
    exec_lua(
        &rpc,
        r#"nx.setqflist({}, "a", { items = { { filename = "/tmp/x.rs", lnum = 9, text = "extra" } } })"#,
    )
    .await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 4);

    // Replace (action 'r') the current list in place.
    exec_lua(
        &rpc,
        r#"nx.setqflist({}, "r", { items = { { filename = "/tmp/y.rs", lnum = 1, text = "only" } } })"#,
    )
    .await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 1);
    assert_eq!(
        as_str(&exec_lua(&rpc, "return nx.getqflist()[1].text").await),
        "only"
    );

    // A new list pushes onto the stack; `:colder` walks back to the previous one.
    set_qf(&rpc, 7, "second").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 7);
    command(&rpc, "colder").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 1);
    command(&rpc, "cnewer").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 7);
}

#[tokio::test]
async fn the_loclist_mirror_follows_its_window() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    command(&rpc, "split").await;

    exec_lua(
        &rpc,
        r#"
        _G.__win = nx.win.current()
        nx.setloclist(0, {}, " ", { title = "loc",
          items = { { filename = "/tmp/a.rs", lnum = 2, text = "one" },
                    { filename = "/tmp/b.rs", lnum = 3, text = "two" } } })
        "#,
    )
    .await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getloclist(0)").await), 2);
    assert_eq!(
        as_int(&exec_lua(&rpc, "return #(nx._loclist[_G.__win] or {}).items").await),
        2
    );

    // Closing the owner window must drop it from the mirror — the case a
    // content-only gate would miss.
    command(&rpc, "close").await;
    assert_eq!(
        as_int(&exec_lua(&rpc, "return nx._loclist[_G.__win] == nil and 1 or 0").await),
        1
    );
}

#[tokio::test]
async fn the_register_mirror_follows_every_write() {
    let (rpc, _i) = open("alpha\nbravo\ncharlie\n").await;
    push_every_key(&rpc).await;

    // Yank fills the unnamed register and "0.
    feed(&rpc, "yy");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, r#"return vim.fn.getreg('"')"#).await),
        "alpha\n"
    );
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('0')").await),
        "alpha\n"
    );

    // A linewise delete shifts the numbered ring: the freshest lands in "1.
    feed(&rpc, "dd");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('1')").await),
        "alpha\n"
    );
    feed(&rpc, "dd");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('1')").await),
        "bravo\n"
    );
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('2')").await),
        "alpha\n"
    );

    // A named register, and an uppercase append onto it.
    feed(&rpc, "\"ayy");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('a')").await),
        "charlie\n"
    );

    // `setreg` through the API lands too (it queues a server-side op).
    exec_lua(&rpc, "vim.fn.setreg('z', 'from-lua')").await;
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('z')").await),
        "from-lua"
    );
}

#[tokio::test]
async fn the_register_mirror_follows_the_read_only_specials() {
    // `%` `/` `:` `.` are resolved from live editor state, not stored in the
    // register file — so a gate keyed on the stored registers alone would freeze
    // them. This is the test that fails if the gate forgets them.
    let (rpc, _i) = open("alpha\nbravo\ncharlie\n").await;
    push_every_key(&rpc).await;

    feed(&rpc, "/bravo<CR>");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('/')").await),
        "bravo"
    );

    feed(&rpc, "/charlie<CR>");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('/')").await),
        "charlie"
    );

    // Typed as keys, not `nvim_command`: the `:` register records what the user
    // entered on the command line.
    feed(&rpc, ":noh<CR>");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg(':')").await),
        "noh"
    );

    feed(&rpc, "ggIxyz<Esc>");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getreg('.')").await),
        "xyz"
    );
}

// --------------------------------------------------------------- the gate closes

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_qf_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    set_qf(&rpc, 50, "hits").await;

    pin(&rpc, "nx._qflist").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "nx._qflist").await,
        "a keystroke that changed no list republished the whole quickfix mirror",
    );

    // …and the gate still opens for a real change.
    set_qf(&rpc, 2, "fewer").await;
    assert!(
        !same_table(&rpc, "nx._qflist").await,
        "a new quickfix list did not reach the mirror",
    );
    assert_eq!(as_int(&exec_lua(&rpc, "return #nx.getqflist()").await), 2);
}

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_register_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    feed(&rpc, "yy");
    let _ = lines(&rpc).await;

    pin(&rpc, "nx._registers").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "nx._registers").await,
        "a keystroke that wrote no register republished the whole register file",
    );

    feed(&rpc, "yy");
    let _ = lines(&rpc).await;
    assert!(
        !same_table(&rpc, "nx._registers").await,
        "a yank did not reach the register mirror",
    );
}

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_loclist_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    exec_lua(
        &rpc,
        r#"nx.setloclist(0, {}, " ", { items = { { filename = "/tmp/a.rs", lnum = 2, text = "one" } } })"#,
    )
    .await;

    pin(&rpc, "nx._loclist").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "nx._loclist").await,
        "a keystroke that changed no location list rebuilt the whole loclist mirror",
    );
}

// ------------------------------------------------------------------ perf guards

/// Time `keys` keystrokes of insert-mode typing in the middle of the buffer.
async fn time_typing(rpc: &Rpc, keys: usize) -> std::time::Duration {
    feed(rpc, "500GI");
    let _ = lines(rpc).await; // the setup has landed before the clock starts
    let started = std::time::Instant::now();
    for _ in 0..keys {
        feed(rpc, "z");
    }
    let _ = lines(rpc).await;
    started.elapsed()
}

#[tokio::test]
async fn typing_does_not_scale_with_the_quickfix_list() {
    // The regression guard. Re-serializing every entry on every tick made 300
    // keystrokes with 5000 hits take 8.7 s against a 0.49 s baseline (18x); gated,
    // the two are indistinguishable. A **ratio** rather than a wall-clock bound so
    // both halves see the same machine load under a loaded `cargo test
    // --workspace` — the convention the extmark guards established.
    let (plain, _i1) = open(&sample(1_000)).await;
    push_every_key(&plain).await;
    let baseline = time_typing(&plain, 300).await;

    let (heavy, _i2) = open(&sample(1_000)).await;
    push_every_key(&heavy).await;
    set_qf(&heavy, 5_000, "hits").await;
    assert_eq!(
        as_int(&exec_lua(&heavy, "return #nx.getqflist()").await),
        5_000,
        "the benchmark must not pass by having built no list",
    );
    let loaded = time_typing(&heavy, 300).await;

    let ratio = loaded.as_secs_f64() / baseline.as_secs_f64().max(0.001);
    assert!(
        ratio < 3.0,
        "typing with a 5000-entry quickfix list cost {ratio:.1}x a list-free buffer \
         ({loaded:?} vs {baseline:?}) — the quickfix mirror is being rebuilt per tick again",
    );
}

#[tokio::test]
async fn typing_does_not_scale_with_the_register_contents() {
    let (plain, _i1) = open(&sample(1_000)).await;
    push_every_key(&plain).await;
    let baseline = time_typing(&plain, 300).await;

    let (heavy, _i2) = open(&sample(1_000)).await;
    push_every_key(&heavy).await;
    // A megabyte in a register — what `ggyG` on a real source file leaves behind.
    // Written straight through `setreg` rather than yanked so the buffer under the
    // cursor stays identical to the baseline's: the only difference between the two
    // runs is what the register file holds.
    exec_lua(
        &heavy,
        "vim.fn.setreg('a', string.rep('x', 32 * 1024 * 1024))",
    )
    .await;
    let _ = lines(&heavy).await;
    assert!(
        as_int(&exec_lua(&heavy, r#"return #vim.fn.getreg('a')"#).await) == 32 * 1024 * 1024,
        "the benchmark must not pass by having stored nothing",
    );
    let loaded = time_typing(&heavy, 300).await;

    let ratio = loaded.as_secs_f64() / baseline.as_secs_f64().max(0.001);
    assert!(
        ratio < 2.0,
        "typing with a whole file in the registers cost {ratio:.1}x an empty-register \
         buffer ({loaded:?} vs {baseline:?}) — the register mirror is copying per tick again",
    );
}
