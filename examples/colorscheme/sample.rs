// A small Rust sample so `:colorscheme bemtvi` has something colorful to paint:
// keywords, strings, numbers, types, and comments all get their One Dark hue.

/// Greet someone by name, the classic way.
fn greet(name: &str) -> String {
    let times = 3;
    let mut out = String::new();
    for i in 0..times {
        out.push_str(&format!("[{i}] hello, {name}!\n"));
    }
    out
}

struct Config {
    verbose: bool,
    retries: u32,
}

fn main() {
    let cfg = Config { verbose: true, retries: 5 };
    if cfg.verbose {
        print!("{}", greet("bemtvi"));
    }
    println!("retries = {}", cfg.retries);
}
