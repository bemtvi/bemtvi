# Image previews

Open an image file and nxvim renders the **picture** inline instead of its raw
bytes — image.nvim's behavior, built in, with no terminal-specific plugin to
wire up. Each client draws it the native way: ratatui-image in the terminal
(Kitty/Sixel/iTerm2 graphics, with a block-cell fallback), a textured quad in the
GPU GUI, and an out-of-band `<img>` in the browser build.

The feature is opt-in through one option.

## Enabling it

`'imagepreview'` (nxvim-native, **off by default**) turns it on:

```lua
nx.o.imagepreview = true        -- or :set imagepreview
```

With it on, opening a file with an image extension —
`png`, `jpg`/`jpeg`, `gif`, `bmp`, `webp`, `tiff`/`tif`, `ico`, `tga`, `qoi`,
`ppm`/`pgm`/`pbm`/`pnm`
(case-insensitive) — loads it as an **inert preview buffer**: the bytes are never
decoded as text, the rope stays empty, and the window projects an image the
client paints. The path is retained, so the buffer still has a name. With the
option off, an image file opens as ordinary binary text, exactly as before.

There are no special commands or keymaps — opening is the usual `:e <path>` or a
file argument on the command line.
