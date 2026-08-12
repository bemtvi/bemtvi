# bemtvi-gui assets

Brand assets for the native GUI client. The **single source of truth is
`bemtvi.svg`** — still the old "NX" monogram on a dark rounded square (the
artwork has not been redrawn for the bemtvi name yet). Everything else is
derived from it, by platform:

| File            | Platform | Produced by | When |
|-----------------|----------|-------------|------|
| `bemtvi.svg`     | source   | hand-edited | — |
| `bemtvi.desktop` | Linux    | hand-edited | — |
| `bemtvi.png`     | Linux (AppImage) | `rsvg-convert` at package time | CI |
| `bemtvi.icns`    | macOS (.app)     | `rsvg-convert` + `iconutil` at package time | CI |
| `bemtvi.ico`     | Windows (.exe)   | **committed binary** (see below) | manual |

The Linux PNG and macOS `.icns` are generated fresh from the SVG during the
release build (`.github/workflows/build.yml`), so they are not committed. The
Windows `.ico` **is** committed, because it is embedded by `build.rs` at compile
time (including local `cargo build` on Windows) and CI runners have no SVG
rasterizer on Windows.

## Regenerating `bemtvi.ico` after editing `bemtvi.svg`

The `.ico` is the only derived asset that can drift from the SVG. Regenerate it
whenever the brand changes. With `rsvg-convert` (librsvg) + Python 3:

```sh
cd crates/bemtvi-gui/assets
tmp=$(mktemp -d)
for s in 16 24 32 48 64 128 256; do
  rsvg-convert -w "$s" -h "$s" bemtvi.svg -o "$tmp/$s.png"
done
python3 - "$tmp" <<'PY'
import struct, sys
tmp = sys.argv[1]
sizes = [16, 24, 32, 48, 64, 128, 256]
imgs = [(s, open(f"{tmp}/{s}.png", "rb").read()) for s in sizes]
off = 6 + 16 * len(imgs)
entries = data = b""
for s, png in imgs:
    d = 0 if s >= 256 else s  # 0 encodes 256 in the ICO dir entry
    entries += struct.pack("<BBBBHHII", d, d, 0, 0, 1, 32, len(png), off)
    data += png; off += len(png)
open("bemtvi.ico", "wb").write(struct.pack("<HHH", 0, 1, len(imgs)) + entries + data)
PY
```

(On macOS without `rsvg-convert`, `qlmanage -t -s 1024 -o "$tmp" bemtvi.svg` then
`sips -z $s $s` per size is a workable substitute for the per-size renders.)
