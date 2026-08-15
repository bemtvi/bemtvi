//! Throwaway generator: dumps the tables the wasm web client's DOM renderer needs to
//! agree with the server's column model, as JS array literals to paste into
//! `crates/bemtvi-edithost/web/index.html`. Run:
//! `cargo run -p bemtvi-core --example dump_width`.
//!
//! Two families of table come out:
//!
//! 1. **Per-codepoint width** (`WIDE_RANGES` / `ZERO_RANGES`) — the codepoints
//!    `unicode-width` reports as 2 and 0 cells, for the client's `charWidth`.
//! 2. **Cluster rules** (the four `*_BASE` / `*_WIDENS` / `*_NARROWS` tables) — the
//!    bases for which a grapheme cluster's width is NOT the sum of its codepoints'.
//!    An emoji-modifier sequence, a ZWJ sequence and an emoji-presentation sequence
//!    are 2 cells however many codepoints they hold, and a text-presentation sequence
//!    collapses a wide emoji to 1. `UnicodeWidthStr` applies those rules over a whole
//!    string; the client has to reproduce them per cluster, and these tables say which
//!    bases each rule fires for.
//!
//! The generator also **self-checks** the client's rule (`cluster_rule` below, the
//! exact shape `clusterWidth` implements in JS) against `UnicodeWidthStr::width` over
//! every single-cluster emoji sequence that can be built from every codepoint — see
//! the mismatch count it prints, and the note there about what does not converge.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn w(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}
fn sw(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The codepoints satisfying `f`, coalesced into sorted inclusive ranges.
fn ranges<F: Fn(char) -> bool>(f: F) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for cp in 0u32..=0x10FFFF {
        // Skip surrogates (not scalar values).
        if (0xD800..=0xDFFF).contains(&cp) {
            continue;
        }
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if !f(c) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 + 1 == cp => last.1 = cp,
            _ => out.push((cp, cp)),
        }
    }
    out
}

