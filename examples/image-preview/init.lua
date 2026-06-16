-- ~~~ nxvim image previews: open an image, see the picture ~~~
--
-- Run it (from the repo root) against the sample image:
--
--     NXVIM_CONFIG=examples/image-preview \
--       cargo run -p nxvim -- examples/image-preview/sample.png
--
-- With `'imagepreview'` on, opening a file whose extension is a known image type
-- (png/jpg/jpeg/gif/bmp/webp/tiff/ico/tga/qoi/pnm) shows the *picture* instead of
-- the file's raw bytes. The buffer is inert — its bytes are never loaded as text —
-- so it is a preview, not an editable buffer.
--
-- The TUI renders the image with the best graphics protocol your terminal speaks
-- (Kitty / Sixel / iTerm2), falling back to unicode half-blocks on a terminal that
-- supports none. So for a real picture (not a blocky approximation), run this in a
-- graphics-capable terminal — e.g. Kitty, WezTerm, Ghostty, or iTerm2.
--
-- TRY IT:
--   :e sample.png         open the image (this dir's sample) — preview appears
--   :e some-other.jpg     any image file works
--   :e init.lua           a NON-image opens as ordinary text, unchanged
--   :set noimagepreview   turn it off; now :e sample.png shows the raw bytes
--
-- The option is a normal nx.* option, so `nx.o` / `vim.o` / `:set` all reach it.

-- Turn previews on (off by default). `nx.o` is the canonical surface; `vim.o`
-- and `:set imagepreview` are equivalent.
nx.o.imagepreview = true

-- Nothing else is needed — opening an image is all it takes. A line-number gutter
-- is left off here so the picture fills the window body.
vim.o.number = false
