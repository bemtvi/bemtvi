//! A language *alias* — a markdown fence's info string, or a filetype — must
//! resolve to the grammar nxvim actually installs for it.
//!
//! Reported: a ```` ```jsonc ```` block in a markdown buffer stayed plain (dropping
//! the `c` highlighted it), and ```` ```sh ```` never highlighted as shell. The
//! injection resolver normalized the info string (trim / lowercase / `-`→`_`) and
//! then looked for a grammar under that literal name — but nxvim ships no `jsonc`
//! or `sh` grammar: those are *aliases* of `json` and `bash`, exactly as the
//! extension table already declares for `foo.jsonc` / `foo.sh`. The same gap hit
//! `Engine::open`, so `:set filetype=sh` painted nothing either.
//!
//! Hermetic: the fixture grammars (rust + markdown) come out of the cargo registry,
//! so the *behavioral* half is proved with the `rs` → `rust` alias — the same
//! resolution the reported `jsonc` / `sh` / `cs` aliases take. Those three have no
//! registry grammar to compile here, so they are asserted at the resolver itself.

mod fixture;

use fixture::{install_markdown_grammar, install_rust_grammar, TempDir};
use nxvim_core::{resolve_language, BufferId, OpenOutcome};
use nxvim_ts::Engine;

/// The injection path: a fence tagged with an alias (`rs`) must inject the aliased
/// grammar (`rust`), just as ```` ```rust ```` does. `keyword` can only come from
/// the injected rust layer — markdown's highlights query has no such capture.
#[test]
fn a_fence_info_string_alias_injects_the_aliased_grammar() {
    let data = TempDir::new("lang_alias_fence");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    let buf = BufferId(1);
    let text = "```rs\nfn f() {}\n```\n";
    assert!(matches!(
        engine.open(buf, "markdown", text),
        OpenOutcome::Ok
    ));

    let spans = engine.highlights(buf, 0, 4);
    assert!(
        spans.iter().any(|s| s.group == "keyword" && s.line == 1),
        "a ```rs fence must inject the rust grammar and paint `fn` as keyword; got {spans:?}"
    );
}

/// The stateless preview twin (`highlight_text`, behind the picker preview and the
/// markdown doc float) resolves the same alias.
#[test]
fn highlight_text_resolves_a_fence_info_string_alias() {
    let data = TempDir::new("lang_alias_preview");
    install_rust_grammar(&data.0);
    install_markdown_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    let spans = engine.highlight_text("markdown", "```rs\nfn f() {}\n```\n", 0, 4);
    assert!(
        spans.iter().any(|s| s.group == "keyword" && s.line == 1),
        "highlight_text must inject rust for a ```rs fence; got {spans:?}"
    );
}

/// The buffer path: opening under an alias (what `:set filetype=rs` — or `sh`,
/// `jsonc` — feeds the engine) highlights with the aliased grammar, and the buffer
/// is registered under the *resolved* language.
#[test]
fn opening_a_buffer_under_a_language_alias_highlights() {
    let data = TempDir::new("lang_alias_open");
    install_rust_grammar(&data.0);
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    let buf = BufferId(1);
    assert!(matches!(
        engine.open(buf, "rs", "fn main() {}\n"),
        OpenOutcome::Ok
    ));
    assert_eq!(
        engine.language_of(buf),
        Some("rust"),
        "a buffer opened under an alias must register as the resolved grammar"
    );
    assert!(
        !engine.highlights(buf, 0, 1).is_empty(),
        "a buffer opened as `rs` must highlight as rust"
    );
}

/// The reported aliases at the resolver: `jsonc` *is* json, `sh` *is* bash, `cs`
/// *is* c_sharp — the same answers the extension table gives for `foo.jsonc` /
/// `foo.sh` / `foo.cs`. An unknown name and a real grammar name both pass through
/// untouched, so a grammar the table never lists still loads under its own name.
#[test]
fn the_reported_aliases_resolve_to_their_grammars() {
    for (alias, grammar) in [
        ("jsonc", "json"),
        ("sh", "bash"),
        ("cs", "c_sharp"),
        ("shell", "bash"),
        ("py", "python"),
        ("ts", "typescript"),
        ("csharp", "c_sharp"),
        ("golang", "go"),
        // Grammar names pass through unchanged...
        ("json", "json"),
        ("bash", "bash"),
        ("c_sharp", "c_sharp"),
        // ...as does a name nxvim's tables don't know at all (it may still be an
        // installed grammar — resolution must never lose it).
        ("nxvim_no_such_lang", "nxvim_no_such_lang"),
    ] {
        assert_eq!(
            resolve_language(alias),
            grammar,
            "resolve_language({alias:?})"
        );
    }
}
