//! A project-wide LSP `WorkspaceEdit` (rename / code action) must reach files that
//! aren't open in a buffer **even in a daemon / web session**, where an unopened
//! file's bytes live across the wire and can only be fetched off the editor tick.
//!
//! The reachable seam without a real stdio server is `nx._lsp_apply_workspace_edit`
//! (the Lua entry behind `vim.lsp.util.apply_workspace_edit`): it hands an LSP-shape
//! `WorkspaceEdit` into the same `apply_workspace_edit` path a native rename reply
//! uses. Driving it under an **async daemon fs** (so `host_fs_offtick` is on)
//! exercises the deferred-apply path: the unopened file's replica buffer is created,
//! its fetch enqueued, the edits stashed, and applied once the bytes land.
//!
//! Faithful, not a no-op: the unopened file's path is `/virtual/...`, which the
//! edit-host's *local* disk cannot read — so the renamed content appearing in its
//! buffer can only have come across the wire from the daemon's fs.

use nxvim_test_harness::{
    await_lines, command, cursor, exec_lua, spawn_with_daemon_fs, temp_dir, DaemonFs,
};

/// A rename's `WorkspaceEdit` touches the open file *and* a file that was never
/// opened, in a daemon session. The unopened file's bytes are fetched over the wire,
/// then the stashed edits apply to its replica buffer — so a project-wide rename
/// reaches unopened files off-tick, not just locally.
#[tokio::test]
async fn workspace_edit_reaches_an_unopened_file_off_tick() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/b.rs", "use a::foo;\nfn g() { foo() }\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;

    // The startup file fetches over the wire first.
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"],
        "the open file should load over the daemon wire"
    );

    // Rename `foo` -> `bar`: one occurrence in the open `a.rs`, two in the unopened
    // `b.rs` (which has never been opened in a buffer).
    let edit = "nx._lsp_apply_workspace_edit({ changes = { \
        ['file:///virtual/a.rs'] = { \
          { range = { start = { line = 0, character = 4 }, ['end'] = { line = 0, character = 7 } }, newText = 'bar' } }, \
        ['file:///virtual/b.rs'] = { \
          { range = { start = { line = 0, character = 7 }, ['end'] = { line = 0, character = 10 } }, newText = 'bar' }, \
          { range = { start = { line = 1, character = 9 }, ['end'] = { line = 1, character = 12 } }, newText = 'bar' } } } })";
    exec_lua(&rpc, edit).await;

    // The open buffer is rewritten synchronously.
    assert_eq!(
        await_lines(&rpc, &["let bar = 1"]).await,
        vec!["let bar = 1"],
        "the open file should be renamed in place"
    );

    // The unopened `b.rs` was brought into a replica buffer, its bytes fetched over
    // the wire, and the stashed edits applied once they landed. Switch to it (the
    // edit created the buffer, so `:edit` reuses it) and check both occurrences.
    command(&rpc, "edit /virtual/b.rs").await;
    assert_eq!(
        await_lines(&rpc, &["use a::bar;", "fn g() { bar() }"]).await,
        vec!["use a::bar;", "fn g() { bar() }"],
        "the rename should reach both occurrences in the unopened, off-tick file"
    );
}

/// `create` with `ignoreIfExists` over a file that **is** already there must leave it
/// exactly as it is — the edits that follow land on its real content. Off-tick the
/// editor tick cannot see the remote filesystem, so this used to fall through to
/// "create it empty": the existing content was replaced by whatever the edit inserted,
/// and (since Phase 3 writes a created file out) that emptied version would have been
/// written back over the real file.
#[tokio::test]
async fn an_ignore_if_exists_create_spares_the_remote_file_it_finds() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/keep.rs", "fn keep() {}\nfn tail() {}\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"],
        "the open file should load over the daemon wire"
    );

    // `create keep.rs (ignoreIfExists)`, then an edit that renames its function. The
    // file exists on the daemon, so the create is a no-op and the edit applies to the
    // content that is there.
    let edit = "nx.lsp.apply_workspace_edit({ documentChanges = { \
        { kind = 'create', uri = 'file:///virtual/keep.rs', options = { ignoreIfExists = true } }, \
        { textDocument = { uri = 'file:///virtual/keep.rs', version = 0 }, \
          edits = { { range = { start = { line = 0, character = 3 }, \
                                ['end'] = { line = 0, character = 7 } }, newText = 'kept' } } } } })";
    exec_lua(&rpc, edit).await;

    command(&rpc, "edit /virtual/keep.rs").await;
    assert_eq!(
        await_lines(&rpc, &["fn kept() {}", "fn tail() {}"]).await,
        vec!["fn kept() {}", "fn tail() {}"],
        "the existing remote content must survive, with the edit applied on top of it"
    );
}

