#!/usr/bin/env python3
"""Regenerate every derived app icon from the hand-drawn pixel-art sources.

The sources are the `icon<N>.png` files next to this script, each drawn by hand
at its own size (a downscale cannot keep pixel art legible). Each one is used as
is. Every larger size is an exact integer upscale of the largest source that
divides it (48 from `icon24.png`, 64 and up from `icon32.png`), with
nearest-neighbour sampling, never a filtered resize. A filter would blur the
pixel edges. The upscaled art is centred on its canvas (see `upscale`). Outputs:

- `icons/bemtvi-<N>.png`: the per-size PNGs that CI packs into the AppImage and
  the macOS `.icns`.
- `bemtvi.ico`: the Windows icon that `build.rs` embeds into `bemtvi-gui.exe`.
- `bemtvi-file.ico`: the Windows document icon, for files opened with bemtvi,
  from the hand-drawn `fileicon<N>.png` sources. `build.rs` embeds it as the
  exe's second icon. Its upscales keep the source's own placement (see
  `upscale_in_place`).
- `crates/bemtvi-lua/src/prelude/intro_logo.lua`: `icon24.png`'s pixels as Lua
  data, which the intro screen draws in the terminal with half-block characters.

Needs Pillow. Run it from anywhere: `python3 crates/bemtvi-gui/assets/gen-icons.py`.
"""

import io
import re
import struct
import sys
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
OUT = HERE / "icons"
INTRO_LOGO = HERE.parent.parent / "bemtvi-lua" / "src" / "prelude" / "intro_logo.lua"
# Each one must be a whole multiple of some source, so no upscale is fractional (a
# fractional scale would blur or distort the pixel grid).
UPSCALES = [48, 64, 128, 256, 512, 1024]
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
# The document icon has only a 32px drawing, so every size is 32 or a whole
# multiple of it. Windows samples the 32 down for the 16 to 24px views; a
# hand-drawn `fileicon16.png` / `fileicon24.png` would let 16, 24 and 48 join.
FILE_ICO_SIZES = [32, 64, 96, 128, 256]


def load(name, size):
    img = Image.open(HERE / name).convert("RGBA")
    if img.size != (size, size):
        sys.exit(f"{name} is {img.size[0]}x{img.size[1]}, expected {size}x{size}")
    return img


