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
# Each one must be a whole multiple of some source, so no upscale is fractional (a
# fractional scale would blur or distort the pixel grid).
UPSCALES = [48, 64, 128, 256, 512, 1024]
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


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


def png_bytes(img):
    buf = io.BytesIO()
    img.save(buf, "PNG", optimize=True)
    return buf.getvalue()


def main():
    renders = {}
    for path in HERE.glob("icon*.png"):
        m = re.fullmatch(r"icon(\d+)\.png", path.name)
        if m:
            size = int(m.group(1))
            renders[size] = load(path.name, size)
    sources = dict(renders)
    for s in UPSCALES:
        bases = [b for b in sources if s % b == 0]
        if not bases:
            sys.exit(f"no icon<N>.png divides {s}: it cannot be an integer upscale")
        renders[s] = upscale(sources[max(bases)], s)
    missing = [s for s in ICO_SIZES if s not in renders]
    if missing:
        sys.exit(f"no source for .ico size(s) {missing}: draw icon<N>.png for each")

    OUT.mkdir(exist_ok=True)
    for stale in OUT.glob("bemtvi-*.png"):
        stale.unlink()
    for s, img in sorted(renders.items()):
        img.save(OUT / f"bemtvi-{s}.png", "PNG", optimize=True)

    # ICO with PNG-compressed entries (Vista+), smallest first.
    imgs = [(s, png_bytes(renders[s])) for s in ICO_SIZES]
    off = 6 + 16 * len(imgs)
    entries = data = b""
    for s, png in imgs:
        d = 0 if s >= 256 else s  # 0 encodes 256 in the ICO dir entry
        entries += struct.pack("<BBBBHHII", d, d, 0, 0, 1, 32, len(png), off)
        data += png
        off += len(png)
    (HERE / "bemtvi.ico").write_bytes(struct.pack("<HHH", 0, 1, len(imgs)) + entries + data)
    print(f"wrote {len(renders)} PNGs to {OUT} and a {len(imgs)}-entry bemtvi.ico")


if __name__ == "__main__":
    main()
