//! Throwaway generator: dumps the codepoint ranges where `unicode-width` (the exact
//! version the server uses) reports a display width of 2 or 0, as JS array literals to
//! paste into the wasm web client's `charWidth`. Run: `cargo run -p bemtvi-core --example dump_width`.
use unicode_width::UnicodeWidthChar;

fn ranges_for(target: usize) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for cp in 0u32..=0x10FFFF {
        // Skip surrogates (not scalar values).
        if (0xD800..=0xDFFF).contains(&cp) {
            continue;
        }
        let c = match char::from_u32(cp) {
            Some(c) => c,
            None => continue,
        };
        // Match the server: `UnicodeWidthStr::width` of a single printable char is its
        // `UnicodeWidthChar::width` (control chars return None and never reach the client,
        // so treat None as "not this target").
        let w = UnicodeWidthChar::width(c);
        if w == Some(target) {
            match ranges.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => ranges.push((cp, cp)),
            }
        }
    }
    ranges
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

fn main() {
    print_js("WIDE_RANGES", &ranges_for(2));
    println!();
    print_js("ZERO_RANGES", &ranges_for(0));
}
