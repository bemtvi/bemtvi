-- Markdown rendering for doc popups.
--
-- bemtvi renders markdown in its read-only doc popups — LSP hover (press `K` over a
-- symbol in an LSP-backed buffer), completion documentation, and signature help all
-- show *rendered* markdown (bold/headings styled, `#`/`**`/fences stripped) instead
-- of raw markdown text. That happens natively; no config is needed.
--
-- This example demonstrates the same engine you can call yourself. `btv.markdown.to_view`
-- turns a markdown string into view-ready `{ lines, decor }`; we drop that into an
-- `btv.view.component` mounted as a **real floating window**, so it scrolls: a long
-- document pages with the wheel or `j`/`k`/`<C-d>`/`<C-u>`, and `q` / `<Esc>` closes it.
-- Open `sample.md` and press `K` (or run `:MarkdownFloat`).
--
-- Code blocks get real per-language syntax highlighting for free: `to_view` keeps the
-- ``` fences (hidden behind a blank overlay) and we mount the view `filetype = "markdown"`,
-- so the grammar's injections highlight the fenced code in its own language. (That needs
-- the language's grammar installed, e.g. `:TSInstall rust`.)
--
--   Run:  cargo run -p bemtvi -- --config-dir examples/markdown examples/markdown/sample.md

-- The rendered-markdown float: an `btv.view.component` whose `render` maps the source
-- markdown — handed in as a prop — to the view-ready output `btv.markdown.to_view` builds.
-- The content is fixed at mount, so `render` runs once; mounted as a float, it scrolls
-- like any window.
local MarkdownFloat = btv.view.component({
  setup = function(ctx)
    ctx.wo.wrap = true -- wrap long paragraphs within the float, like the native hover
    ctx.keymap_set("n", "q", ctx.close)
    ctx.keymap_set("n", "<Esc>", ctx.close)
    return { src = ctx.props.src }
  end,
  render = function(state)
    return btv.markdown.to_view(state.src)
  end,
})

-- Render the current buffer's markdown into a centered floating window. `filetype =
-- "markdown"` turns on the grammar (and its code-fence injections) for the view buffer.
local function open_markdown_float()
  local src = table.concat(btv.buf.lines(btv.buf.current(), 0, -1), "\n")
  MarkdownFloat.mount({
    name = "[Rendered Markdown]",
    filetype = "markdown",
    props = { src = src },
    float = {
      relative = "editor",
      width = "80%",
      height = "80%",
      align = "center",
      border = "rounded",
      title = " rendered markdown ",
      grab = true, -- modal: focus stays in the float so j/k scroll it; `q` closes
    },
  })
end

-- Expose it as both a key (over the buffer) and a command.
btv.keymap.set("n", "K", open_markdown_float, { desc = "Render this buffer's markdown in a float" })
btv.command(
  "MarkdownFloat",
  open_markdown_float,
  { desc = "Render the current buffer's markdown in a floating window" }
)
