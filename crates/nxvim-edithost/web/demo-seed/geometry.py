"""A tiny geometry toolkit — the demo project's library module.

Open this alongside main.py: basedpyright type-checks both, so hover (K),
go-to-definition (gd) and rename (grn) work across the two files.
"""

from dataclasses import dataclass
from math import pi


@dataclass
class Circle:
    radius: float

    def area(self) -> float:
        return pi * self.radius**2

    def circumference(self) -> float:
        return 2 * pi * self.radius


def rectangle_area(width: float, height: float) -> float:
    return width * height
