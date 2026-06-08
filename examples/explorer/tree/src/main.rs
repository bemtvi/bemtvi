fn main() {
    // A sample file to open from the explorer's `src/` sub-directory.
    println!("{}", greeting());
}

fn greeting() -> String {
    format!("hello from {}", module_path!())
}
