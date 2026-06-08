fn main() {
    let answer = 42;
    let doubled = double(answer);
    println!("{answer} doubled is {doubled}");
}

fn double(n: i64) -> i64 {
    n * 2
}
