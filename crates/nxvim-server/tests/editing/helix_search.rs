//! Helix editing model — document search (`/`, `?`, `n`, `N`) + case defaults.
//!
//! Distinct from the selection-*within* regex prompts (`s`/`S`/`K`, see
//! `helix_regex.rs`), these are Helix's document-wide search: `/` forward, `?`
//! backward, `n`/`N` repeat. A match *selects* the whole matched text (anchor at
//! its start, head on its last char) rather than landing a point cursor. Helix
//! search is smart-case by default — a **self-contained** flag
//! (`nx.helix.smart_case`), not the global `:set ignorecase`/`smartcase`, which it
//! never touches (turning it off falls back to those). Tests assert on the rendered
//! selection span, the cursor head, and the option state.

use crate::support::*;

/// `/` in Helix mode searches the document and *selects* the whole match.
#[tokio::test]
async fn forward_search_selects_the_match() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>gg0:helix<CR>");

    // `/bar` lands on and selects "bar" (cols 4,5,6 → span [4,7), head on the last).
    let map = redraw_after(&rpc, &mut incoming, "/bar<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 6),
        "head on the last char of the match"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((4, 7)),
        "the whole match is selected",
    );
}

/// `?` searches backward and likewise selects the match.
#[tokio::test]
async fn backward_search_selects_the_match() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>$:helix<CR>");

    // From the end, `?foo` finds "foo" at the line start (cols 0,1,2 → span [0,3)).
    let map = redraw_after(&rpc, &mut incoming, "?foo<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "head on the last char of the match"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 3)),
        "the whole match is selected",
    );
}

/// `n` / `N` walk forward / backward through matches, re-selecting each.
#[tokio::test]
async fn n_and_shift_n_repeat_the_search() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaa foo bb foo cc<Esc>gg0:helix<CR>");

    // `/foo` → the first "foo" (cols 3,4,5 → span [3,6)).
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "first match");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((3, 6))
    );

    // `n` → the second "foo" (cols 10,11,12 → span [10,13)).
    let map = redraw_after(&rpc, &mut incoming, "n").await;
    assert_eq!(cursor(&rpc).await, (1, 12), "next match");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((10, 13))
    );

    // `N` → back to the first "foo".
    let map = redraw_after(&rpc, &mut incoming, "N").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "back to the first match");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((3, 6))
    );
}

/// In select mode (`v`), `n` *adds* the next match as a new selection — keeping
/// every existing selection — rather than replacing or extending them. Each match
/// is its own range (a multi-selection), not one growing span.
#[tokio::test]
async fn n_adds_match_as_new_selection_in_select_mode() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaa foo bb foo cc<Esc>gg0:helix<CR>");

    // `/foo` (normal mode) selects the first match (cols 3,4,5), then `v` enables
    // the multi-selection mode.
    feed(&rpc, "/foo<CR>v");
    assert_eq!(cursor(&rpc).await, (1, 5), "first match selected");

    // `n` adds the second "foo" (cols 10,11,12) as a *new* primary selection while
    // the first stays as a separate secondary selection — two distinct ranges.
    let map = redraw_after(&rpc, &mut incoming, "n").await;
    assert_eq!(cursor(&rpc).await, (1, 12), "head on the newly-added match");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((10, 13)),
        "the new primary selection is just the second match",
    );
    assert_eq!(
        view_secondary_selection(&map)
            .first()
            .cloned()
            .unwrap_or_default(),
        vec![(3, 6)],
        "the first match remains its own selection",
    );
}

/// Entering search from select mode keeps the current selection and *adds* the
/// match as a new selection (and each subsequent `n` adds another).
#[tokio::test]
async fn search_from_select_mode_adds_match_as_new_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaa foo bb foo cc<Esc>gg0:helix<CR>");

    // Select "aa" (cols 0,1) in select mode, then `/foo` keeps it and adds the
    // first match (cols 3,4,5) as a new primary selection.
    let map = redraw_after(&rpc, &mut incoming, "vl/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "head on the added match");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((3, 6)),
        "the added match is the new primary selection",
    );
    assert_eq!(
        view_secondary_selection(&map)
            .first()
            .cloned()
            .unwrap_or_default(),
        vec![(0, 2)],
        "the original 'aa' selection is kept as a secondary",
    );

    // A following `n` adds the second match too — now three separate selections.
    let map = redraw_after(&rpc, &mut incoming, "n").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 12),
        "head on the second added match"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((10, 13)),
        "the second match is the new primary",
    );
    let mut secs = view_secondary_selection(&map)
        .first()
        .cloned()
        .unwrap_or_default();
    secs.sort_unstable();
    assert_eq!(
        secs,
        vec![(0, 2), (3, 6)],
        "both the 'aa' selection and the first match are kept",
    );
}