def upscale(src, size):
    """Scale `src` to `size` by a whole factor, nearest-neighbour, and centre its
    drawn (non-transparent) area on the canvas. A source may leave a margin: the
    24px art is 23×23 in the top-left corner, so a plain ×2 would leave its 2px of
    slack all on the right and bottom of the 48."""
    k = size // src.width
    box = src.getchannel("A").getbbox() or (0, 0, src.width, src.height)
    art = src.crop(box)
    art = art.resize((art.width * k, art.height * k), Image.Resampling.NEAREST)
    out = Image.new("RGBA", (size, size))
    out.paste(art, ((size - art.width) // 2, (size - art.height) // 2))
    return out


def upscale_in_place(src, size):
    """Scale the whole of `src` to `size` by a whole factor, nearest-neighbour,
    keeping the art where the source draws it. `fileicon32.png` leaves 5px empty
    on the left and 1px on top; re-centring it the way `upscale` does would move
    the art in every size but the 32."""
    return src.resize((size, size), Image.Resampling.NEAREST)


def load_sources(prefix):
    """Every hand-drawn `<prefix><N>.png` next to this script, keyed by N."""
    out = {}
    for path in HERE.glob(f"{prefix}*.png"):
        m = re.fullmatch(rf"{prefix}(\d+)\.png", path.name)
        if m:
            size = int(m.group(1))
            out[size] = load(path.name, size)
    return out


def scaled(sources, size, scale):
    """`size` as a source when one is drawn at it, else an integer upscale of the
    largest source that divides it."""
    if size in sources:
        return sources[size]
    bases = [b for b in sources if size % b == 0]
    if not bases:
        sys.exit(f"no source divides {size}: it cannot be an integer upscale")
    return scale(sources[max(bases)], size)


def write_ico(path, renders, sizes):
    """An ICO with PNG-compressed entries (Vista+), smallest first."""
    imgs = [(s, png_bytes(renders[s])) for s in sizes]
    off = 6 + 16 * len(imgs)
    entries = data = b""
    for s, png in imgs:
        d = 0 if s >= 256 else s  # 0 encodes 256 in the ICO dir entry
        entries += struct.pack("<BBBBHHII", d, d, 0, 0, 1, 32, len(png), off)
        data += png
        off += len(png)
    path.write_bytes(struct.pack("<HHH", 0, 1, len(imgs)) + entries + data)
    return len(imgs)


def write_intro_logo(src):
    """Write `src`'s drawn (non-transparent) area as the intro screen's Lua data: a
    palette of its exact colours and one string per pixel row, two hex digits per
    pixel, `00` for a transparent one and `NN` for `palette[NN]`. Every colour is
    kept: quantizing loses the grey beak and turns the white brow cream."""
    art = src.crop(src.getchannel("A").getbbox())
    palette, index, rows = [], {}, []
    for y in range(art.height):
        row = ""
        for x in range(art.width):
            r, g, b, a = art.getpixel((x, y))
            if a < 128:
                row += "00"
                continue
            color = f"#{r:02x}{g:02x}{b:02x}"
            if color not in index:
                palette.append(color)
                index[color] = len(palette)
            row += f"{index[color]:02x}"
        rows.append(row)
    if len(palette) > 255:
        sys.exit(f"icon24.png has {len(palette)} colours; the intro logo holds at most 255")
    lines = [
        "-- GENERATED by crates/bemtvi-gui/assets/gen-icons.py from icon24.png. Do not edit:",
        "-- change the PNG and rerun the script.",
        "--",
        "-- The app icon's pixels, for the intro screen (intro.lua). `rows` holds one string",
        "-- per pixel row, two hex digits per pixel: `00` is transparent, `NN` is",
        "-- `palette[NN]`.",
        "btv._intro_logo = {",
        f"  width = {art.width},",
        f"  height = {art.height},",
        "  palette = {",
        *[f'    "{c}",' for c in palette],
        "  },",
        "  rows = {",
        *[f'    "{r}",' for r in rows],
        "  },",
        "}",
    ]
    INTRO_LOGO.write_text("\n".join(lines) + "\n")


def png_bytes(img):
    buf = io.BytesIO()
    img.save(buf, "PNG", optimize=True)
    return buf.getvalue()


def main():
    sources = load_sources("icon")
    renders = dict(sources)
    for s in UPSCALES:
        renders[s] = scaled(sources, s, upscale)
    missing = [s for s in ICO_SIZES if s not in renders]
    if missing:
        sys.exit(f"no source for .ico size(s) {missing}: draw icon<N>.png for each")

    OUT.mkdir(exist_ok=True)
    for stale in OUT.glob("bemtvi-*.png"):
        stale.unlink()
    for s, img in sorted(renders.items()):
        img.save(OUT / f"bemtvi-{s}.png", "PNG", optimize=True)

    n_app = write_ico(HERE / "bemtvi.ico", renders, ICO_SIZES)

    file_sources = load_sources("fileicon")
    file_renders = {s: scaled(file_sources, s, upscale_in_place) for s in FILE_ICO_SIZES}
    n_file = write_ico(HERE / "bemtvi-file.ico", file_renders, FILE_ICO_SIZES)

    write_intro_logo(renders[24])
    print(
        f"wrote {len(renders)} PNGs to {OUT}, a {n_app}-entry bemtvi.ico"
        f" and a {n_file}-entry bemtvi-file.ico"
    )


if __name__ == "__main__":
    main()
