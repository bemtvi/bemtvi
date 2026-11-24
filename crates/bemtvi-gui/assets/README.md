# bemtvi-gui assets

Brand assets for the native GUI client. The app icon is a bem-te-vi in **pixel
art**. The hand-drawn sources are the files every other size comes from:

| File                   | Role | Produced by |
|------------------------|------|-------------|
| `icon16.png`, `icon24.png`, `icon32.png` | sources | hand-drawn |
| `fileicon32.png`       | document icon source | hand-drawn |
| `icons/bemtvi-<N>.png` | 16, 24, 32, 48, 64 … 1024 | `gen-icons.py` |
| `bemtvi.ico`           | Windows (.exe), embedded by `build.rs` | `gen-icons.py` |
| `bemtvi-file.ico`      | Windows document icon, embedded by `build.rs` | `gen-icons.py` |
| `bemtvi.desktop`       | Linux desktop entry | hand-edited |

Each larger size is a **nearest-neighbour integer upscale** of the largest
source that divides it: 48 is `icon24.png` ×2, and 64 … 1024 are `icon32.png`
×2 … ×32. A filtered resize would blur the pixel edges, and so would a
fractional scale. The scaled art is centred on its canvas: `icon24.png` draws
in its top-left 23×23, so its ×2 lands 1px right and 1px down in the 48. The
sizes below 48 are separate drawings because a downscale
cannot keep pixel art legible. `gen-icons.py` copies every `icon<N>.png` as is,
so a new hand-drawn size only needs the file (plus an entry in `UPSCALES` /
`ICO_SIZES` for any size it newly makes reachable). The 24 and 48px sizes go only
into the `.ico`, since macOS iconsets have no slot for them.

The document icon (`fileicon32.png`) is the icon Windows shows on files
associated with bemtvi. `bemtvi-file.ico` holds it at 32, 64, 96, 128 and 256,
each a nearest-neighbour ×1 … ×8 of the 32. Unlike the app icon's upscales,
these keep the art where the source draws it rather than centring it (the
32 leaves 5px empty on the left and 1px on top, and every size keeps that). No 16 or 24 is drawn yet, so Windows samples the 32
down for those views. A hand-drawn `fileicon16.png` or `fileicon24.png` only
needs the file plus its sizes in `FILE_ICO_SIZES`.

Every derived file is committed. CI only copies them:

- **Linux AppImage**: `icons/bemtvi-256.png` (`.github/workflows/build.yml`).
- **macOS `.app`**: `iconutil` packs the `.icns` from `icons/` at package time.
- **Windows `.exe`**: `build.rs` embeds `bemtvi.ico` (icon ID 1, the exe's own
  icon) and `bemtvi-file.ico` (icon ID 2, index 1) at compile time, including a
  local `cargo build` on Windows. A file association uses the document icon by
  setting its `DefaultIcon` to `bemtvi-gui.exe,1`.

## Regenerating after the art changes

Edit the `icon<N>.png` sources (the layered working files are
`docs/icon.xcf` and `docs/icon16.xcf`), then run (needs Pillow):

```sh
python3 crates/bemtvi-gui/assets/gen-icons.py
```

It rewrites `icons/`, `bemtvi.ico` and `bemtvi-file.ico`. Commit them with the
source change.