/// Range membership by binary search — the same lookup the client's `inRanges` does.
fn has(r: &[(u32, u32)], c: char) -> bool {
    let cp = c as u32;
    r.binary_search_by(|&(lo, hi)| {
        if cp < lo {
            std::cmp::Ordering::Greater
        } else if cp > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

fn print_js(name: &str, ranges: &[(u32, u32)]) {
    println!("const {name} = [");
    for chunk in ranges.chunks(6) {
        let line: Vec<String> = chunk
            .iter()
            .map(|(lo, hi)| format!("[0x{lo:x}, 0x{hi:x}]"))
            .collect();
        println!("  {},", line.join(", "));
    }
    println!("];  // {} ranges", ranges.len());
}

const TONES: std::ops::RangeInclusive<char> = '\u{1F3FB}'..='\u{1F3FF}';

fn main() {
    // --- 1. per-codepoint width -------------------------------------------------
    // Match the server: `UnicodeWidthStr::width` of a single printable char is its
    // `UnicodeWidthChar::width` (control chars return None and never reach the client,
    // so treat None as "not this target").
    let wide = ranges(|c| UnicodeWidthChar::width(c) == Some(2));
    let zero = ranges(|c| UnicodeWidthChar::width(c) == Some(0));

    // --- 2. cluster rules -------------------------------------------------------
    // A width-1 base that an emoji-presentation selector widens to 2 (`\u{2764}\u{fe0f}`).
    let vs16 = ranges(|c| w(c) == 1 && sw(&format!("{c}\u{FE0F}")) == 2);
    // A width-2 emoji that a text-presentation selector narrows to 1 (`\u{2614}\u{fe0e}`).
    let vs15 = ranges(|c| w(c) == 2 && sw(&format!("{c}\u{FE0E}")) == 1);
    // A base that absorbs a Fitzpatrick modifier into its own 2 cells (`\u{1f934}\u{1f3fc}`).
    let emod = ranges(|c| w(c) > 0 && sw(&format!("{c}\u{1F3FC}")) == 2);
    // A base that starts a 2-cell ZWJ sequence, unqualified and VS16-qualified. The two
    // differ: a redundant `\u{fe0f}` after an already-emoji base disqualifies the join,
    // so the client picks the table by whether the cluster carries a VS16.
    let ezwj = ranges(|c| w(c) > 0 && sw(&format!("{c}\u{200D}\u{1F600}")) == 2);
    let ezwjq = ranges(|c| w(c) > 0 && sw(&format!("{c}\u{FE0F}\u{200D}\u{1F600}")) == 2);

    // The client's rule, in Rust, so it can be diffed against the real thing.
    let cluster_rule = |s: &str| -> usize {
        let cps: Vec<char> = s.chars().collect();
        let base = cps[0];
        let hasc = |x: char| cps.contains(&x);
        if cps.iter().any(|&c| TONES.contains(&c)) && has(&emod, base) {
            return 2;
        }
        let zwj_base = if hasc('\u{FE0F}') { &ezwjq } else { &ezwj };
        if hasc('\u{200D}') && has(zwj_base, base) {
            return 2;
        }
        if hasc('\u{FE0F}') && has(&vs16, base) {
            return 2;
        }
        if hasc('\u{FE0E}') && has(&vs15, base) {
            return 1;
        }
        cps.iter().map(|&c| w(c)).sum()
    };

    // --- 3. self-check ----------------------------------------------------------
    // Every codepoint under every emoji sequence shape, keeping only the ones that are
    // a single grapheme cluster (the rule's domain).
    let tails: &[&str] = &[
        "",
        "\u{FE0F}",
        "\u{FE0E}",
        "\u{1F3FC}",
        "\u{200D}\u{1F600}",
        "\u{FE0F}\u{200D}\u{1F308}",
        "\u{FE0F}\u{20E3}",
        "\u{20E3}",
        "\u{1F3FB}\u{200D}\u{1F4BB}",
        "\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{1F1F7}",
    ];
    let (mut checked, mut bad, mut sample) = (0usize, 0usize, Vec::new());
    for cp in 0u32..=0x10FFFF {
        if (0xD800..=0xDFFF).contains(&cp) {
            continue;
        }
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if w(c) == 0 {
            continue; // a mark never starts a cluster on its own
        }
        for t in tails {
            let s = format!("{c}{t}");
            if s.graphemes(true).count() != 1 {
                continue;
            }
            checked += 1;
            let (want, got) = (sw(&s), cluster_rule(&s));
            if want != got {
                bad += 1;
                if sample.len() < 5 {
                    sample.push(format!(
                        "{:?} want {want} got {got}",
                        s.chars()
                            .map(|c| format!("{:04X}", c as u32))
                            .collect::<Vec<_>>()
                    ));
                }
            }
        }
    }
    println!("// self-check: {checked} single-cluster sequences, {bad} mismatches");
    println!("// Every mismatch has one shape: a base that does NOT itself begin an emoji");
    println!("// sequence, followed by a complete NESTED one (a lone skin tone, or a ZWJ join).");
    println!("// `unicode-width` ligatures the nested part to 2 cells and adds the base's own");
    println!("// width; a cluster-LOCAL rule has no way to see the nesting and sums instead.");
    println!("// No well-formed emoji sequence nests that way, and every one of them converges.");
    println!("// Samples:");
    for s in &sample {
        println!("//   {s}");
    }
    println!();

    print_js("WIDE_RANGES", &wide);
    println!();
    print_js("ZERO_RANGES", &zero);
    println!();
    print_js("VS16_WIDENS", &vs16);
    println!();
    print_js("VS15_NARROWS", &vs15);
    println!();
    print_js("EMOJI_MOD_BASE", &emod);
    println!();
    print_js("EMOJI_ZWJ_BASE", &ezwj);
    println!();
    print_js("EMOJI_ZWJ_VS16_BASE", &ezwjq);
}
