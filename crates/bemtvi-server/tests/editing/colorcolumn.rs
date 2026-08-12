//! `'colorcolumn'` — the resolved ruler columns reach the client in the redraw.
//! The option stores the raw string; the projection resolves it to the sorted
//! 1-based columns the client paints (skipping junk and `'textwidth'`-relative
//! `+N`/`-N` entries, which bemtvi has no `'textwidth'` to anchor). The *painting*
//! itself is covered by the `bemtvi-tui` paint tests; this pins the core→wire step.

use crate::support::*;

/// Read the focused window's projected `colorcolumn` array off a redraw map.
fn colorcolumn_of(map: &[(Value, Value)]) -> Vec<u64> {
    field(map, "colorcolumn")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn colorcolumn_resolves_absolute_columns_into_the_redraw() {
    let (rpc, mut incoming) = start(None).await;

    // No ruler by default.
    let map = redraw_after(&rpc, &mut incoming, ":redraw<CR>").await;
    assert!(colorcolumn_of(&map).is_empty(), "no rulers by default");

    // `:set colorcolumn=80,120` projects both columns, sorted.
    let map = redraw_after(&rpc, &mut incoming, ":set colorcolumn=80,120<CR>").await;
    assert_eq!(
        colorcolumn_of(&map),
        vec![80, 120],
        "the absolute ruler columns reach the client"
    );

    // Clearing the option removes the rulers again.
    let map = redraw_after(&rpc, &mut incoming, ":set colorcolumn=<CR>").await;
    assert!(colorcolumn_of(&map).is_empty(), "cleared → no rulers");
}

#[tokio::test]
async fn colorcolumn_skips_relative_and_junk_entries() {
    let (rpc, mut incoming) = start(None).await;

    // `+1`/`-2` are `'textwidth'`-relative (unsupported — bemtvi has no textwidth) and
    // `foo`/`0` are junk; only the absolute `80` survives, so the ruler still renders
    // instead of the whole option being rejected (matching vim's lenient parsing).
    let map = redraw_after(&rpc, &mut incoming, ":set colorcolumn=+1,foo,0,80,-2<CR>").await;
    assert_eq!(
        colorcolumn_of(&map),
        vec![80],
        "relative and junk entries are skipped, the absolute one kept"
    );
}
