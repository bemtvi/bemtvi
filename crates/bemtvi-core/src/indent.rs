//! Detect a file's indentation convention from its own text.
//!
//! `'indentdetect'` (on by default) lets an opened file's *existing* indentation decide
//! that buffer's `'expandtab'` and `'shiftwidth'`, so a tab-indented file edited in a
//! spaces-by-default config keeps growing tabs, and a 2-space file keeps indenting by 2
//! — vim-sleuth's behavior, built in rather than bolted on.
//!
//! This module is the pure detector: text in, a verdict out, no editor state. The verdict
//! is applied at every read seam by [`Editor::detect_buffer_indent`], which is what makes
//! it land identically on a local read, a daemon read, and a browser read.
//!
//! It is deliberately a *detector*, not a parser: it reads only the leading whitespace of
//! each line, and it answers `None` — "no opinion, keep what the config said" — whenever
//! the file gives it nothing to go on. An empty file, a one-line file, and a file with no
//! indented line at all all leave the configured style untouched.

/// The indentation convention read off a file's own lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndentStyle {
    /// Whether the file indents with spaces (`true` → `'expandtab'`) or with tabs.
    pub expandtab: bool,
    /// The indent step in columns, when the file showed one. Always `None` for a
    /// tab-indented file — one indent level there is one tab, whose width is
    /// `'tabstop'`, a display choice the bytes cannot reveal.
    pub width: Option<usize>,
}

/// Lines read before the detector stops looking. A file that hasn't shown its hand in
/// this many lines is either enormous or unindented; either way the scan must stay
/// bounded, since it runs on every file read (CLAUDE.md: the editor must never freeze).
const MAX_SCAN_LINES: usize = 8192;

/// Indented lines that count as a decided verdict — the early exit for the common case
/// where a file's convention is obvious in its first screenful.
const ENOUGH_SAMPLES: usize = 512;

/// The widest step considered a plausible indent unit. A jump wider than this is a
/// multi-level jump or continuation alignment, not the file's indent size.
const MAX_WIDTH: usize = 8;

/// Space-indented lines a file must show before its *width* is adopted. One indented
/// line is not a convention — it is as likely a stray misindent, and inheriting its
/// width would let a single odd line set `'shiftwidth'` for the whole file. The
/// *direction* (tabs vs spaces) is still taken from a single line: that is the one thing
/// a lone indented line does say clearly, and there is nothing better to go on.
const MIN_SAMPLES_FOR_WIDTH: usize = 2;

/// The narrowest step considered a plausible indent unit. Deliberately 2, not 1: a
/// single-space step is almost never a real indent convention, but it *is* what a
/// stray continuation line or a nested-list body produces, so counting it would let a
/// handful of odd lines drag `'shiftwidth'` down to 1 for the whole file.
const MIN_WIDTH: usize = 2;

/// Read the indentation convention off `lines` (a file's editable lines, without their
/// line breaks), or `None` when the text carries no usable evidence.
///
/// The rules, in the order they matter:
///
/// * A line whose indent **starts with a tab** is tab evidence — including the
///   tab-indent-then-space-alignment style, where the tabs are the indent and the
///   spaces are alignment inside a line.
/// * A line indented with **spaces only** is space evidence, and its width joins the
///   vote for `'shiftwidth'`.
/// * Whichever kind of evidence is more common wins; an exact tie is no verdict.
///
/// The `'shiftwidth'` vote is over the *steps between consecutive lines* — the
/// difference in indent from one line to the next — not the raw indent widths, because a
/// 2-space file's raw widths are 2, 4, 6, 8 (which alone look as much like a 4-space file
/// with skipped levels) while its steps are all 2. Blank lines and C-style block-comment
/// bodies (`*` continuations) are passed over without breaking the chain, so a comment
/// between two statements doesn't manufacture a 1-column step.
pub fn detect<S: AsRef<str>>(lines: impl Iterator<Item = S>) -> Option<IndentStyle> {
    // Lines indented with a leading tab, and lines indented with spaces only.
    let (mut tabs, mut spaces) = (0usize, 0usize);
    // Votes for each candidate indent step, indexed by width.
    let mut steps = [0usize; MAX_WIDTH + 1];
    // The narrowest space indent seen — the fallback when a file shows indented lines
    // but never a step between two of them (a one-level file).
    let mut min_space = usize::MAX;
    // The previous space-indent width, or `None` where the chain broke (a tab line, a
    // mixed indent) and the next step would be meaningless.
    let mut prev: Option<usize> = None;

    for line in lines.take(MAX_SCAN_LINES) {
        let line = line.as_ref();
        let lead_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let (lead, rest) = line.split_at(lead_len);
        // A blank line indents nothing, and a block-comment body indents to align its
        // `*` with the opening `/*` — one column off the code around it. Neither is
        // evidence, and neither breaks the chain between the lines on either side.
        if rest.is_empty() || rest.starts_with('*') {
            continue;
        }
        if lead.is_empty() {
            // Column 0 is a real indent level: the step from it to the next line is the
            // single clearest sample of the file's indent unit.
            prev = Some(0);
        } else if lead.starts_with('\t') {
            tabs += 1;
            prev = None;
        } else if lead.bytes().all(|b| b == b' ') {
            spaces += 1;
            let width = lead.len();
            min_space = min_space.min(width);
            if let Some(prev) = prev {
                let step = width.abs_diff(prev);
                if (MIN_WIDTH..=MAX_WIDTH).contains(&step) {
                    steps[step] += 1;
                }
            }
            prev = Some(width);
        } else {
            // Spaces then a tab: an alignment mix that says nothing about the indent
            // unit, and whose width is not comparable with the lines around it.
            prev = None;
        }
        if tabs + spaces >= ENOUGH_SAMPLES {
            break;
        }
    }

    match tabs.cmp(&spaces) {
        // No indented line anywhere, or the two conventions are exactly as common as
        // each other: the file has no convention to honor, so keep the configured one.
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(IndentStyle {
            expandtab: false,
            width: None,
        }),
        std::cmp::Ordering::Less => Some(IndentStyle {
            expandtab: true,
            width: (spaces >= MIN_SAMPLES_FOR_WIDTH)
                .then(|| pick_width(&steps, min_space))
                .flatten(),
        }),
    }
}

/// The winning indent step: the most-voted width, ties going to the narrower one (a
/// 2-space file that also steps by 4 indents by 2). With no step at all — a file whose
/// indented lines all sit at one level, so nothing ever *steps* — fall back to the
/// narrowest indent actually seen, which for such a file is exactly its unit. Only
/// reached once the file has shown [`MIN_SAMPLES_FOR_WIDTH`] indented lines.
fn pick_width(steps: &[usize; MAX_WIDTH + 1], min_space: usize) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (width, &votes) in steps.iter().enumerate().take(MAX_WIDTH + 1).skip(MIN_WIDTH) {
        if votes > 0 && best.is_none_or(|(most, _)| votes > most) {
            best = Some((votes, width));
        }
    }
    match best {
        Some((_, width)) => Some(width),
        None => (MIN_WIDTH..=MAX_WIDTH)
            .contains(&min_space)
            .then_some(min_space),
    }
}
