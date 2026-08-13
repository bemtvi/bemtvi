//! `:connect` target parsing, the SSH-askpass prompt classifier, and the `bemtvi://`
//! connect-URI parser — pure helpers exercised like the GUI's other Tier-1 logic (no
//! window). The live SSH/QUIC hop and the window itself can't run headless, but the
//! parsing they ride is covered here.

use bemtvi_gui::parse_connect_uri;
use bemtvi_gui::remote::{
    connect_command, is_confirmation, is_connect_uri, workspace_command, ConnectTarget, RemoteSpec,
};

// --- SSH target parsing -----------------------------------------------------

#[test]
fn parses_user_host_port() {
    let spec = RemoteSpec::parse_target("david@myserver.com:5022").expect("remote target");
    assert_eq!(spec.user.as_deref(), Some("david"));
    assert_eq!(spec.host, "myserver.com");
    assert_eq!(spec.port, Some(5022));
    assert_eq!(spec.file, None);
}

#[test]
fn parses_embedded_remote_file_path() {
    let spec = RemoteSpec::parse_target("david@myserver.com:5022/home/work/test.rs")
        .expect("remote target");
    assert_eq!(spec.user.as_deref(), Some("david"));
    assert_eq!(spec.host, "myserver.com");
    assert_eq!(spec.port, Some(5022));
    // The leading slash is kept, so the absolute path stays absolute.
    assert_eq!(spec.file.as_deref(), Some("/home/work/test.rs"));
}

#[test]
fn parses_host_without_port_but_with_path() {
    let spec = RemoteSpec::parse_target("david@host/etc/hosts").expect("remote target");
    assert_eq!(spec.host, "host");
    assert_eq!(spec.port, None);
    assert_eq!(spec.file.as_deref(), Some("/etc/hosts"));
}

#[test]
fn parse_target_requires_a_host() {
    assert_eq!(RemoteSpec::parse_target(""), None);
    assert_eq!(RemoteSpec::parse_target("@host"), None);
    assert_eq!(RemoteSpec::parse_target("user@"), None);
}

#[test]
fn overflowing_port_falls_back_to_host() {
    // 99999 > u16::MAX isn't a usable port; keep the whole thing as the host rather than
    // silently dropping the colon-suffix.
    let spec = RemoteSpec::parse_target("david@host:99999").expect("remote target");
    assert_eq!(spec.host, "host:99999");
    assert_eq!(spec.port, None);
}

#[test]
fn rejects_dash_leading_host_or_user_to_block_ssh_flag_injection() {
    // A `host`/`user` starting with `-` would be smuggled to ssh as an option (e.g.
    // `-oProxyCommand=…`, an RCE vector) — reject it on both the spec and `:connect`.
    assert_eq!(RemoteSpec::parse_target("user@-oProxyCommand=evil"), None);
    assert_eq!(RemoteSpec::parse_target("-oProxyCommand=evil@host"), None);
    assert_eq!(RemoteSpec::parse_target("-lh:22"), None);
    assert_eq!(connect_command("connect -oProxyCommand=evil@host"), None);
}

// --- `:connect` command → ConnectTarget -------------------------------------

#[test]
fn connect_command_parses_ssh_target_with_optional_user() {
    // A full `user@host:port/file` is an SSH daemon target.
    let target =
        connect_command("connect user@server.com:5022/home/work/test.rs").expect("connect target");
    let ConnectTarget::Ssh(spec) = target else {
        panic!("expected an SSH target, got {target:?}");
    };
    assert_eq!(spec.user.as_deref(), Some("user"));
    assert_eq!(spec.port, Some(5022));
    assert_eq!(spec.file.as_deref(), Some("/home/work/test.rs"));

    // `:connect` intent is explicit, so a user-less target is accepted (ssh defaults it).
    let target = connect_command("connect server.com:5022").expect("connect target");
    let ConnectTarget::Ssh(spec) = target else {
        panic!("expected an SSH target");
    };
    assert_eq!(spec.user, None);
    assert_eq!(spec.host, "server.com");
    assert_eq!(spec.port, Some(5022));
}

#[test]
fn connect_command_parses_bemtvi_uri_as_quic() {
    let target = connect_command("connect bemtvi://127.0.0.1:8765/tok123?cert=abcd")
        .expect("connect target");
    assert_eq!(
        target,
        ConnectTarget::Quic("bemtvi://127.0.0.1:8765/tok123?cert=abcd".into())
    );
}

#[test]
fn connect_command_does_not_accept_nvim_shorthand() {
    // Only the canonical `bemtvi://` scheme is a QUIC target; a bare `nvim://…` is not a
    // valid SSH `[user@]host` either (no `@`, a `:` with non-digits), so it's rejected.
    assert_eq!(
        connect_command("connect nvim://127.0.0.1:8765/tok?cert=x"),
        None
    );
}

