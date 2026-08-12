"""A shape module with an unresolved merge conflict — for the bemtvi-diff plugin.

Two branches both reworked ``Triangle.area()`` and disagreed, so this file still
carries its git conflict markers. It does NOT import or run as-is (the markers are
not valid Python) — that is the point: it's here to be resolved. Open it and run

    :BtvDiffConflict

to see the conflict laid out as a 3-way diff — ours | base | theirs, the two outer
panes anchored against the common ancestor in the middle. Step between conflicts
with ]c / [c and resolve the one under the cursor:

    co  keep ours        ct  keep theirs        cb  keep both
    cp  stage selected line(s) from a pane    ca  apply    cx  clear

(co / ct / cb rewrite the marker block as one undoable edit, then close the diff.)
Once it's resolved, ``:BtvDiffGit`` diffs your edits against the file on disk.
"""

from dataclasses import dataclass
from math import sqrt


@dataclass
class Triangle:
    """A triangle given by the lengths of its three sides."""

    a: float
    b: float
    c: float

    def perimeter(self) -> float:
        """Return the sum of the three side lengths."""
        return self.a + self.b + self.c

    def area(self) -> float:
        """Return the area enclosed by the triangle."""
<<<<<<< HEAD
        # Heron's formula, factored through the semi-perimeter for readability.
        s = self.perimeter() / 2
        return sqrt(s * (s - self.a) * (s - self.b) * (s - self.c))
||||||| merged common ancestor
        # TODO: a branch still needs to land the real formula here.
        raise NotImplementedError("Triangle.area")
=======
        # Heron's formula, expanded into a single product to skip the temporary.
        a, b, c = self.a, self.b, self.c
        return 0.25 * sqrt((a + b + c) * (-a + b + c) * (a - b + c) * (a + b - c))
>>>>>>> feature/triangle-area
