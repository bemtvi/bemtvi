// The string literal below holds Rust source. With the injection query enabled
// (see init.lua) the engine parses that string body as Rust and paints it — so
// `fn`, the type, and the operators inside the quotes light up, instead of the
// whole string being one flat color.
const SNIPPET: &str = "fn demo() {}";

fn main() {
    println!("{SNIPPET}");
}
