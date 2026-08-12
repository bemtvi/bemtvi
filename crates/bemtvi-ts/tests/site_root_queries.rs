//! Regression: for a grammar resolved from a read-only **fallback root** (an
//! existing neovim `site/` — the "borrowed nvim-treesitter install" path), the
//! engine's disk-query reads must use *that* root, not the writable data dir.
//!
//! `Engine::grammar()` picks the first search root that has the parser and loads
//! the queries from it, but `read_disk_query` — behind `base_query` (what the
//! server's runtimepath query-resolution bridge merges overlays onto),
//! `set_query_overlay`'s disk comparison, and `set_query`'s revert-on-clear —
//! hardcoded `data_dir`. For a site-root grammar `base_query` therefore returned
//! `None`: the bridge saw "no base highlights query", so any `after/queries`
//! `;; extends` overlay replaced the whole base instead of extending it, and
//! clearing a highlights override errored with "no highlights query on disk".
//!
//! Hermetic: the grammar is compiled from the cargo registry into a temp
//! `$XDG_DATA_HOME/nvim/site` (the fallback root [`bemtvi_ts::extra_roots`]
//! searches); `BEMTVI_DATA_DIR` stays unset so the fallback is consulted. Its own
//! test binary, since those env vars are process-global.

mod fixture;

use bemtvi_core::SyntaxEngine;
use bemtvi_ts::Engine;
use fixture::{install_rust_grammar, TempDir};

#[test]
fn base_query_reads_from_the_root_the_grammar_loads_from() {
    // The grammar lives ONLY under the neovim-site fallback root.
    let xdg = TempDir::new("site_root_xdg");
    let site = xdg.0.join("nvim").join("site");
    std::fs::create_dir_all(&site).unwrap();
    install_rust_grammar(&site);

    // The writable data dir is empty (nothing was ever `:TSInstall`ed).
    let data = TempDir::new("site_root_data");
    std::env::remove_var("BEMTVI_DATA_DIR");
    std::env::set_var("XDG_DATA_HOME", &xdg.0);

    let mut engine = Engine::new(data.0.clone());

    // Sanity: the grammar itself resolves from the site root and paints.
    assert!(
        !engine
            .highlight_text("rust", "fn main() {}\n", 0, 1)
            .is_empty(),
        "the site-root rust grammar should load and paint"
    );

    // The base query must be the site root's file — the same one the loaded
    // grammar compiled — not a missing `data_dir` read.
    let base = engine
        .base_query("rust", "highlights")
        .expect("base_query should not error");
    assert_eq!(
        base.as_deref(),
        Some(tree_sitter_rust::HIGHLIGHTS_QUERY),
        "base_query must return the highlights query of the root the grammar \
         actually loads from (the neovim site fallback), not None from the \
         empty data dir"
    );

    // Clearing an override reverts to the on-disk base; for a site-root grammar
    // this used to fail loud with "no highlights query on disk".
    engine
        .set_query("rust", "highlights", None)
        .expect("clearing a highlights override must revert to the site root's base");
}
