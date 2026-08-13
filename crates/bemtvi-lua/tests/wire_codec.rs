//! The daemon/wasm wire codecs, driven as the public functions the legs call.
//!
//! These sit at a **first-party** boundary: the peer encoding these frames is our own
//! daemon (or the edit-host), so a field that is present but the wrong shape is a bug
//! in code we ship — not hostile input to be tolerated. The codecs used to decode such
//! a field to its default, which turned a bug into a silent wrong answer: a mangled
//! `read` body resolved as a 0-byte success, a `recursive = true` became a
//! non-recursive remove, a truncated `stat` reported size 0 and mode 0.
//!
//! The rule these tests pin down has two halves, and both matter:
//!
//!  * an **absent** optional field keeps its default — an older peer that predates a
//!    flag must keep working;
//!  * a **present but wrong-typed** field fails loud, naming what was wrong.
//!
//! Tier-1 and pure: no server, no wire, no toolkit. The malformed frames a real peer
//! cannot produce are built here by hand, which is the only way to reach these paths —
//! our own encoder never emits them.

use bemtvi_lua::{
    fs_job_from_value, fs_job_to_value, fs_result_from_value, git_job_from_value,
    http_request_from_value, http_result_from_value,
};
use bemtvi_lua::{FsJob, LuaStat};
use rmpv::Value;

/// The `["ok", <payload>]` envelope every reply frame is wrapped in.
fn ok_reply(payload: Value) -> Value {
    Value::Array(vec![Value::from("ok"), payload])
}

/// A msgpack map from `(key, value)` pairs — the shape every op frame takes.
fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

// ============================================================== fs op requests

#[test]
fn an_absent_optional_flag_keeps_its_default() {
    // An older peer that predates `recursive` sends no key at all: that is the
    // pre-flag behaviour, not an error.
    let job = fs_job_from_value(&map(&[
        ("op", Value::from("remove")),
        ("path", Value::from("/tmp/x")),
    ]))
    .expect("an absent optional flag is legal");
    match job {
        FsJob::Remove { recursive, .. } => assert!(!recursive, "defaults to false"),
        other => panic!("expected a remove job, got {other:?}"),
    }
}

#[test]
fn a_wrong_typed_flag_fails_loud_instead_of_silently_defaulting() {
    // The failure this replaces: `recursive = "true"` (a string) decoded to `false`,
    // so `btv.fs.remove(dir, { recursive = true })` quietly did not recurse and the
    // caller saw an ENOTEMPTY it could not explain.
    let err = fs_job_from_value(&map(&[
        ("op", Value::from("remove")),
        ("path", Value::from("/tmp/x")),
        ("recursive", Value::from("true")),
    ]))
    .expect_err("a present but wrong-typed flag is malformed");
    assert!(
        err.contains("recursive") && err.contains("bool"),
        "the error must name the field and what was wrong: {err}"
    );
}

#[test]
fn a_wrong_typed_mode_fails_loud() {
    let err = fs_job_from_value(&map(&[
        ("op", Value::from("mkdir")),
        ("path", Value::from("/tmp/x")),
        ("mode", Value::from("0755")),
    ]))
    .expect_err("a string mode is malformed");
    assert!(err.contains("mode"), "must name the field: {err}");
}

#[test]
fn an_out_of_range_mode_fails_loud_rather_than_truncating() {
    let err = fs_job_from_value(&map(&[
        ("op", Value::from("mkdir")),
        ("path", Value::from("/tmp/x")),
        ("mode", Value::from(u64::MAX)),
    ]))
    .expect_err("a mode past u32 is malformed");
    assert!(err.contains("u32"), "must name the range: {err}");
}

#[test]
fn a_wrong_typed_encoding_fails_loud() {
    let err = fs_job_from_value(&map(&[
        ("op", Value::from("read_text")),
        ("path", Value::from("/tmp/x")),
        ("encoding", Value::from(7u64)),
    ]))
    .expect_err("a numeric encoding is malformed");
    assert!(err.contains("encoding"), "must name the field: {err}");
}

