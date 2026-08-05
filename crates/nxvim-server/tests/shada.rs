//! Shada persistence — cross-session state survives a server restart, and the
//! per-instance stores compact rather than accumulating.
//!
//! Black-box, per the harness convention: spawn a server against a **temp** state
//! dir (so the real `~/.local/state` is never touched and the test stays
//! hermetic), drive it with `nx_input`, quit, then **respawn** a second server
//! against the same dir and assert the first session's state was restored.
//!
//! Phase 1 covers registers; Phase 2 the global file marks `A`–`Z`; Phase 3 the
//! per-file marks (incl. the `` `" `` last-cursor) and search/ex history; Phase 4
//! the numbered marks `'0`–`'9`, the jumplist, and the changelist; Phase 5 the
//! debounced live checkpoint (a flush fires mid-session, not just at exit). See
//! `docs/plans/2026-06-11-shada-persistence.md`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_server::{is_store_file, PersistState, RedbFileStore, ServerInit, ShadaStore};
use nxvim_test_harness::{
    await_server_exit, cursor, exec_lua, feed, lines, start_attached, temp_dir, write_temp,
};

/// A server that persists into `dir` via the native redb store.
fn init_with_store(dir: &Path, file: Option<String>) -> ServerInit {
    ServerInit {
        file,
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        ..Default::default()
    }
}

/// A **workspace** server: a private primary store under `primary`, plus the shared
/// global history store under `global` (used only when `'persisthistory'` targets it).
fn init_workspace(primary: &Path, global: &Path, file: Option<String>) -> ServerInit {
    ServerInit {
        file,
        shada: Some(Box::new(RedbFileStore::new(primary.to_path_buf()))),
        global_shada: Some(Box::new(RedbFileStore::new(global.to_path_buf()))),
        workspace_session: true,
        ..Default::default()
    }
}

/// A probe [`ShadaStore`] that records every flushed snapshot in memory, so a test
/// can prove a *live* checkpoint fired mid-session (not just the exit flush). `load`
/// hands back an empty state — this exercises the flush cadence, not restore, which
/// the redb-backed tests above already cover.
#[derive(Clone, Default)]
struct ProbeStore {
    flushes: Arc<Mutex<Vec<PersistState>>>,
}

impl ShadaStore for ProbeStore {
    fn load(&mut self) -> std::io::Result<PersistState> {
        Ok(PersistState::default())
    }
    fn flush(&mut self, state: &PersistState, _compact: bool) -> std::io::Result<()> {
        self.flushes.lock().unwrap().push(state.clone());
        Ok(())
    }
    fn reload(&mut self) -> std::io::Result<PersistState> {
        Ok(PersistState::default())
    }
}

/// Count the surviving shada store files in `dir`.
fn store_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_store_file(p))
        .collect()
}

#[tokio::test]
async fn register_survives_a_restart() {
    let dir = temp_dir("shada_registers");
    let file = write_temp("shada_registers", "txt", "hello world\n");

    // Session 1: yank "hello" into register `a`, then quit.
    {
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        feed(&rpc, "\"ayiw");
        // Barrier: ensure the yank is processed before we quit.
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        // Wait for the server to fully exit — guarantees the store was flushed.
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server against the same store. Register `a` should hold
    // "hello" from the previous session, so `"ap` pastes it.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        feed(&rpc, "\"ap");
        assert_eq!(lines(&rpc).await, vec!["hello"]);
    }
}

#[tokio::test]
async fn manual_folds_survive_a_restart() {
    let dir = temp_dir("shada_folds");
    let file = write_temp("shada_folds", "txt", "L1\nL2\nL3\nL4\nL5\nL6\n");

    // Session 1: create a closed manual fold over lines 2-4 (`zf2j`), then quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "2Gzf2j");
        assert_eq!(
            lines(&rpc).await,
            vec!["L1", "L2", "L3", "L4", "L5", "L6"],
            "the fold hides lines on screen but leaves the buffer intact"
        );
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: reopen the same file. The manual fold is restored *closed*, so
    // `dd` on its header line (2) deletes the whole fold range — proving both that
    // the fold survived and that its closed state did.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        feed(&rpc, "2Gdd");
        assert_eq!(
            lines(&rpc).await,
            vec!["L1", "L5", "L6"],
            "the restored closed fold makes dd remove all of lines 2-4"
        );
    }
}

