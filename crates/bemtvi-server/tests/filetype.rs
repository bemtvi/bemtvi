//! Filetype **detection** — how a buffer gets a `'filetype'` when nobody set one.
//!
//! The rules run most-specific first (`bemtvi_core::language_of_path`): the exact
//! filename, then a path pattern, then the extension, and — only when the path says
//! nothing — the buffer's own `#!` line. These tests drive the real thing end to end:
//! open a file, ask `:set filetype?` what the editor decided.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    command, drain_to_latest_redraw, exec_lua, message, start_attached, temp_dir,
};
use std::path::Path;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Write `name` (a bare filename, or a `dir/name` relative path) under a fresh temp
/// dir with `content`, open it, and return what the editor decided its filetype is.
/// The whole detection stack is exercised — no `:setf`, no config.
async fn detect(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    name: &str,
    content: &str,
) -> String {
    let dir = temp_dir("filetype");
    let path = dir.join(name);
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("fixture dir");
    }
    std::fs::write(&path, content).expect("fixture file");
    command(rpc, &format!("edit {}", path.display())).await;
    filetype(rpc, incoming).await
}

/// The current buffer's `'filetype'`, read the way a user would (`:set filetype?`)
/// with the `filetype=` prefix stripped. `""` means "no filetype".
async fn filetype(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    command(rpc, "set filetype?").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(incoming, |_| true).expect("a redraw arrived");
    message(&frame)
        .strip_prefix("filetype=")
        .expect("a filetype readout")
        .to_string()
}

/// A shell startup file has no extension at all — `Path::extension` returns `None`
/// for a leading-dot name — so an extension table can never type it. The filename
/// table is what makes `.bashrc` open as bash instead of as nothing.
#[tokio::test]
async fn a_shell_dotfile_is_typed_by_its_filename() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, ".bashrc", "alias l=ls\n").await,
        "bash"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, ".zshrc", "alias l=ls\n").await,
        "zsh"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, ".profile", "export A=1\n").await,
        "bash"
    );
}

/// `Makefile` is the other half of the same gap, and it also reaches a filetype no
/// extension produced before: `make` was an installable grammar with nothing mapped
/// to it.
#[tokio::test]
async fn a_makefile_is_typed_by_its_filename() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, "Makefile", "all:\n\techo hi\n").await,
        "make"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, "GNUmakefile", "all:\n").await,
        "make"
    );
    // ...and the extension spellings resolve to the same filetype.
    assert_eq!(
        detect(&rpc, &mut incoming, "rules.mk", "all:\n").await,
        "make"
    );
}

/// A filetype detection can produce must also be one `:setfiletype <Tab>` offers.
/// `make` / `git_config` / `gomod` are reachable only by filename, so the completion
/// catalog has to be the union of the tables, not the extension table alone.
#[tokio::test]
async fn filename_only_filetypes_are_offered_by_setfiletype_completion() {
    let (rpc, _incoming) = start().await;
    let missing = exec_lua(
        &rpc,
        r#"
        local have = {}
        for _, ft in ipairs(btv._filetypes or {}) do have[ft] = true end
        local missing = {}
        for _, ft in ipairs({ "make", "git_config", "gomod", "requirements", "lua" }) do
          if not have[ft] then missing[#missing + 1] = ft end
        end
        return table.concat(missing, ",")
        "#,
    )
    .await;
    assert_eq!(
        missing.as_str(),
        Some(""),
        "every detectable filetype must be completable"
    );
}

/// A fixed name can't express a suffixed variant, which is what the pattern table is
/// for: `.env.local` and `Dockerfile.dev` are as common as the bare names.
#[tokio::test]
async fn a_path_pattern_types_a_suffixed_variant() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, ".env.local", "A=1\n").await,
        "bash"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, "Dockerfile.dev", "FROM alpine\n").await,
        "dockerfile"
    );
}

/// Precedence, where it is actually observable: `Dockerfile.lua` matches the
/// `Dockerfile.*` pattern *and* has a known extension. The more specific rule wins,
/// so it is a dockerfile — flip the order in `language_of_path` and this says `lua`.
#[tokio::test]
async fn a_path_pattern_beats_the_extension() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, "Dockerfile.lua", "FROM alpine\n").await,
        "dockerfile"
    );
}

/// A directory-anchored pattern must need its directory: `config` is a filetype
/// under `.ssh/` and nothing at all anywhere else. This is the guard against the
/// pattern table quietly over-matching.
#[tokio::test]
async fn a_directory_anchored_pattern_needs_its_directory() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, ".ssh/config", "Host *\n").await,
        "ssh_config"
    );
    assert_eq!(detect(&rpc, &mut incoming, "config", "Host *\n").await, "");
}

/// The case no path rule can ever reach: an executable script with no extension and
/// a name that means nothing. Only the content says what it is.
#[tokio::test]
async fn a_shebang_types_an_extensionless_script() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(
            &rpc,
            &mut incoming,
            "deploy",
            "#!/usr/bin/env python3\nprint(1)\n"
        )
        .await,
        "python"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, "build", "#!/bin/bash -eu\necho hi\n").await,
        "bash"
    );
    // `env -S` (and any other flag or `VAR=value` assignment) is skipped to find the
    // real interpreter — the spelling that makes a multi-word shebang portable.
    assert_eq!(
        detect(
            &rpc,
            &mut incoming,
            "serve",
            "#!/usr/bin/env -S node --watch\n"
        )
        .await,
        "javascript"
    );
    // A versioned interpreter resolves through its unversioned row.
    assert_eq!(
        detect(&rpc, &mut incoming, "report", "#!/usr/bin/python3.12\n").await,
        "python"
    );
}

/// The shebang is the *last* resort, not an override: a `.lua` file whose first line
/// is `#!/bin/sh` is lua. (Reverse the order and this says `bash`.)
#[tokio::test]
async fn the_path_beats_the_shebang() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, "tool.lua", "#!/bin/sh\nprint(1)\n").await,
        "lua"
    );
}

/// Every rule here is a *default*. An explicit `:setfiletype` — the escape hatch, and
/// what a config's own `BufReadPost` handler writes — still wins over all of them.
#[tokio::test]
async fn an_explicit_filetype_beats_every_rule() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, "Makefile", "all:\n").await,
        "make"
    );
    command(&rpc, "setfiletype json").await;
    assert_eq!(filetype(&rpc, &mut incoming).await, "json");
}

/// A config that had to work around a missing extension shouldn't have to: `.jade` is
/// pug, and an extension nothing claims still resolves to no filetype (the guard that
/// none of these tables started matching everything).
#[tokio::test]
async fn a_missing_extension_row_is_filled_and_unknown_files_stay_untyped() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(
        detect(&rpc, &mut incoming, "page.jade", "doctype html\n").await,
        "pug"
    );
    assert_eq!(
        detect(&rpc, &mut incoming, "notes.qqzz", "hello\n").await,
        ""
    );
    assert_eq!(
        detect(&rpc, &mut incoming, "notes.txt", "hello\n").await,
        ""
    );
}