#[test]
fn an_absent_encoding_still_defaults_to_utf8() {
    let job = fs_job_from_value(&map(&[
        ("op", Value::from("read_text")),
        ("path", Value::from("/tmp/x")),
    ]))
    .expect("an absent encoding is legal");
    match job {
        FsJob::ReadText { encoding, .. } => assert_eq!(encoding, "utf-8"),
        other => panic!("expected read_text, got {other:?}"),
    }
}

#[test]
fn a_write_whose_data_is_not_bytes_fails_loud() {
    // The sharpest one: decoding this to empty bytes TRUNCATES THE FILE and reports
    // success. The caller is told the write worked.
    let err = fs_job_from_value(&map(&[
        ("op", Value::from("write")),
        ("path", Value::from("/tmp/x")),
        ("data", Value::from(42u64)),
    ]))
    .expect_err("a non-byte payload is malformed");
    assert!(
        err.contains("byte") || err.contains("expected"),
        "must say what was expected: {err}"
    );
}

#[test]
fn a_well_formed_job_still_round_trips() {
    // The control for every strictness test above: the encoder's own output must
    // decode, or the hardening broke the ordinary path.
    for job in [
        FsJob::Read {
            path: "/tmp/a".into(),
        },
        FsJob::Write {
            path: "/tmp/a".into(),
            data: vec![1, 2, 3],
        },
        FsJob::Mkdir {
            path: "/tmp/a".into(),
            recursive: true,
            mode: 0o700,
        },
        FsJob::Remove {
            path: "/tmp/a".into(),
            recursive: true,
        },
        FsJob::ReadText {
            path: "/tmp/a".into(),
            encoding: "latin1".into(),
        },
    ] {
        let wire = fs_job_to_value(&job);
        let back = fs_job_from_value(&wire).expect("our own encoding must decode");
        assert_eq!(format!("{back:?}"), format!("{job:?}"), "round trip");
    }
}

// ============================================================== fs op replies

#[test]
fn a_read_reply_with_a_mangled_body_fails_instead_of_reading_empty() {
    // The audit's headline: a `bytes` payload that is not bytes used to decode to
    // `Vec::new()`, so `btv.fs.read` RESOLVED with an empty file. A caller that then
    // wrote it back would have destroyed the file's contents.
    let err = fs_result_from_value(&ok_reply(Value::Array(vec![
        Value::from("bytes"),
        Value::from(true),
    ])))
    .expect_err("a mangled body must not resolve as a 0-byte success");
    assert!(
        format!("{err:?}").to_lowercase().contains("byte"),
        "the error must name the payload: {err:?}"
    );
}

#[test]
fn a_reply_with_no_payload_at_all_fails() {
    for kind in ["bool", "bytes", "text", "stat", "dir"] {
        let _ = fs_result_from_value(&ok_reply(Value::Array(vec![Value::from(kind)])))
            .unwrap_err_or_panic(kind);
    }
}

#[test]
fn a_truncated_stat_fails_instead_of_reporting_zeroes() {
    // A short stat row used to zero-fill: size 0, mode 0, no mtime. Every consumer
    // then believes the file is empty and unreadable.
    let err = fs_result_from_value(&ok_reply(Value::Array(vec![
        Value::from("stat"),
        Value::Array(vec![Value::from("file")]),
    ])))
    .expect_err("a truncated stat is malformed");
    assert!(
        format!("{err:?}").contains("stat"),
        "must name what failed: {err:?}"
    );
}

#[test]
fn an_unknown_file_kind_fails_instead_of_becoming_a_plain_file() {
    // The encoder emits a closed vocabulary, so an unknown kind is a first-party
    // bug. Decoding it as `file` makes a directory look like a file to the caller.
    let err = fs_result_from_value(&ok_reply(Value::Array(vec![
        Value::from("dir"),
        Value::Array(vec![Value::Array(vec![
            Value::from("socket"),
            Value::from("a"),
        ])]),
    ])))
    .expect_err("an unknown dirent kind is malformed");
    assert!(
        format!("{err:?}").contains("socket"),
        "the error must name the offending kind: {err:?}"
    );
}