#[tokio::test]
async fn stores_compact_instead_of_accumulating() {
    let dir = temp_dir("shada_compaction");

    // Session 1: yank into register `a`, exit. One store file is left behind.
    {
        let f = write_temp("shada_compaction", "txt", "alpha\n");
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(f)), 80, 25).await;
        feed(&rpc, "\"ayy");
        assert_eq!(lines(&rpc).await, vec!["alpha"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    assert_eq!(
        store_files(&dir).len(),
        1,
        "one store after the first session"
    );

    // Session 2: a new instance mints its own file but absorbs + deletes session
    // 1's, so after it exits there is still exactly one file — count is bounded by
    // live instances, not total launches.
    {
        let f = write_temp("shada_compaction", "txt", "beta\n");
        let (rpc, incoming) = start_attached(init_with_store(&dir, Some(f)), 80, 25).await;
        feed(&rpc, "\"byy");
        assert_eq!(lines(&rpc).await, vec!["beta"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    assert_eq!(
        store_files(&dir).len(),
        1,
        "still one store after the second session — the first was compacted away"
    );

    // Session 3: both registers survived the carry-forward (session 1's `a` was
    // merged into session 2's store, which also holds session 2's `b`).
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        feed(&rpc, "\"ap\"bp");
        assert_eq!(lines(&rpc).await, vec!["", "alpha", "beta"]);
    }
}

#[tokio::test]
async fn an_absorbed_sibling_stays_visible_while_the_absorber_is_live() {
    // Compaction is deferred to a clean exit, not done at load. While instance B is
    // live it holds its own file's lock, so the data it merged from a dead sibling A
    // must remain readable *on disk* (in A's not-yet-deleted file) — otherwise a
    // freshly launched C, which skips B's locked file, would lose A's data for the
    // whole of B's session. This is neovim's "you see another instance's data once
    // it has written" contract: deleting A at B's load would break it.
    let dir = temp_dir("shada_absorb_visibility");
    let file_a = write_temp("shada_absorb_a", "txt", "from A\n");
    let file_c = write_temp("shada_absorb_c", "txt", "ccc\n");

    // A: yank a line into register `x`, then exit cleanly — leaves one readable store.
    {
        let (rpc_a, inc_a) = start_attached(init_with_store(&dir, Some(file_a)), 80, 25).await;
        feed(&rpc_a, "\"xyy");
        assert_eq!(lines(&rpc_a).await, vec!["from A"]);
        feed(&rpc_a, ":qa!<CR>");
        await_server_exit(inc_a).await;
    }

    // B: launches and merges A's store at load, then stays live (no exit), holding
    // its own file's lock for the rest of the test.
    let (_rpc_b, _inc_b) = start_attached(init_with_store(&dir, None), 80, 25).await;

    // C: a brand-new instance launched while B is still live. B's file is locked
    // (skipped), so the only way C can see register `x` is if A's file still exists —
    // which it must, because B deferred A's deletion to its (never-reached) exit.
    {
        let (rpc_c, _inc_c) = start_attached(init_with_store(&dir, Some(file_c)), 80, 25).await;
        feed(&rpc_c, "\"xp");
        assert_eq!(lines(&rpc_c).await, vec!["ccc", "from A"]);
    }
}

#[tokio::test]
async fn global_mark_survives_a_restart() {
    let dir = temp_dir("shada_global_mark");
    let file = write_temp("shada_global_mark", "txt", "alpha\n  beta\ngamma\n");

    // Session 1: open the file, set global mark `A` on line 2, col 4, then quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggj04l");
        assert_eq!(cursor(&rpc).await, (2, 4));
        feed(&rpc, "mA");
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server with an *empty* buffer (no file argument). Jumping
    // to `` `A `` must lazily reopen the marked file and land at the saved spot —
    // the file was never loaded at startup, only on the jump.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        // Precondition: the marked file is not open yet — the buffer is empty.
        assert_eq!(lines(&rpc).await, vec![""]);
        feed(&rpc, "`A");
        assert_eq!(lines(&rpc).await, vec!["alpha", "  beta", "gamma"]);
        assert_eq!(cursor(&rpc).await, (2, 4));
    }
}

#[tokio::test]
async fn last_cursor_mark_reopens_a_file_where_it_was_left() {
    let dir = temp_dir("shada_quote_mark");
    let file = write_temp("shada_quote_mark", "txt", "alpha\nbeta\ngamma\ndelta\n");

    // Session 1: move the cursor to line 3, col 2 and quit (no edit). The current
    // buffer's live cursor becomes its `"` last-cursor mark at the exit flush.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggjjll");
        assert_eq!(cursor(&rpc).await, (3, 2));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: reopen the same file; the cursor starts at the top, and `` `" ``
    // returns to the saved last-cursor spot.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "`\"");
        assert_eq!(cursor(&rpc).await, (3, 2));
    }
}

