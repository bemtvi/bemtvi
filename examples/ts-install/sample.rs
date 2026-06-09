// After `:TSInstall rust`, try these (the indentation is treesitter-driven):
//
//   * Put the cursor on the `fn main() {` line and press  o  — the new line
//     lands one level in (4 spaces).
//   * Append at the end of a `{` line with  A  then press  <CR>  — same.
//   * Jam a statement to column 0 inside a block and press  ==  — it reindents.
//   * Select the whole file with  ggVG  then press  =  — the lot reflows.

fn main() {
    let xs = [1, 2, 3];
    for x in xs {
        if x % 2 == 0 {
            println!("{x} is even");
        } else {
            println!("{x} is odd");
        }
    }
}
