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

use bemtvi_rpc::Rpc;
use bemtvi_test_harness::{command, exec_lua, feed, lines, start_with_file as open};
use rmpv::Value;

fn sample(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

/// Register a handler on a per-keystroke event, so the mirrors are pushed on every
/// key the way a loaded config makes them — not only when a chunk reaches Lua.
async fn push_every_key(rpc: &Rpc) {
    exec_lua(rpc, "btv.on('CursorMoved', function() end)").await;
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
            btv.setqflist({{}}, " ", {{ title = "{title}", items = items }})
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
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 3);
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getqflist({ title = 0 }).title").await),
        "first"
    );

    // Append (action 'a') into the current list.
    exec_lua(
        &rpc,
        r#"btv.setqflist({}, "a", { items = { { filename = "/tmp/x.rs", lnum = 9, text = "extra" } } })"#,
    )
    .await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 4);

    // Replace (action 'r') the current list in place.
    exec_lua(
        &rpc,
        r#"btv.setqflist({}, "r", { items = { { filename = "/tmp/y.rs", lnum = 1, text = "only" } } })"#,
    )
    .await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 1);
    assert_eq!(
        as_str(&exec_lua(&rpc, "return btv.getqflist()[1].text").await),
        "only"
    );

    // A new list pushes onto the stack; `:colder` walks back to the previous one.
    set_qf(&rpc, 7, "second").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 7);
    command(&rpc, "colder").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 1);
    command(&rpc, "cnewer").await;
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 7);
}

#[tokio::test]
async fn the_loclist_mirror_follows_its_window() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    command(&rpc, "split").await;

    exec_lua(
        &rpc,
        r#"
        _G.__win = btv.win.current()
        btv.setloclist(0, {}, " ", { title = "loc",
          items = { { filename = "/tmp/a.rs", lnum = 2, text = "one" },
                    { filename = "/tmp/b.rs", lnum = 3, text = "two" } } })
        "#,
    )
    .await;
    assert_eq!(
        as_int(&exec_lua(&rpc, "return #btv.getloclist(0)").await),
        2
    );
    assert_eq!(
        as_int(&exec_lua(&rpc, "return #(btv._loclist[_G.__win] or {}).items").await),
        2
    );

    // Closing the owner window must drop it from the mirror — the case a
    // content-only gate would miss.
    command(&rpc, "close").await;
    assert_eq!(
        as_int(&exec_lua(&rpc, "return btv._loclist[_G.__win] == nil and 1 or 0").await),
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

    pin(&rpc, "btv._qflist").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "btv._qflist").await,
        "a keystroke that changed no list republished the whole quickfix mirror",
    );

    // …and the gate still opens for a real change.
    set_qf(&rpc, 2, "fewer").await;
    assert!(
        !same_table(&rpc, "btv._qflist").await,
        "a new quickfix list did not reach the mirror",
    );
    assert_eq!(as_int(&exec_lua(&rpc, "return #btv.getqflist()").await), 2);
}

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_register_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    feed(&rpc, "yy");
    let _ = lines(&rpc).await;

    pin(&rpc, "btv._registers").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "btv._registers").await,
        "a keystroke that wrote no register republished the whole register file",
    );

    feed(&rpc, "yy");
    let _ = lines(&rpc).await;
    assert!(
        !same_table(&rpc, "btv._registers").await,
        "a yank did not reach the register mirror",
    );
}

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_loclist_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    exec_lua(
        &rpc,
        r#"btv.setloclist(0, {}, " ", { items = { { filename = "/tmp/a.rs", lnum = 2, text = "one" } } })"#,
    )
    .await;

    pin(&rpc, "btv._loclist").await;
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_table(&rpc, "btv._loclist").await,
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
        as_int(&exec_lua(&heavy, "return #btv.getqflist()").await),
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

// ---------------------------------------------------------- the `bo` mirror gate
//
// `btv._bo_mirror[bufnr]` carries every buffer-local option `vim.bo` reads. It used
// to be rebuilt for EVERY open buffer on every tick — i.e. on every keystroke — so a
// session with many buffers paid a full per-buffer rebuild per key. It is now gated
// on the core's option-state generation (any `:set`-family / `vim.o` / `vim.bo`
// write, a filetype or `ts_highlight` change, a completed save) plus the buffer's own
// `changedtick` (a text edit flips `modified`).
//
// The mirror now MERGES rows rather than replacing the whole table, so the table's
// identity no longer moves on a push — the probe pins the *row* instead, which each
// push replaces wholesale.

