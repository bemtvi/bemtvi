"""A tiny geometry toolkit — the demo project's library module.

Open this alongside main.py: basedpyright type-checks both, so hover (K),
go-to-definition (gd) and rename (grn) work across the two files. As you type,
the editor's autocomplete suggests the names defined here.

The module is deliberately small — a single dataclass and one free function — so
it reads top-to-bottom in one screen while still exercising the language server.
"""

from dataclasses import dataclass
from math import pi


@dataclass
class Circle:
    """A circle, identified by its radius.

    Defined as a :func:`~dataclasses.dataclass`, so the constructor, ``repr`` and
    equality are generated automatically — ``Circle(radius=3.0)`` is all it takes.

    Attributes:
        radius: The distance from the centre to the edge, in arbitrary units.
            Areas and circumferences come back in the matching squared / linear
            units.
    """

    radius: float

    def area(self) -> float:
        """Return the area enclosed by the circle (``pi * r**2``)."""
        return pi * self.radius**2

    def circumference(self) -> float:
        """Return the perimeter of the circle (``2 * pi * r``)."""
        return 2 * pi * self.radius


def rectangle_area(width: float, height: float) -> float:
    """Return the area of an axis-aligned rectangle.

    Args:
        width: The length of the horizontal sides.
        height: The length of the vertical sides.

    Returns:
        The area, i.e. ``width * height``.
    """
    return width * height
