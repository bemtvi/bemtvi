"""Generate the tiny fonts the GUI's font-chain tests shape against.

Each font draws a known handful of characters as plain boxes, so a test can tell
which font drew a glyph by its font, and checks nothing about the drawing itself.
Rerun to regenerate (needs fontTools): python3 make.py

- BtvTest Mono: space, a-e, and an `ss01` alternate for `a` (a.alt); its Italic
  face draws a and b only
- BtvTest Second: a, x, y, z (its `a` must lose to the first font in a chain)
- BtvTest Wide: x and 中 (U+4E2D)
"""

import os

from fontTools.feaLib.builder import addOpenTypeFeaturesFromString
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

HERE = os.path.dirname(os.path.abspath(__file__))


def box(width):
    pen = TTGlyphPen(None)
    pen.moveTo((50, 0))
    pen.lineTo((50, 700))
    pen.lineTo((width - 50, 700))
    pen.lineTo((width - 50, 0))
    pen.closePath()
    return pen.glyph()


def empty():
    return TTGlyphPen(None).glyph()


def build(file, family, chars, advance=600, wide=(), features="", italic=False):
    names = [".notdef"]
    cmap = {}
    for ch in chars:
        name = "u%04X" % ord(ch)
        names.append(name)
        cmap[ord(ch)] = name
    if "ss01" in features:
        names.append("a.alt")
    fb = FontBuilder(1000, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap(cmap)
    glyphs, metrics = {}, {}
    for name in names:
        ch = next((c for c, n in cmap.items() if n == name), None)
        w = 2 * advance if ch is not None and chr(ch) in wide else advance
        glyphs[name] = empty() if ch == 0x20 else box(w)
        metrics[name] = (w, 50)
    fb.setupGlyf(glyphs)
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=800, descent=-200)
    # fontdb skips a face without a PostScript name (name ID 6).
    fb.setupNameTable(
        {
            "familyName": family,
            "styleName": "Italic" if italic else "Regular",
            "psName": family.replace(" ", "") + ("-Italic" if italic else "-Regular"),
        }
    )
    fb.setupOS2(
        sTypoAscender=800,
        sTypoDescender=-200,
        usWinAscent=800,
        usWinDescent=200,
        fsSelection=0x01 if italic else 0x40,
    )
    if italic:
        fb.font["head"].macStyle = 0x02
    fb.setupPost(isFixedPitch=1)
    if features:
        addOpenTypeFeaturesFromString(fb.font, features)
    fb.save(os.path.join(HERE, file))


build(
    "BtvTestMono.ttf",
    "BtvTest Mono",
    " abcde",
    features="feature ss01 { sub u0061 by a.alt; } ss01;",
)
build("BtvTestMonoItalic.ttf", "BtvTest Mono", "ab", italic=True)
build("BtvTestSecond.ttf", "BtvTest Second", "axyz")
build("BtvTestWide.ttf", "BtvTest Wide", "x中", wide="中")