#[test]
fn a_well_formed_stat_and_listing_still_decode() {
    // The control: the real encoder's shapes must survive the strictness.
    let stat = bemtvi_lua::fs_result_to_value(&Ok(bemtvi_lua::FsValue::Stat(LuaStat {
        kind: bemtvi_lua::FileKind::File,
        size: 12,
        mode: 0o644,
        mtime: Some((1, 2)),
        atime: None,
        ino: 3,
        uid: 4,
        gid: 5,
        nlink: 6,
        dev: 7,
    })));
    let back = fs_result_from_value(&stat).expect("our own stat encoding must decode");
    match back {
        bemtvi_lua::FsValue::Stat(s) => {
            assert_eq!(s.size, 12);
            assert_eq!(s.mtime, Some((1, 2)));
            assert_eq!(s.atime, None, "an absent time stays absent");
        }
        other => panic!("expected a stat, got {other:?}"),
    }
}

// ================================================================= git op requests

#[test]
fn a_wrong_typed_git_flag_fails_loud() {
    let err = git_job_from_value(&map(&[
        ("op", Value::from("checkout")),
        ("dir", Value::from("/tmp/r")),
        ("rev", Value::from("main")),
        ("detach", Value::from(1u64)),
    ]))
    .expect_err("a numeric `detach` is malformed");
    assert!(
        err.contains("detach"),
        "the error must name the flag: {err}"
    );
}

#[test]
fn an_out_of_range_clone_depth_fails_instead_of_truncating() {
    // Truncating here silently changes the shallow depth the caller asked for.
    let err = git_job_from_value(&map(&[
        ("op", Value::from("clone")),
        ("url", Value::from("https://x/y")),
        ("dir", Value::from("/tmp/r")),
        ("depth", Value::from(u64::MAX)),
    ]))
    .expect_err("a depth past u32 is malformed");
    assert!(err.contains("u32"), "must name the range: {err}");
}

#[test]
fn an_absent_git_flag_keeps_the_pre_flag_behaviour() {
    let job = git_job_from_value(&map(&[
        ("op", Value::from("status")),
        ("path", Value::from("/tmp/r")),
    ]))
    .expect("an older peer sends no `ignored` key");
    assert!(
        format!("{job:?}").contains("ignored: false"),
        "absent means the default: {job:?}"
    );
}

// ================================================================ http op frames

#[test]
fn a_wrong_typed_http_method_fails_loud() {
    let err = http_request_from_value(&map(&[
        ("url", Value::from("https://x/y")),
        ("method", Value::from(7u64)),
    ]))
    .expect_err("a numeric method is malformed");
    assert!(err.contains("method"), "must name the field: {err}");
}

#[test]
fn a_malformed_header_row_fails_instead_of_being_skipped() {
    // Skipping the row silently DROPS a header — an Authorization the caller set is
    // simply not sent, and the request comes back 401 with no explanation.
    let err = http_request_from_value(&map(&[
        ("url", Value::from("https://x/y")),
        (
            "headers",
            Value::Array(vec![Value::Array(vec![Value::from("Accept")])]),
        ),
    ]))
    .expect_err("a header row without a value is malformed");
    assert!(err.contains("header"), "must name the field: {err}");
}

#[test]
fn an_http_reply_missing_its_body_fails_instead_of_resolving_empty() {
    let err = http_result_from_value(&ok_reply(map(&[
        ("status", Value::from(200u64)),
        ("status_text", Value::from("OK")),
        ("headers", Value::Array(vec![])),
    ])))
    .expect_err("a reply with no body is malformed");
    assert!(
        format!("{err:?}").contains("body"),
        "must name what was missing: {err:?}"
    );
}

#[test]
fn an_absent_http_option_still_takes_its_default() {
    // The control: only `url` is required; everything else defaults.
    let req = http_request_from_value(&map(&[("url", Value::from("https://x/y"))]))
        .expect("a bare url is a legal request");
    assert_eq!(req.method, "GET");
    assert!(req.headers.is_empty());
    assert!(req.body.is_empty());
}

/// Small helper so the "no payload" sweep reads as one assertion per kind.
trait UnwrapErrOrPanic<T, E> {
    fn unwrap_err_or_panic(self, what: &str) -> E;
}
impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<T, E> for Result<T, E> {
    fn unwrap_err_or_panic(self, what: &str) -> E {
        match self {
            Err(e) => e,
            Ok(v) => panic!("a `{what}` reply with no payload must fail, got {v:?}"),
        }
    }
}