/// Pin the identity of buffer `bufnr`'s `bo` row.
async fn pin_row(rpc: &Rpc, bufnr: &str) {
    exec_lua(rpc, &format!("_G.__row = btv._bo_mirror[{bufnr}]")).await;
}

/// Whether buffer `bufnr`'s row is still the exact table pinned by [`pin_row`].
async fn same_row(rpc: &Rpc, bufnr: &str) -> bool {
    matches!(
        exec_lua(rpc, &format!("return btv._bo_mirror[{bufnr}] == _G.__row")).await,
        Value::Boolean(true)
    )
}

/// The current buffer's number, as a Lua expression string.
const CUR: &str = "btv._cur_buf.bufnr";

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_bo_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    let _ = lines(&rpc).await;

    pin_row(&rpc, CUR).await;
    // A pure cursor move: no option moved, no text changed.
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert!(
        same_row(&rpc, CUR).await,
        "a keystroke that changed no option and no text rebuilt the whole bo mirror",
    );
}

#[tokio::test]
async fn a_set_option_reaches_the_bo_mirror() {
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    let _ = lines(&rpc).await;

    pin_row(&rpc, CUR).await;
    command(&rpc, "set tabstop=7").await;
    let _ = lines(&rpc).await;
    assert!(
        !same_row(&rpc, CUR).await,
        "a `:set` did not reach the bo mirror — the gate never opened",
    );
    assert_eq!(as_int(&exec_lua(&rpc, "return vim.bo.tabstop").await), 7);
}

#[tokio::test]
async fn a_lua_option_write_reaches_the_bo_mirror() {
    // The `vim.bo` bridge is a different door into the same state than `:set`; the
    // generation must be bumped there too or a Lua-written option reads stale.
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    exec_lua(&rpc, "vim.bo.shiftwidth = 3").await;
    let _ = lines(&rpc).await;
    assert_eq!(as_int(&exec_lua(&rpc, "return vim.bo.shiftwidth").await), 3);
}

#[tokio::test]
async fn a_filetype_change_reaches_the_bo_mirror() {
    // `filetype` is a `bo` row that does NOT live in the options struct (it is the
    // treesitter language noun), so it needs its own generation bump.
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    let _ = lines(&rpc).await;

    pin_row(&rpc, CUR).await;
    command(&rpc, "set filetype=rust").await;
    let _ = lines(&rpc).await;
    assert!(
        !same_row(&rpc, CUR).await,
        "a filetype change must republish"
    );
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.bo.filetype").await),
        "rust"
    );
}

#[tokio::test]
async fn an_edit_reaches_the_bo_mirror_through_modified() {
    // The one `bo` row a *text* edit moves is `modified`, which the per-buffer
    // `changedtick` gate covers rather than the option generation.
    let (rpc, _i) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    let _ = lines(&rpc).await;
    assert!(
        !matches!(
            exec_lua(&rpc, "return vim.bo.modified").await,
            Value::Boolean(true)
        ),
        "a freshly-opened buffer is unmodified"
    );
    feed(&rpc, "x");
    let _ = lines(&rpc).await;
    assert!(
        matches!(
            exec_lua(&rpc, "return vim.bo.modified").await,
            Value::Boolean(true)
        ),
        "an edit must flip `modified` in the mirror",
    );
}

