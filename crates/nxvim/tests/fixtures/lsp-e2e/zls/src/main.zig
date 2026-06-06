pub fn main() void {
    // Deliberate syntax error: the binding's right-hand side is missing, so zls
    // reports a parse/AST diagnostic on this line. zls parses with its own Zig
    // tokenizer, so this surfaces without a Zig toolchain installed.
    const answer: i32 =
    _ = answer;
}