/// With `restorecursor` enabled, reopening a file lands the cursor on the saved
/// last-cursor position automatically — no manual `` `" `` — via the built-in
/// `BufReadPost` autocmd (neovim's recipe, dogfooded on `:normal! g\`"`).
#[tokio::test]
async fn restorecursor_option_reopens_a_file_where_it_was_left() {
    let dir = temp_dir("shada_restorecursor");
    let cfg = temp_dir("shada_restorecursor_cfg");
    std::fs::write(cfg.join("init.lua"), "vim.o.restorecursor = true\n").expect("init.lua");
    let file = write_temp("shada_restorecursor", "txt", "alpha\nbeta\ngamma\ndelta\n");

    let init = |file: Option<String>| ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        ..init_with_store(&dir, file)
    };

    // Session 1: leave the cursor at line 3, col 2, then quit.
    {
        let (rpc, incoming) = start_attached(init(Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggjjll");
        assert_eq!(cursor(&rpc).await, (3, 2));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: reopen — the cursor is already on the saved spot, no jump fed.
    {
        let (rpc, _incoming) = start_attached(init(Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (3, 2));
    }
}

/// Default (option off): a reopened file still starts at the top — restore is
/// strictly opt-in, so existing behavior is unchanged for everyone else.
#[tokio::test]
async fn restorecursor_off_by_default_opens_at_top() {
    let dir = temp_dir("shada_restorecursor_off");
    let file = write_temp(
        "shada_restorecursor_off",
        "txt",
        "alpha\nbeta\ngamma\ndelta\n",
    );

    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggjjll");
        assert_eq!(cursor(&rpc).await, (3, 2));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
    }
}

#[tokio::test]
async fn search_history_survives_a_restart() {
    let dir = temp_dir("shada_history");
    let file = write_temp("shada_history", "txt", "alpha\nbeta\ngamma\n");

    // Session 1: run a `/gamma` search (pushes the search history), then quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "/gamma<CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: open `/`, recall the restored pattern with <Up>, run it — the
    // cursor lands on `gamma`, proving the history came back.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "/<Up><CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
    }
}

#[tokio::test]
async fn input_namespace_history_survives_a_workspace_restart() {
    // `nx.ui.input{ history = "<ns>" }` rings (e.g. the DAP repl's `dap>` prompt recall)
    // must persist to the workspace shada like the `:` / `/` histories — they did not, so
    // a reopened project lost its prompt history under `--workspace`.
    let gdir = temp_dir("shada_inh_global");
    let ws = temp_dir("shada_inh_ws");

    // Session 1: open a prompt under the `repl` namespace, submit `myexpr`, then quit.
    {
        let (rpc, incoming) = start_attached(init_workspace(&ws, &gdir, None), 80, 25).await;
        exec_lua(
            &rpc,
            "nx.ui.input({ prompt = '> ', history = 'repl' }):next(function() end)",
        )
        .await;
        feed(&rpc, "myexpr<CR>");
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2 (same workspace): a fresh prompt under `repl` recalls `myexpr` with <Up>,
    // proving the namespace ring was restored from the workspace store.
    {
        let (rpc, _incoming) = start_attached(init_workspace(&ws, &gdir, None), 80, 25).await;
        exec_lua(
            &rpc,
            "_G.r = nil
             nx.ui.input({ prompt = '> ', history = 'repl' }):next(function(t) _G.r = t end)",
        )
        .await;
        feed(&rpc, "<Up><CR>");
        assert_eq!(
            exec_lua(&rpc, "return _G.r").await.as_str(),
            Some("myexpr"),
            "the input-history namespace ring must restore from the workspace shada"
        );
    }
}

#[tokio::test]
async fn persisthistory_none_does_not_persist() {
    let dir = temp_dir("shada_phist_none");
    let file = write_temp("shada_phist_none", "txt", "alpha\nbeta\ngamma\n");

    // Session 1: disable history persistence, then run a search and quit.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        exec_lua(&rpc, "nx.o.persisthistory = 'none'").await;
        feed(&rpc, "/gamma<CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: nothing was written, so <Up> recalls no pattern — the empty search is
    // E35 and the cursor stays at the top.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        feed(&rpc, "/<Up><CR>");
        assert_eq!(cursor(&rpc).await, (1, 0));
    }
}

#[tokio::test]
async fn default_workspace_history_is_workspace_scoped() {
    // The default `persisthistory = "workspace,global"` saves to the WORKSPACE store
    // when one is open (not the global one): a project's history is restored in that
    // project and does NOT leak to other workspaces.
    let gdir = temp_dir("shada_dws_global");
    let ws1 = temp_dir("shada_dws_ws1");
    let ws2 = temp_dir("shada_dws_ws2");
    let file = write_temp("shada_dws", "txt", "alpha\nbeta\ngamma\n");

    // Workspace 1: search `/gamma`, then quit.
    {
        let (rpc, incoming) =
            start_attached(init_workspace(&ws1, &gdir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "/gamma<CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // A DIFFERENT workspace (same shared global store) does NOT see it — the default
    // kept it workspace-scoped, so `/<Up>` recalls nothing and the cursor stays at top.
    {
        let (rpc, _incoming) =
            start_attached(init_workspace(&ws2, &gdir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "/<Up><CR>");
        assert_eq!(
            cursor(&rpc).await,
            (1, 0),
            "history must not leak across workspaces"
        );
    }

    // Re-opening workspace 1 restores its history (the workspace is loaded again).
    {
        let (rpc, _incoming) =
            start_attached(init_workspace(&ws1, &gdir, Some(file)), 80, 25).await;
        feed(&rpc, "/<Up><CR>");
        assert_eq!(cursor(&rpc).await, (3, 0), "workspace history must restore");
    }
}

/// Write an `init.lua` into a fresh config dir and return it, for tests that need a
/// config-set option (`'persisthistory'` is read post-config).
fn config_with(slug: &str, lua: &str) -> PathBuf {
    let dir = temp_dir(slug);
    std::fs::write(dir.join("init.lua"), lua).expect("write init.lua");
    dir
}

/// A workspace server that also sources `config_dir/init.lua` at startup.
fn init_workspace_cfg(
    primary: &Path,
    global: &Path,
    config_dir: &Path,
    file: Option<String>,
) -> ServerInit {
    ServerInit {
        config_dir: Some(config_dir.to_path_buf()),
        ..init_workspace(primary, global, file)
    }
}

#[tokio::test]
async fn persisthistory_global_targets_the_global_store() {
    // `persisthistory = "global"` on a workspace launch routes history to the shared
    // global store instead of the workspace one, so it crosses workspaces.
    let gdir = temp_dir("shada_ptg_global");
    let ws1 = temp_dir("shada_ptg_ws1");
    let ws2 = temp_dir("shada_ptg_ws2");
    let cfg = config_with("shada_ptg_cfg", "nx.o.persisthistory = 'global'");
    let file = write_temp("shada_ptg", "txt", "alpha\nbeta\ngamma\n");

    // Workspace 1 (history → global): search `/gamma`, quit.
    {
        let (rpc, incoming) = start_attached(
            init_workspace_cfg(&ws1, &gdir, &cfg, Some(file.clone())),
            80,
            25,
        )
        .await;
        feed(&rpc, "/gamma<CR>");
        assert_eq!(cursor(&rpc).await, (3, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Workspace 2 (different primary, same global, also → global): the global history
    // restores, so `/<Up>` recalls `gamma`.
    {
        let (rpc, _incoming) =
            start_attached(init_workspace_cfg(&ws2, &gdir, &cfg, Some(file)), 80, 25).await;
        feed(&rpc, "/<Up><CR>");
        assert_eq!(
            cursor(&rpc).await,
            (3, 0),
            "global-targeted history must cross workspaces"
        );
    }
}

#[tokio::test]
async fn persisthistory_rejects_an_invalid_value() {
    let dir = temp_dir("shada_phist_valid");
    let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;

    // A valid value applies; a bogus one is rejected (E474), leaving the prior value.
    exec_lua(&rpc, "nx.cmd('set persisthistory=global')").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.o.persisthistory").await.as_str(),
        Some("global")
    );
    exec_lua(&rpc, "nx.cmd('set persisthistory=bogus')").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.o.persisthistory").await.as_str(),
        Some("global"),
        "an invalid persisthistory must be rejected, not stored"
    );
}

#[tokio::test]
async fn numbered_marks_shift_across_sessions() {
    let dir = temp_dir("shada_numbered");
    let file = write_temp("shada_numbered", "txt", "L1\nL2\nL3\nL4\nL5\n");

    // Session 1 exits with the cursor on line 4 — that becomes `'0` next launch.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "ggjjj");
        assert_eq!(cursor(&rpc).await, (4, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: `'0` is session 1's exit (line 4). Then exit on line 2, which
    // becomes the *next* launch's `'0` and pushes line 4 down to `'1`.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "`0");
        assert_eq!(cursor(&rpc).await, (4, 0), "`0 is session 1's exit line");
        feed(&rpc, "ggj");
        assert_eq!(cursor(&rpc).await, (2, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 3: `'0` is session 2's exit (line 2); `'1` is the shifted line 4.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        feed(&rpc, "`0");
        assert_eq!(cursor(&rpc).await, (2, 0), "`0 is the most recent exit");
        feed(&rpc, "`1");
        assert_eq!(
            cursor(&rpc).await,
            (4, 0),
            "`1 is the prior exit, shifted down"
        );
    }
}

#[tokio::test]
async fn jumplist_survives_a_restart() {
    let dir = temp_dir("shada_jumplist");
    let file = write_temp("shada_jumplist", "txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");

    // Session 1: `G` then `gg` records two jump-from positions (lines 1 and 10).
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "G"); // jump from line 1 -> line 10
        feed(&rpc, "gg"); // jump from line 10 -> line 1
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: the restored jumplist materializes on the first `<C-o>`, which
    // walks back to the most recent jump-from (line 10).
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "<C-o>");
        assert_eq!(cursor(&rpc).await.0, 10);
    }
}

/// A `--workspace` session keeps its jumplist in its *private* store and never
/// inherits the shared **global** pool: a jump recorded by an unrelated *plain*
/// session (which writes to the global store) must not bleed into a fresh
/// workspace session and surface on its first `<C-o>`. This is the structural
/// reason nxvim avoids neovim's "random file from another session opens on `<C-o>`"
/// annoyance — the global store backs the jumplist only for plain launches, while a
/// workspace session loads its jumplist from `state/shada/ns/<project>/` alone (the
/// global store is consulted only for `:`/`/` history). Guards that isolation.
#[tokio::test]
async fn workspace_jumplist_does_not_inherit_the_global_pool() {
    let gdir = temp_dir("shada_wsj_global");
    let wdir = temp_dir("shada_wsj_ws");
    let foreign = write_temp("shada_wsj_foreign", "txt", "f1\nf2\nf3\nf4\nf5\n");
    let project = write_temp("shada_wsj_project", "txt", "p1\np2\np3\np4\np5\n");

    // A plain session records a jump (foreign:1) into the shared GLOBAL store.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&gdir, Some(foreign.clone())), 80, 25).await;
        feed(&rpc, "G"); // jump from foreign:1
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // A FRESH workspace session (own primary store `wdir`, same global store): with
    // no in-session jump yet, the first `<C-o>` must find nothing — the foreign jump
    // from the global pool must not have leaked into this workspace's jumplist, so
    // the cursor stays in the project file and the foreign file is never reopened.
    {
        let (rpc, _incoming) =
            start_attached(init_workspace(&wdir, &gdir, Some(project.clone())), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "<C-o>");
        let here = exec_lua(&rpc, "return vim.fn.expand('%:p')").await;
        assert_eq!(
            here.as_str(),
            Some(project.as_str()),
            "a fresh workspace session must not inherit the global jumplist pool"
        );
    }
}

#[tokio::test]
async fn changelist_survives_a_restart() {
    let dir = temp_dir("shada_changelist");
    let file = write_temp("shada_changelist", "txt", "aaa\nbbb\nccc\nddd\neee\n");

    // Session 1: edit line 1 and line 3 (two changelist entries), then quit
    // discarding the edits — only the change *positions* persist.
    {
        let (rpc, incoming) =
            start_attached(init_with_store(&dir, Some(file.clone())), 80, 25).await;
        feed(&rpc, "x"); // change on line 1
        feed(&rpc, "3Gx"); // change on line 3
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: `g;` walks the restored changelist back to the newest change
    // (line 3), proving the per-file changelist came back.
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, Some(file)), 80, 25).await;
        assert_eq!(cursor(&rpc).await, (1, 0));
        feed(&rpc, "g;");
        assert_eq!(cursor(&rpc).await.0, 3);
    }
}

#[tokio::test]
async fn debounced_checkpoint_flushes_mid_session() {
    // Phase 5: a yank is checkpointed by the debounced live flush *while the session
    // is still running* — no quit, no exit flush involved — so a crash would lose at
    // most the last debounce window rather than the whole session.
    let file = write_temp("shada_debounce", "txt", "hello world\n");
    let probe = ProbeStore::default();
    let flushes = probe.flushes.clone();

    let (rpc, _incoming) = start_attached(
        ServerInit {
            file: Some(file),
            shada: Some(Box::new(probe)),
            ..Default::default()
        },
        80,
        25,
    )
    .await;

    // Yank "hello" into register `a`; the barrier ensures it is processed (and re-arms
    // the debounce one last time).
    feed(&rpc, "\"ayiw");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);

    // Stay idle past the debounce window: the live checkpoint must fire on its own.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let snapshots = flushes.lock().unwrap();
    assert!(
        snapshots.iter().any(|s| s
            .registers
            .iter()
            .any(|r| r.name == 'a' && r.text == "hello")),
        "a live checkpoint should have flushed register `a` before any exit; \
         saw {} flush(es)",
        snapshots.len(),
    );
    // The live checkpoint must never write the clean-exit cursor — `'0` tracks clean
    // exits only, so a crash leaves the prior session's `'0` intact.
    assert!(
        snapshots.iter().all(|s| s.exit_cursor.is_none()),
        "live checkpoints must leave exit_cursor unset (only the exit flush sets it)",
    );
}

#[tokio::test]
async fn wshada_flushes_synchronously_without_a_quit() {
    // Phase 7: `:wshada` writes the store *now*, on the tick that runs it — not on the
    // debounce timer and not at exit. A probe store records every flush; right after
    // the `:wshada` barrier (no sleep, so the 150ms debounce can't have fired) the
    // register must already be in a flushed snapshot, with `exit_cursor` unset
    // (`:wshada` is not a clean exit).
    let file = write_temp("shada_wshada", "txt", "hello world\n");
    let probe = ProbeStore::default();
    let flushes = probe.flushes.clone();

    let (rpc, _incoming) = start_attached(
        ServerInit {
            file: Some(file),
            shada: Some(Box::new(probe)),
            ..Default::default()
        },
        80,
        25,
    )
    .await;

    feed(&rpc, "\"ayiw");
    feed(&rpc, ":wshada<CR>");
    // Barrier: drives the command to completion (and the `:wshada` drain) before we
    // inspect the flushes. No sleep — so any flush we see was the explicit `:wshada`,
    // not the debounced checkpoint.
    assert_eq!(lines(&rpc).await, vec!["hello world"]);

    let snapshots = flushes.lock().unwrap();
    assert!(
        snapshots.iter().any(|s| s
            .registers
            .iter()
            .any(|r| r.name == 'a' && r.text == "hello")),
        ":wshada should have flushed register `a` synchronously; saw {} flush(es)",
        snapshots.len(),
    );
    assert!(
        snapshots.iter().all(|s| s.exit_cursor.is_none()),
        ":wshada must leave exit_cursor unset — `'0` tracks clean exits only",
    );
}

#[tokio::test]
async fn rshada_merges_a_sibling_that_exited_while_we_were_live() {
    // Phase 7: the concurrent two-*live*-instance case. A and B run at once; a live
    // instance's store is locked, so B cannot see A's data while A runs (neovim's
    // contract). Once A exits cleanly (releasing the lock), B's explicit `:rshada`
    // re-merges A's now-readable store into B's running session — reconciliation
    // between instances at runtime, not just at the next launch.
    let dir = temp_dir("shada_rshada_concurrent");
    let file_a = write_temp("shada_rshada_a", "txt", "hello world\n");
    let file_b = write_temp("shada_rshada_b", "txt", "beta\n");

    // A and B are both live simultaneously: B starts while A holds its lock.
    let (rpc_a, inc_a) = start_attached(init_with_store(&dir, Some(file_a)), 80, 25).await;
    let (rpc_b, _inc_b) = start_attached(init_with_store(&dir, Some(file_b)), 80, 25).await;

    // A yanks a whole line into register `x`, then exits cleanly (final flush +
    // lock release). B was live the whole time and never set `x`.
    feed(&rpc_a, "\"xyy");
    assert_eq!(lines(&rpc_a).await, vec!["hello world"]);
    feed(&rpc_a, ":qa!<CR>");
    await_server_exit(inc_a).await;

    // B re-reads: A's store is now openable, so register `x` lands in B's session and
    // `"xp` pastes A's line below B's.
    feed(&rpc_b, ":rshada<CR>");
    feed(&rpc_b, "\"xp");
    assert_eq!(lines(&rpc_b).await, vec!["beta", "hello world"]);
}

#[tokio::test]
async fn rshada_fills_only_empty_slots_unless_banged() {
    // Phase 7: `:rshada` only fills a register the session hasn't set (a conflict is
    // kept); `:rshada!` overwrites it. The stored value comes from a sibling that
    // exited while this instance stayed live.
    let dir = temp_dir("shada_rshada_replace");
    let file_a = write_temp("shada_rshada_r_a", "txt", "FROM_DISK\n");
    let file_b = write_temp("shada_rshada_r_b", "txt", "live line\n");

    let (rpc_a, inc_a) = start_attached(init_with_store(&dir, Some(file_a)), 80, 25).await;
    let (rpc_b, _inc_b) = start_attached(init_with_store(&dir, Some(file_b)), 80, 25).await;

    // A stores "FROM_DISK" in register `x` and exits, leaving a readable store.
    feed(&rpc_a, "\"xyy");
    assert_eq!(lines(&rpc_a).await, vec!["FROM_DISK"]);
    feed(&rpc_a, ":qa!<CR>");
    await_server_exit(inc_a).await;

    // B sets its *own* register `x` live to a different line.
    feed(&rpc_b, "\"xyy");
    assert_eq!(lines(&rpc_b).await, vec!["live line"]);

    // Plain `:rshada` keeps B's live `x` (a conflict): pasting it yields B's line.
    feed(&rpc_b, ":rshada<CR>");
    feed(&rpc_b, "\"xp");
    assert_eq!(lines(&rpc_b).await, vec!["live line", "live line"]);

    // `:rshada!` overwrites the conflict with the stored value, so the next paste is
    // A's line.
    feed(&rpc_b, ":rshada!<CR>");
    feed(&rpc_b, "\"xp");
    assert_eq!(
        lines(&rpc_b).await,
        vec!["live line", "live line", "FROM_DISK"],
    );
}

#[tokio::test]
async fn no_store_means_no_persistence() {
    // With `shada: None` (the default), a register set in one session must NOT
    // bleed into the next — persistence is strictly opt-in, keeping every other
    // test hermetic.
    let file = write_temp("shada_off", "txt", "hello world\n");
    {
        let (rpc, incoming) = start_attached(
            ServerInit {
                file: Some(file),
                ..Default::default()
            },
            80,
            25,
        )
        .await;
        feed(&rpc, "\"ayiw");
        assert_eq!(lines(&rpc).await, vec!["hello world"]);
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }
    {
        let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
        feed(&rpc, "\"ap");
        // Nothing pasted: register `a` is empty, so the buffer is unchanged.
        assert_eq!(lines(&rpc).await, vec![""]);
    }
}

#[tokio::test]
async fn plugin_namespace_survives_a_restart() {
    // A plugin that opts in via `nx.shada.plugin()` keeps its own isolated key/value
    // data across a restart. Driven over `exec_lua` (no source file on the stack), so
    // the explicit dev-namespace argument is used here — a real plugin gets its
    // namespace assigned from its location, covered by the test below.
    let dir = temp_dir("shada_plugin");

    // Session 1: an opted-in plugin stores a list + a number, then quits.
    {
        let (rpc, incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        let stored = exec_lua(
            &rpc,
            r#"
            local s = nx.shada.plugin("demo")
            s:set("recent", { "a.txt", "b.txt" })
            s:set("count", 3)
            return s:get("count")
            "#,
        )
        .await;
        // Read-back within the session works immediately (in-memory).
        assert_eq!(stored.as_u64(), Some(3));
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server against the same store. The plugin gets its data
    // back, and another namespace sees nothing of it (isolation).
    {
        let (rpc, _incoming) = start_attached(init_with_store(&dir, None), 80, 25).await;
        let count = exec_lua(&rpc, r#"return nx.shada.plugin("demo"):get("count")"#).await;
        assert_eq!(count.as_u64(), Some(3), "number survives the restart");

        let recent = exec_lua(
            &rpc,
            r#"return nx.json.encode(nx.shada.plugin("demo"):get("recent"))"#,
        )
        .await;
        assert_eq!(
            recent.as_str(),
            Some(r#"["a.txt","b.txt"]"#),
            "the table value round-trips through JSON"
        );

        let keys = exec_lua(
            &rpc,
            r#"return table.concat(nx.shada.plugin("demo"):keys(), ",")"#,
        )
        .await;
        assert_eq!(keys.as_str(), Some("count,recent"), "keys() is sorted");

        // A different plugin namespace is empty — a plugin sees only its own slice.
        let other = exec_lua(&rpc, r#"return nx.shada.plugin("other"):get("count")"#).await;
        assert!(other.is_nil(), "another namespace can't read demo's data");
    }
}

#[tokio::test]
async fn plugin_namespace_is_assigned_from_location() {
    // The namespace is derived from where the calling code lives (its runtimepath /
    // plugin dir), not chosen by the plugin. We forge plugin files by loading chunks
    // with an `@<abs-path>` name under a registered rtp entry — exactly how nxvim
    // sources a real plugin — and assert each attributes to its own dir.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
    let summary = exec_lua(
        &rpc,
        r#"
        nx._add_rtp("/virt/plugins/alpha")
        nx._add_rtp("/virt/plugins/beta")
        local function as_plugin(path, body)
          return assert(loadstring(body, "@" .. path))()
        end

        -- alpha (no argument) -> assigned the "alpha" namespace; a second file in the
        -- SAME plugin shares it.
        as_plugin("/virt/plugins/alpha/lua/a.lua", [[ nx.shada.plugin():set("x", 1) ]])
        local alpha_self =
          as_plugin("/virt/plugins/alpha/plugin/init.lua", [[ return nx.shada.plugin():get("x") ]])

        -- beta sees nothing of alpha's data — a plugin can't reach another's slice.
        local beta_x =
          as_plugin("/virt/plugins/beta/lua/b.lua", [[ return nx.shada.plugin():get("x") ]])

        -- A sourced file may NOT claim a DIFFERENT namespace (it would break isolation).
        local forced_ok =
          as_plugin("/virt/plugins/alpha/lua/c.lua", [[ return pcall(nx.shada.plugin, "forced") ]])

        -- ...but it MAY redundantly restate its OWN assigned namespace (a framework that
        -- resolves the ns once at an attributing site and threads it explicitly relies on this).
        local self_ok, self_ns =
          as_plugin("/virt/plugins/alpha/lua/d.lua", [[ return pcall(function() return nx.shada.plugin("alpha").namespace end) ]])

        -- The user's config root maps to the reserved `user` namespace, not its dir
        -- name. The handle exposes its assigned `.namespace`.
        local cfg = vim.fn.stdpath("config")
        nx._add_rtp(cfg)
        local user_ns =
          as_plugin(cfg .. "/lua/u.lua", [[ return nx.shada.plugin().namespace ]])

        -- A plugin loaded by the manager keys on the REGISTERED name, even when it
        -- differs from the install directory's basename.
        nx.plugins._specs["registered-name"] = { name = "registered-name", _dir = "/virt/managed/pkgdir" }
        nx._add_rtp("/virt/managed/pkgdir")
        local managed_ns =
          as_plugin("/virt/managed/pkgdir/lua/m.lua", [[ return nx.shada.plugin().namespace ]])

        return ("alpha_native=%s alpha_self=%s beta_x=%s forced_ok=%s self_ok=%s self_ns=%s user_ns=%s managed_ns=%s"):format(
          tostring(nx._shada_plugin_get("alpha", "x")),
          tostring(alpha_self),
          tostring(beta_x),
          tostring(forced_ok),
          tostring(self_ok),
          tostring(self_ns),
          tostring(user_ns),
          tostring(managed_ns))
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some(
            "alpha_native=1 alpha_self=1 beta_x=nil forced_ok=false self_ok=true self_ns=alpha \
             user_ns=user managed_ns=registered-name"
        ),
        "alpha's two files share the path-derived `alpha` namespace; beta can't see it; \
         a sourced file can't claim a different namespace but may restate its own; the \
         config root maps to `user`; a manager-loaded plugin keys on its registered name"
    );
}

#[tokio::test]
async fn browser_bundled_plugin_attributes_via_its_named_chunk() {
    // Regression for the python web demo crash "nx.shada.plugin: this caller attributes to
    // no plugin". The browser build amalgamates every plugin's `lua/` tree into ONE Lua file
    // that the wasm boot sources under the single chunk name `@init.lua` with an EMPTY
    // runtimepath. A plugin that calls `nx.shada.plugin()` at load (e.g. nxvim-tree's session
    // persistence) therefore attributed to nothing and raised — taking the whole config down.
    //
    // The fix is in the amalgamator: instead of an inline `preload = function() … end` (which
    // inherits the bundle's `@init.lua` source), it compiles each module through
    // `load(<src>, "@<root>/lua/<rel>")` — a plugin-scoped chunk name — and registers a
    // synthetic runtimepath root, so a bundled plugin attributes to its own namespace exactly
    // as a native install does. This drives both shapes to prove the contract the amalgamator
    // now satisfies.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
    let summary = exec_lua(
        &rpc,
        r#"
        -- OLD (broken) amalgamator shape: an inline preload function DEFINED in the bundle
        -- chunk, so its source is `@init.lua`; with no rtp entry for it, nx.shada.plugin()
        -- can't attribute. Forge the exact bundle source name via `load(..., "@init.lua")`.
        assert(load(
          'package.preload["oldtree"] = function(...)\n' ..
          '  return nx.shada.plugin().namespace\n' ..
          'end',
          "@init.lua"))()
        local old_ok, old_err = pcall(require, "oldtree")

        -- NEW (fixed) amalgamator shape: a synthetic rtp root + the module compiled under its
        -- own `@<root>/lua/<rel>` chunk name. A module that persists at load resolves to the
        -- install-dir-basename namespace and its write lands in that isolated slice.
        nx._add_rtp("/nxvim-plugins/webtree")
        package.preload["webtree"] = assert(load(
          'nx.shada.plugin():set("v", 42)\nreturn nx.shada.plugin().namespace',
          "@/nxvim-plugins/webtree/lua/webtree/init.lua"))
        local new_ns = require("webtree")

        return ("old_ok=%s old_err=%s new_ns=%s stored=%s"):format(
          tostring(old_ok),
          tostring(old_err ~= nil and old_err:find("attributes to no plugin", 1, true) ~= nil),
          tostring(new_ns),
          tostring(nx._shada_plugin_get("webtree", "v")))
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some("old_ok=false old_err=true new_ns=webtree stored=42"),
        "an inline bundle module (source @init.lua, no rtp) raises the attribution error, \
         while the amalgamator's load-named chunk + synthetic rtp root attributes a bundled \
         plugin to its own isolated namespace"
    );
}

#[tokio::test]
async fn plugin_namespace_tolerates_a_trailing_slash_rtp_entry() {
    // A runtimepath entry carried in with a TRAILING SLASH (e.g. `NXVIM_CONFIG=foo/`) must
    // still attribute the files under it: the prefix match trims the entry first, so it does
    // not become a never-matching `foo//`. Regression for the "this caller attributes to no
    // plugin" error a trailing-slash launch raised.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
    let got = exec_lua(
        &rpc,
        r#"
        nx._add_rtp("/virt/slashed/")        -- registered WITH a trailing slash
        local function as_plugin(path, body)
          return assert(loadstring(body, "@" .. path))()
        end
        -- A file under it resolves to the dir's basename, not an error.
        return as_plugin("/virt/slashed/lua/x.lua", [[ return nx.shada.plugin().namespace ]])
        "#,
    )
    .await;
    assert_eq!(
        got.as_str(),
        Some("slashed"),
        "a trailing-slash rtp entry still attributes its files (basename namespace)"
    );
}

#[tokio::test]
async fn plugin_namespace_enforces_a_size_budget() {
    // A namespace is capped at 1 MiB: a write that would cross the cap fails LOUD and
    // leaves the prior contents intact; a shrink afterwards is allowed.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
    let summary = exec_lua(
        &rpc,
        r#"
        local s = nx.shada.plugin("budget-demo")
        -- ~900 KB fits.
        s:set("a", string.rep("x", 900 * 1024))
        -- Another ~300 KB would push the namespace over 1 MiB -> error.
        local ok, err = pcall(function() s:set("b", string.rep("y", 300 * 1024)) end)
        local crossed = tostring(ok) .. "/" .. (tostring(err):match("exceed its 1 MiB") and "msg" or "nomsg")

        -- The failed write stored nothing; "a" is untouched, "b" is absent.
        local a_len = #(s:get("a") or "")
        local b_present = s:get("b") ~= nil

        -- Replacing "a" with a small value (a shrink) is always allowed, even though
        -- the set itself is a write to a near-full namespace.
        local shrink_ok = pcall(function() s:set("a", "tiny") end)

        return ("crossed=%s a_len=%d b_present=%s shrink_ok=%s"):format(
          crossed, a_len, tostring(b_present), tostring(shrink_ok))
        "#,
    )
    .await;
    assert_eq!(
        summary.as_str(),
        Some("crossed=false/msg a_len=921600 b_present=false shrink_ok=true"),
        "the over-budget write fails loud, leaves the namespace intact, and a later shrink succeeds"
    );
}

#[tokio::test]
async fn plugin_namespaces_can_be_listed_and_forgotten() {
    // nx.shada.namespaces() audits what plugins have stored; nx.shada.forget(ns)
    // prunes one namespace (its data stops being persisted) without touching another.
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 25).await;
    let out = exec_lua(
        &rpc,
        r#"
        nx.shada.plugin("alpha"):set("k", 1)
        nx.shada.plugin("beta"):set("k", 2)
        local before = table.concat(nx.shada.namespaces(), ",")
        nx.shada.forget("alpha")
        local after = table.concat(nx.shada.namespaces(), ",")
        return ("before=%s after=%s gone=%s kept=%s"):format(
          before, after,
          tostring(nx.shada.plugin("alpha"):get("k")),
          tostring(nx.shada.plugin("beta"):get("k")))
        "#,
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some("before=alpha,beta after=beta gone=nil kept=2"),
        "namespaces() lists both sorted; forget('alpha') drops only alpha"
    );
}

#[tokio::test]
async fn picker_filter_history_survives_a_restart() {
    // The include/exclude glob boxes remember the lines you have used, and that
    // memory outlives the session — so the exclude you worked out on Friday is one
    // `<C-Up>` away on Monday. It rides `nx.shada.plugin`, which flushes on the
    // ordinary shada cadence, so nothing extra is wired into the store.
    let dir = temp_dir("shada_picker_filters");
    let cfg = temp_dir("shada_picker_filters_cfg");
    // A path source with the filter boxes; no process spawn, so this stays hermetic.
    std::fs::write(
        cfg.join("init.lua"),
        r#"
nx.picker.source {
  name = "paths",
  filter = true,
  items = function(ctx)
    for _, p in ipairs({ "src/main.rs", "target/junk.rs" }) do
      ctx.push { text = p, path = p }
    end
  end,
  confirm = function(item) end,
}
"#,
    )
    .expect("write init.lua");
    let init = |dir: &Path| ServerInit {
        config_dir: Some(cfg.clone()),
        runtimepath: vec![cfg.clone()],
        shada: Some(Box::new(RedbFileStore::new(dir.to_path_buf()))),
        ..Default::default()
    };

    // Session 1: use an exclude line, then quit cleanly (flushing the store).
    {
        let (rpc, incoming) = start_attached(init(&dir), 80, 25).await;
        exec_lua(&rpc, "nx.picker.open('paths', { exclude = 'target/' })").await;
        feed(&rpc, "<Esc>");
        // Barrier: let the close (and the history write it triggers) land.
        assert_eq!(
            exec_lua(
                &rpc,
                r#"return table.concat(nx.picker.history("exclude"), "|")"#
            )
            .await
            .as_str(),
            Some("target/"),
            "the line is recorded in-session first"
        );
        feed(&rpc, ":qa!<CR>");
        await_server_exit(incoming).await;
    }

    // Session 2: a fresh server on the same store recalls it — and a filterable
    // picker opens pre-filled with it, which is the point of persisting at all.
    {
        let (rpc, _incoming) = start_attached(init(&dir), 80, 25).await;
        assert_eq!(
            exec_lua(
                &rpc,
                r#"return table.concat(nx.picker.history("exclude"), "|")"#
            )
            .await
            .as_str(),
            Some("target/"),
            "the filter history came back from the previous session"
        );
        assert_eq!(
            exec_lua(&rpc, r#"return #nx.picker.history("include")"#)
                .await
                .as_u64(),
            Some(0),
            "and the box that was never used stayed empty"
        );
    }
}