#[tokio::test]
async fn a_deleted_buffer_is_dropped_from_the_bo_mirror() {
    // The mirror merges rows now, so a vanished buffer's row has to be deleted
    // explicitly — otherwise `vim.bo[dead]` keeps reading a live-looking row
    // forever, and the table grows with every closed buffer.
    let (rpc, _i) = open(&sample(5)).await;
    push_every_key(&rpc).await;
    command(&rpc, "enew").await;
    let _ = lines(&rpc).await;
    let dead = as_int(&exec_lua(&rpc, &format!("return {CUR}")).await);
    assert!(
        matches!(
            exec_lua(&rpc, &format!("return btv._bo_mirror[{dead}] ~= nil")).await,
            Value::Boolean(true)
        ),
        "the new buffer has a row while it is open",
    );
    command(&rpc, &format!("bwipeout! {dead}")).await;
    let _ = lines(&rpc).await;
    assert!(
        matches!(
            exec_lua(&rpc, &format!("return btv._bo_mirror[{dead}] == nil")).await,
            Value::Boolean(true)
        ),
        "a wiped buffer's row must be dropped from the merged mirror",
    );
}

// ------------------------------------------------------- the jumplist mirror gate
//
// Each window's jumplist rides the per-window mirror. It used to be re-serialized in
// full on every repaint; it is now gated on a per-window structural generation, so an
// unchanged jumplist costs nothing per keystroke.

/// Pin the identity of the focused window's mirrored jumplist.
async fn pin_jumps(rpc: &Rpc) {
    exec_lua(rpc, "_G.__jumps = btv._wins[btv._cur_win].jumps").await;
}

async fn same_jumps(rpc: &Rpc) -> bool {
    matches!(
        exec_lua(rpc, "return btv._wins[btv._cur_win].jumps == _G.__jumps").await,
        Value::Boolean(true)
    )
}

#[tokio::test]
async fn an_idle_keystroke_does_not_republish_the_jumplist() {
    let (rpc, _i) = open(&sample(200)).await;
    push_every_key(&rpc).await;
    // Seed a couple of entries so there is something worth not re-serializing.
    feed(&rpc, "50G");
    feed(&rpc, "100G");
    let _ = lines(&rpc).await;

    // The identity probe below is only meaningful over a NON-EMPTY list: a mirror
    // that lost its jumplist entirely pins nil and then compares nil to nil, which
    // passes for the wrong reason. Pin the length too.
    let n = as_int(&exec_lua(&rpc, "return #(btv._wins[btv._cur_win].jumps or {})").await);
    assert_eq!(
        n, 2,
        "the two jumps must be in the mirror before we gate on it"
    );

    pin_jumps(&rpc).await;
    // `j` is not a jump, so the list must not move.
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_int(&exec_lua(&rpc, "return #(btv._wins[btv._cur_win].jumps or {})").await),
        2,
        "the gated push must CARRY the list over, not drop it",
    );
    assert!(
        same_jumps(&rpc).await,
        "a non-jump keystroke re-serialized the whole jumplist",
    );
}

#[tokio::test]
async fn a_jump_reaches_the_jumplist_mirror() {
    let (rpc, _i) = open(&sample(200)).await;
    push_every_key(&rpc).await;
    feed(&rpc, "50G");
    let _ = lines(&rpc).await;

    let before = as_int(&exec_lua(&rpc, "return #btv._wins[btv._cur_win].jumps").await);
    pin_jumps(&rpc).await;
    feed(&rpc, "150G");
    let _ = lines(&rpc).await;
    assert!(
        !same_jumps(&rpc).await,
        "a real jump did not reach the mirror — the gate never opened",
    );
    assert_eq!(
        as_int(&exec_lua(&rpc, "return #btv._wins[btv._cur_win].jumps").await),
        before + 1,
        "the new jump is in the mirrored list",
    );
}

#[tokio::test]
async fn navigating_the_jumplist_reaches_the_mirror() {
    // `<C-o>` moves only the navigation POINTER, not the entries. The pointer is
    // read fresh every tick, but the generation must move too — otherwise a plugin
    // reading the mirror sees a stale index forever.
    let (rpc, _i) = open(&sample(200)).await;
    push_every_key(&rpc).await;
    feed(&rpc, "50G");
    feed(&rpc, "150G");
    let _ = lines(&rpc).await;
    let idx = as_int(&exec_lua(&rpc, "return btv._wins[btv._cur_win].jump_idx").await);

    feed(&rpc, "<C-o>");
    let _ = lines(&rpc).await;
    assert_eq!(
        as_int(&exec_lua(&rpc, "return btv._wins[btv._cur_win].jump_idx").await),
        idx - 1,
        "`<C-o>` must move the mirrored jumplist pointer",
    );
}