#[test]
fn connect_command_rejects_other_commands() {
    assert_eq!(connect_command("w"), None);
    assert_eq!(connect_command("wq"), None);
    assert_eq!(connect_command("connection foo"), None);
    // `:connect` with no target isn't actionable.
    assert_eq!(connect_command("connect"), None);
}

#[test]
fn embedded_file_only_from_ssh_targets() {
    let ssh = connect_command("connect host:22/a/b.rs").unwrap();
    assert_eq!(ssh.embedded_file().as_deref(), Some("/a/b.rs"));
    let quic = connect_command("connect bemtvi://h:1/t?cert=c").unwrap();
    assert_eq!(quic.embedded_file(), None);
}

// --- `:workspace` command → directory ---------------------------------------

#[test]
fn workspace_command_parses_directory_argument() {
    assert_eq!(
        workspace_command("workspace /home/me/project").as_deref(),
        Some("/home/me/project")
    );
    // A relative path is taken verbatim (canonicalized when the session is built).
    assert_eq!(workspace_command("workspace src").as_deref(), Some("src"));
    // The `:Workspace` capitalization is accepted too, like `:Connect`.
    assert_eq!(workspace_command("Workspace /tmp").as_deref(), Some("/tmp"));
}

#[test]
fn workspace_command_with_no_argument_targets_cwd() {
    // A bare `:workspace` (or trailing whitespace only) opens the current directory,
    // matching the TUI's `bemtvi --workspace`.
    assert_eq!(workspace_command("workspace").as_deref(), Some("."));
    assert_eq!(workspace_command("workspace   ").as_deref(), Some("."));
}

#[test]
fn workspace_command_keeps_paths_with_spaces() {
    // The whole remainder is the path (trimmed), so a directory name with spaces works.
    assert_eq!(
        workspace_command("workspace /home/me/My Project").as_deref(),
        Some("/home/me/My Project")
    );
}

#[test]
fn workspace_command_rejects_other_commands() {
    assert_eq!(workspace_command("w"), None);
    assert_eq!(workspace_command("write"), None);
    // The command word must end at `workspace` — `workspacefoo` is something else.
    assert_eq!(workspace_command("workspacefoo"), None);
    assert_eq!(workspace_command("connect host:22"), None);
}

// --- `bemtvi://` URI detection + parsing -------------------------------------

#[test]
fn is_connect_uri_only_matches_bemtvi_scheme() {
    assert!(is_connect_uri("bemtvi://h:1/t?cert=c"));
    assert!(!is_connect_uri("nvim://h:1/t?cert=c"));
    assert!(!is_connect_uri("user@host:22"));
}

#[test]
fn parse_connect_uri_extracts_url_token_and_cert() {
    let (url, cert, token) =
        parse_connect_uri("bemtvi://127.0.0.1:8765/tok123?cert=deadbeef").expect("valid URI");
    // WebTransport requires the `https` scheme on the dial URL.
    assert_eq!(url, "https://127.0.0.1:8765");
    assert_eq!(token, "tok123");
    assert_eq!(cert, "deadbeef");
}

#[test]
fn parse_connect_uri_rejects_malformed() {
    // Wrong scheme, missing cert query, empty pieces — each fails loud rather than
    // dialing a half-specified target.
    //
    // A URI with no `/TOKEN` is *not* malformed: that is the form the daemon now
    // prints for a native connect, with the bearer token on
    // `$BEMTVI_DAEMON_TOKEN` instead of in a string that lands in argv. So these
    // run with that variable explicitly cleared — otherwise this test would pass
    // or fail depending on whether the developer running it happens to have a
    // daemon token exported, which the daemon's own connect command tells them to
    // do. (`bemtvi-server/tests/daemon_token.rs` covers the token resolution
    // itself, both directions.)
    let restore = std::env::var(bemtvi_server::DAEMON_TOKEN_ENV).ok();
    // SAFETY: this test binary is single-threaded at this point, and no other test
    // here reads the environment.
    std::env::remove_var(bemtvi_server::DAEMON_TOKEN_ENV);

    assert!(parse_connect_uri("https://h/t?cert=c").is_err());
    assert!(parse_connect_uri("bemtvi://h:1").is_err()); // no ?cert=
    assert!(parse_connect_uri("bemtvi://h:1/tok").is_err()); // no ?cert=
    assert!(parse_connect_uri("bemtvi:///tok?cert=c").is_err()); // no HOST:PORT
    assert!(parse_connect_uri("bemtvi://h:1/tok?cert=").is_err()); // empty cert
                                                                   // No token in the path and none on the environment: refused, rather than
                                                                   // dialing unauthenticated.
    assert!(parse_connect_uri("bemtvi://h:1/?cert=c").is_err());
    assert!(parse_connect_uri("bemtvi://h:1?cert=c").is_err());

    if let Some(v) = restore {
        std::env::set_var(bemtvi_server::DAEMON_TOKEN_ENV, v);
    }
}

// --- SSH askpass prompt classifier ------------------------------------------

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
