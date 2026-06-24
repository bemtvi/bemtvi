"""The demo project's entry point.

Run it in-browser:  :terminal python main.py
(CPython runs via Pyodide, fully client-side — no server.)
"""

from geometry import Circle, rectangle_area


def main() -> None:
    c = Circle(radius=3.0)
    print(f"circle area        = {c.area():.2f}")
    print(f"circle circumference = {c.circumference():.2f}")
    print(f"rectangle 4x5 area = {rectangle_area(4, 5)}")


if __name__ == "__main__":
    main()
