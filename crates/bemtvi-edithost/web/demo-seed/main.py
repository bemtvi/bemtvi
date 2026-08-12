"""The demo project's entry point.

Run it in-browser:  :terminal python main.py
(CPython runs via Pyodide, fully client-side — no server.)

It imports the typed helpers from :mod:`geometry` and prints a few derived
measurements, so it doubles as a smoke test that cross-file imports, the language
server and autocomplete all work in the browser build.
"""

from geometry import Circle, rectangle_area


def main() -> None:
    """Print the area / circumference of a sample circle and a rectangle.

    Constructs a radius-3 :class:`~geometry.Circle`, then formats its area and
    circumference to two decimal places alongside the area of a 4x5 rectangle.
    Returns nothing — every result goes to standard output via ``print``.
    """
    c = Circle(radius=3.0)
    print(f"circle area        = {c.area():.2f}")
    print(f"circle circumference = {c.circumference():.2f}")
    print(f"rectangle 4x5 area = {rectangle_area(4, 5)}")


if __name__ == "__main__":
    main()