/// Issue 1: starting a search from select mode must NOT hide the current
/// selection — it stays visible (and un-extended) while the pattern is typed.
#[tokio::test]
async fn selection_stays_visible_while_typing_a_search() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaa foo bb foo cc<Esc>gg0:helix<CR>");

    // Select "aa" (cols 0,1) in select mode.
    feed(&rpc, "vl");

    // Type a search *without* submitting: the selection is still rendered, unchanged
    // — it neither disappears nor stretches to the previewed match.
    let map = redraw_after(&rpc, &mut incoming, "/foo").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 2)),
        "the selection stays put and visible while the search is being typed",
    );
}

/// A pending `f`/`t`/`F`/`T` reads its target character *raw*, ahead of the
/// `helix`-bucket keymaps — so `fa`/`fi`/`fg` find the letter instead of the `a`
/// (append), `i` (insert), `g` (goto) verbs those keys otherwise bind.
#[tokio::test]
async fn find_target_beats_the_mapped_verb_keys() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iz a i g<Esc>gg0:helix<CR>");

    // `fa` → the 'a' at col 2 (not append-insert).
    feed(&rpc, "fa");
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "f found 'a', did not enter append"
    );

    // `fi` → the 'i' at col 4 (not insert). `0` collapses back to the line start.
    feed(&rpc, "0fi");
    assert_eq!(
        cursor(&rpc).await,
        (1, 4),
        "f found 'i', did not enter insert"
    );

    // `fg` → the 'g' at col 6 (not the goto menu).
    feed(&rpc, "0fg");
    assert_eq!(cursor(&rpc).await, (1, 6), "f found 'g', did not open goto");

    // The buffer is untouched throughout — no insert-entry key ever fired.
    assert_eq!(lines(&rpc).await, vec!["z a i g"]);
}

/// Helix search defaults to smart-case, self-contained from the global options:
/// a lowercase pattern matches uppercase text, yet entering Helix never mutates
/// `:set ignorecase`/`smartcase` (which vim-mode search reads).
#[tokio::test]
async fn helix_search_is_smart_case_without_touching_global_options() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello FOO bar<Esc>gg0:helix<CR>");

    // Self-contained: the global search options stay at nxvim's own default (off).
    assert_eq!(
        exec_lua(&rpc, "return vim.o.ignorecase").await.as_bool(),
        Some(false),
        "entering Helix left the global 'ignorecase' untouched",
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.smartcase").await.as_bool(),
        Some(false),
        "entering Helix left the global 'smartcase' untouched",
    );

    // …but a lowercase `/foo` still matches the uppercase "FOO" (cols 6,7,8).
    feed(&rpc, "/foo<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 8),
        "lowercase pattern matched uppercase text (Helix's own smart-case)"
    );
}

/// Smart-case: an *uppercase* pattern stays case-sensitive, so it skips
/// lowercase text and only matches the uppercase run.
#[tokio::test]
async fn smartcase_uppercase_pattern_is_case_sensitive() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc foo xyz FOO<Esc>gg0:helix<CR>");

    // `/FOO` (uppercase) with smartcase → case-sensitive → skip "foo" (col 4),
    // match "FOO" (cols 12,13,14 → head on 14).
    feed(&rpc, "/FOO<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 14),
        "uppercase pattern matched only the uppercase run"
    );
}

/// `nx.helix.enable{ smart_case = false }` turns the default off: a lowercase
/// pattern is then case-sensitive, and the global options are still untouched.
#[tokio::test]
async fn smart_case_off_makes_helix_search_case_sensitive() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc FOO def foo<Esc>gg0");

    // Enter Helix with smart-case off (exercises the `enable{…}` opts path).
    exec_lua(&rpc, "nx.helix.enable{ smart_case = false }").await;

    // A lowercase `/foo` is now case-sensitive → skip "FOO" (col 4), match the
    // lowercase "foo" (cols 12,13,14 → head on 14).
    feed(&rpc, "/foo<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 14),
        "case-sensitive search skipped the uppercase run"
    );
    // Still self-contained — the global option was never written.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.ignorecase").await.as_bool(),
        Some(false),
    );
}

/// With Helix smart-case off, search falls back to the *global* `'ignorecase'` —
/// so `:set ignorecase` makes Helix search case-insensitive again, exactly as it
/// would in vim mode.
#[tokio::test]
async fn smart_case_off_falls_back_to_global_ignorecase() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc FOO def foo<Esc>gg0:helix<CR>");

    exec_lua(&rpc, "nx.helix.smart_case(false)").await;
    feed(&rpc, ":set ignorecase<CR>");

    // Global ignorecase now drives it → lowercase `/foo` matches the first run,
    // the uppercase "FOO" (cols 4,5,6 → head on 6).
    feed(&rpc, "/foo<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 6),
        "case-insensitive via the global option (Helix smart-case off)"
    );
}
