// A worked file for the tree-sitter text objects. Move the cursor onto the marked
// spots and try the keys in the "TRY IT" list in init.lua.

/// A point in 2-D space.  (`vit` / `vat` on this struct → the type object.)
struct Point {
    x: i32,
    y: i32,
}

// This is a free-standing comment.  (`vic` / `vac` anywhere on this line.)
fn distance(a: Point, b: Point) -> f64 {
    // Put the cursor on `dx` and try `vif` (inside fn) or `vaf` (around fn).
    let dx = (a.x - b.x) as f64;
    let dy = (a.y - b.y) as f64;

    // A nested closure: from here, `vif` grabs the closure, `2vif` the outer fn.
    let square = |v: f64| -> f64 { v * v };

    (square(dx) + square(dy)).sqrt()
}

// A loop + a call, for the CUSTOM objects mapped in init.lua (`vil` loop, `vik` call).
fn total(points: &[Point]) -> f64 {
    let mut sum = 0.0;
    for p in points {
        // Cursor here, `vil` selects inside the for-loop; `vik` selects the call.
        sum += distance(Point { x: 0, y: 0 }, Point { x: p.x, y: p.y });
    }
    sum
}

fn main() {
    let origin = Point { x: 0, y: 0 };
    let target = Point { x: 3, y: 4 };
    // Cursor on `target`, then `dia` deletes just that argument; `cia` changes it.
    println!("{}", distance(origin, target));
    println!("{}", total(&[origin, target]));
}
