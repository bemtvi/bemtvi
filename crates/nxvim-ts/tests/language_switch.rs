//! Regression: re-opening a buffer under a language with **no grammar** must
//! drop the previous language's parse state, not keep painting it.
//!
//! `Engine::open` is also the language-*switch* path (`:set filetype=`,
//! `:w other.ext`): the editor drops its `syntax_opened` marker and re-opens the
//! buffer under the new language. When the new language had no installed grammar
//! the engine returned early and left the old `BufferState` in place, so the
//! buffer kept its stale highlights — and kept *updating* them incrementally —
//! while the editor believed highlighting was off.
//!
//! Hermetic: compiles the rust grammar out of the cargo registry (no network);
//! `NXVIM_DATA_DIR` pins the search path to the fixture dir (suppressing any
//! borrowed neovim `site/` roots on the host).

mod fixture;

use fixture::{install_rust_grammar, TempDir};
use nxvim_core::{BufferId, OpenOutcome};
use nxvim_ts::Engine;

#[test]
fn switching_to_a_grammarless_language_drops_the_stale_highlights() {
    let data = TempDir::new("lang_switch");
    install_rust_grammar(&data.0);
    // Hermetic: pin the engine's search path to the fixture (no extra roots).
    std::env::set_var("NXVIM_DATA_DIR", &data.0);

    let mut engine = Engine::new(data.0.clone());
    let buf = BufferId(1);
    let text = "fn main() {}\n";

    // Opened as rust, the buffer highlights.
    assert!(matches!(engine.open(buf, "rust", text), OpenOutcome::Ok));
    assert!(
        !engine.highlights(buf, 0, 1).is_empty(),
        "the rust grammar fixture should paint `fn main() {{}}`"
    );

    // Switch the buffer to a language with no installed grammar (what a
    // `:set filetype=` re-open feeds the engine). This must be silent (Ok) —
    // and must *forget* the rust parse state.
    assert!(matches!(
        engine.open(buf, "nxvim_no_such_lang", text),
        OpenOutcome::Ok
    ));
    assert_eq!(
        engine.language_of(buf),
        None,
        "a buffer re-opened under a grammarless language must not stay \
         registered as its previous language"
    );
    let spans = engine.highlights(buf, 0, 1);
    assert!(
        spans.is_empty(),
        "a buffer re-opened under a grammarless language must stop painting \
         the previous language's highlights; got {spans:?}"
    );
}
