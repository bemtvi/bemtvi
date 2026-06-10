//! The SSH target / `:connect` command parsing — pure helpers exercised like the
//! GUI's other Tier-1 logic (no window). The live SSH hop and the window itself
//! can't run headless (see `docs/plans/2026-06-09-remote-ssh-client.md`).

use nxvim_gui::remote::{connect_command, is_confirmation, RemoteSpec};

#[test]
fn parses_user_host_port() {
    let spec = RemoteSpec::parse("david@myserver.com:5022").expect("remote target");
    assert_eq!(spec.user.as_deref(), Some("david"));
    assert_eq!(spec.host, "myserver.com");
    assert_eq!(spec.port, Some(5022));
    assert_eq!(spec.file, None);
}

#[test]
fn parses_embedded_remote_file_path() {
    let spec =
        RemoteSpec::parse("david@myserver.com:5022/home/work/test.rs").expect("remote target");
    assert_eq!(spec.user.as_deref(), Some("david"));
    assert_eq!(spec.host, "myserver.com");
    assert_eq!(spec.port, Some(5022));
    // The leading slash is kept, so the absolute path stays absolute.
    assert_eq!(spec.file.as_deref(), Some("/home/work/test.rs"));
}

#[test]
fn parses_host_without_port_but_with_path() {
    let spec = RemoteSpec::parse("david@host/etc/hosts").expect("remote target");
    assert_eq!(spec.host, "host");
    assert_eq!(spec.port, None);
    assert_eq!(spec.file.as_deref(), Some("/etc/hosts"));
}

#[test]
fn cli_parse_rejects_non_remote_args() {
    // No `@`: an ordinary file argument opens locally, not over SSH.
    assert_eq!(RemoteSpec::parse("notes.txt"), None);
    assert_eq!(RemoteSpec::parse("/abs/path/file.rs"), None);
}

#[test]
fn cli_parse_skips_an_existing_local_path_even_with_at() {
    // A real file whose name contains `@` still opens locally.
    let dir = std::env::temp_dir().join(format!("nxvim_remote_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("git@notes");
    std::fs::write(&path, "x").unwrap();
    assert_eq!(RemoteSpec::parse(&path.to_string_lossy()), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overflowing_port_falls_back_to_host() {
    // 99999 > u16::MAX isn't a usable port; keep the whole thing as the host
    // rather than silently dropping the colon-suffix.
    let spec = RemoteSpec::parse("david@host:99999").expect("remote target");
    assert_eq!(spec.host, "host:99999");
    assert_eq!(spec.port, None);
}

#[test]
fn with_file_overrides_only_when_present() {
    let base = RemoteSpec::parse("david@host:22/embedded.rs").unwrap();
    // An explicit second positional wins.
    assert_eq!(
        base.clone()
            .with_file(Some("explicit.rs".into()))
            .file
            .as_deref(),
        Some("explicit.rs")
    );
    // No positional keeps the embedded path.
    assert_eq!(base.with_file(None).file.as_deref(), Some("/embedded.rs"));
}

#[test]
fn connect_command_parses_target_with_optional_user() {
    let spec =
        connect_command("connect user@server.com:5022/home/work/test.rs").expect("connect target");
    assert_eq!(spec.user.as_deref(), Some("user"));
    assert_eq!(spec.port, Some(5022));
    assert_eq!(spec.file.as_deref(), Some("/home/work/test.rs"));

    // `:connect` intent is explicit, so a user-less target is accepted (unlike the
    // CLI's first-positional heuristic, which requires `@`).
    let spec = connect_command("connect server.com:5022").expect("connect target");
    assert_eq!(spec.user, None);
    assert_eq!(spec.host, "server.com");
    assert_eq!(spec.port, Some(5022));
}

#[test]
fn rejects_dash_leading_host_or_user_to_block_ssh_flag_injection() {
    // A `host`/`user` starting with `-` would be smuggled to ssh as an option
    // (e.g. `-oProxyCommand=…`, an RCE vector) — reject it on both paths.
    assert_eq!(RemoteSpec::parse("user@-oProxyCommand=evil"), None);
    assert_eq!(RemoteSpec::parse_target("-oProxyCommand=evil@host"), None);
    assert_eq!(RemoteSpec::parse_target("-lh:22"), None);
    assert_eq!(connect_command("connect -oProxyCommand=evil@host"), None);
}

#[test]
fn askpass_classifies_host_key_confirmation_vs_secret() {
    // ssh's host-key prompt is a yes/no confirmation (→ a Yes/No dialog)…
    assert!(is_confirmation(
        "The authenticity of host 'h (1.2.3.4)' can't be established.\n\
         ED25519 key fingerprint is SHA256:abc.\n\
         Are you sure you want to continue connecting (yes/no/[fingerprint])? "
    ));
    // …while passwords and key passphrases are secrets to type (→ a masked input).
    assert!(!is_confirmation("user@host's password: "));
    assert!(!is_confirmation(
        "Enter passphrase for key '/home/me/.ssh/id_ed25519': "
    ));
}

#[test]
fn connect_command_rejects_other_commands() {
    assert_eq!(connect_command("w"), None);
    assert_eq!(connect_command("wq"), None);
    assert_eq!(connect_command("connection foo"), None);
    // `:connect` with no target isn't actionable.
    assert_eq!(connect_command("connect"), None);
}