/// Two changes naming the **same** unopened off-tick document: the second one used to
/// find the replica buffer the first had just created, and apply into it inline —
/// while its bytes were still crossing the wire, so the landing overwrote them. Both
/// sets of edits have to wait for the fetch.
#[tokio::test]
async fn two_changes_for_one_unopened_file_both_survive_the_fetch() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/b.rs", "one\ntwo\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"]
    );

    let edit = "nx.lsp.apply_workspace_edit({ documentChanges = { \
        { textDocument = { uri = 'file:///virtual/b.rs', version = 0 }, \
          edits = { { range = { start = { line = 0, character = 0 }, \
                                ['end'] = { line = 0, character = 3 } }, newText = 'ONE' } } }, \
        { textDocument = { uri = 'file:///virtual/b.rs', version = 0 }, \
          edits = { { range = { start = { line = 1, character = 0 }, \
                                ['end'] = { line = 1, character = 3 } }, newText = 'TWO' } } } } })";
    exec_lua(&rpc, edit).await;

    command(&rpc, "edit /virtual/b.rs").await;
    assert_eq!(
        await_lines(&rpc, &["ONE", "TWO"]).await,
        vec!["ONE", "TWO"],
        "both changes for the same unopened document must survive the fetch landing"
    );
}

/// The other half of the same probe: `create` with `ignoreIfExists` over a file that
/// turns out **not** to be there is a create after all — the edits filling it land on
/// disk, so a `:q!` can't lose what the refactor extracted. Off-tick nothing on the
/// editor tick knows which case this is; the replica fetch's answer decides it.
///
/// Real paths under a temp dir (not `/virtual/…`) so the write's own leg — the daemon's
/// `fs_write` — can be asserted through the fake's stored content.
#[tokio::test]
async fn an_ignore_if_exists_create_of_an_absent_file_still_lands_on_disk() {
    let dir = temp_dir("lsp_offtick_create");
    let open = dir.join("a.rs");
    let made = dir.join("made.rs");
    // The startup file exists on the daemon; `made.rs` deliberately does not.
    let fake = DaemonFs::with(&open.to_string_lossy(), "let foo = 1\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), &open.to_string_lossy()).await;
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"]
    );

    let edit = format!(
        "nx.lsp.apply_workspace_edit({{ documentChanges = {{ \
        {{ kind = 'create', uri = 'file://{made}', options = {{ ignoreIfExists = true }} }}, \
        {{ textDocument = {{ uri = 'file://{made}', version = 0 }}, \
          edits = {{ {{ range = {{ start = {{ line = 0, character = 0 }}, \
                                   ['end'] = {{ line = 0, character = 0 }} }}, \
                       newText = 'fn created() {{}}\\n' }} }} }} }} }})",
        made = made.display(),
    );
    exec_lua(&rpc, &edit).await;

    let mut written = None;
    for _ in 0..200 {
        written = fake.content(&made.to_string_lossy());
        if written.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        written.as_deref(),
        Some("fn created() {}\n"),
        "an absent file is a real create: written over the wire, with exactly the \
         edit's text and no trailing blank line"
    );
}

