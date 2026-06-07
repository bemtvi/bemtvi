// Sample buffer for the vim.treesitter playground (examples/treesitter/init.lua).
// Open it and try :TSRoot, :TSFunctions, :TSPub, :TSNodeAt.

struct Point {
    x: i64,
    y: i64,
}

fn origin() -> Point {
    Point { x: 0, y: 0 }
}

fn translate(p: Point, dx: i64, dy: i64) -> Point {
    Point {
        x: p.x + dx,
        y: p.y + dy,
    }
}

// A capitalized name, so :TSPub has something to report.
fn Builder() -> Point {
    origin()
}

fn main() {
    let p = translate(origin(), 3, 4);
    println!("{} {}", p.x, p.y);
}
