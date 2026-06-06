fn main() {
    // Deliberate syntax error: the binding's right-hand side is missing. This is a
    // parse-level diagnostic, which rust-analyzer reports within a few seconds of
    // `didOpen` — it does not require the crate graph / sysroot to finish loading
    // (a cold throwaway crate can take far longer to produce *semantic* diagnostics
    // like a type mismatch, which is why the deliberate error here is syntactic).
    let answer: i32 =
}