/// An edit whose file operation fails takes the rest of *itself* down with it (the
/// `abort` strategy) — including an off-tick `create` whose "is this file there?"
/// probe is still crossing the wire. That probe isn't in the operation queue
/// `drop_workspace_group` empties (it is a replica fetch), so it used to land into an
/// abandoned apply and write the file out regardless: an aborted refactor leaving
/// exactly the artefact it was aborted before creating.
#[tokio::test]
async fn an_aborted_edit_does_not_write_the_file_its_create_probe_was_checking() {
    let dir = temp_dir("lsp_offtick_abort_create");
    let open = dir.join("a.rs");
    let made = dir.join("made.rs");
    // Only the startup file exists on the daemon: `made.rs` does not (so the probe
    // says "create it"), and neither does the file the delete names (so it fails).
    let fake = DaemonFs::with(&open.to_string_lossy(), "let foo = 1\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), &open.to_string_lossy()).await;
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"]
    );

    // `create made.rs (ignoreIfExists)` — whose probe is enqueued — then a `delete` of
    // a file that isn't there *without* `ignoreIfNotExists`, which fails and aborts.
    let edit = format!(
        "nx.lsp.apply_workspace_edit({{ documentChanges = {{ \
        {{ kind = 'create', uri = 'file://{made}', options = {{ ignoreIfExists = true }} }}, \
        {{ textDocument = {{ uri = 'file://{made}', version = 0 }}, \
          edits = {{ {{ range = {{ start = {{ line = 0, character = 0 }}, \
                                   ['end'] = {{ line = 0, character = 0 }} }}, \
                       newText = 'fn created() {{}}\\n' }} }} }}, \
        {{ kind = 'delete', uri = 'file://{gone}' }} }} }})",
        made = made.display(),
        gone = dir.join("never-existed.rs").display(),
    );
    exec_lua(&rpc, &edit).await;

    // Give the probe, the failing delete and any write they might queue every chance
    // to land — the assertion is that nothing was written, so it has to be patient.
    for _ in 0..60 {
        exec_lua(&rpc, "return 1").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        fake.content(&made.to_string_lossy()),
        None,
        "the aborted edit must not write the file its create probe was still checking"
    );
}

/// A goto (`textDocument/definition` and friends, reached here through
/// `nx.lsp.show_document`, which is the same `jump_to_lsp_location`) into a file that
/// isn't open yet must land on the **column** the server named, not just its line —
/// off-tick as exactly as it does locally. The remote session is tier-1: a feature
/// that works locally has to work identically over the wire, and this one didn't.
///
/// Two ways it went wrong, both because the file's bytes are still crossing the wire
/// when the jump happens:
///
/// 1. The refinement pass ran anyway whenever the clamped cursor happened to agree
///    with the target line — which is every **line-0** target, the buffer being empty.
///    It read the column off an empty line (so: `0`) and *overwrote* the landing
///    target the first jump had recorded. A definition on line 0 landed on column 0.
/// 2. On the lines it did correctly leave alone, the recorded column was the raw
///    protocol `character` used as a byte offset. Exact for ASCII; wrong on any line
///    with a multi-byte character, under the utf-16 encoding the protocol defaults to.
///
/// One unopened file per case, because a jump *back* to a file the first case opened
/// would take the synchronous path and prove nothing.
#[tokio::test]
async fn a_goto_into_an_unopened_file_lands_on_the_column_off_tick() {
    let fake = DaemonFs::with_files(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/top.rs", "fn zero() {}\n"),
        ("/virtual/wide.rs", "// header\nlet héllo = world;\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"],
        "the open file should load over the daemon wire"
    );

    // Line 0, column 3 of a file that has never been opened — the case the clamped
    // refinement used to flatten to (1, 0).
    exec_lua(
        &rpc,
        "nx.lsp.show_document({ uri = 'file:///virtual/top.rs', \
         range = { start = { line = 0, character = 3 }, \
                   ['end'] = { line = 0, character = 3 } } })",
    )
    .await;
    assert_eq!(
        await_lines(&rpc, &["fn zero() {}"]).await,
        vec!["fn zero() {}"],
        "the jump should fetch and open the unopened file"
    );
    let mut landed = (0, 0);
    for _ in 0..80 {
        landed = cursor(&rpc).await;
        if landed == (1, 3) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        landed,
        (1, 3),
        "a line-0 target must keep its column across the deferred open"
    );

    // …and a utf-16 column on a line with a multi-byte character converts to the byte
    // column it actually names: in `let héllo = world;`, utf-16 8 is the `o` of
    // `héllo`, which is byte 9 — `é` costs one utf-16 unit and two bytes.
    exec_lua(
        &rpc,
        "nx.lsp.show_document({ uri = 'file:///virtual/wide.rs', \
         range = { start = { line = 1, character = 8 }, \
                   ['end'] = { line = 1, character = 8 } } })",
    )
    .await;
    assert_eq!(
        await_lines(&rpc, &["// header", "let héllo = world;"]).await,
        vec!["// header", "let héllo = world;"],
        "the second jump should fetch and open its file too"
    );
    let mut landed = (0, 0);
    for _ in 0..80 {
        landed = cursor(&rpc).await;
        if landed == (2, 9) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        landed,
        (2, 9),
        "a utf-16 column must be converted against the line that landed, not used raw"
    );
}
