def add(a: int, b: int) -> int:
    return a + b


def main() -> None:
    total = add(1, 2)
    print(total)
    # Deliberate error: `undefined_symbol` is never defined, so pyright must
    # report a `reportUndefinedVariable` diagnostic on this line.
    print(undefined_symbol)
