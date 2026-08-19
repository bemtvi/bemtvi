-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/image-preview
--
-- What the clients do with a preview is pixels (a terminal graphics protocol, a
-- GPU texture, an `<img>`), which a headless spec cannot judge. What it CAN hold
-- to account is the whole editor-side contract the notes make: the buffer is
-- inert, its bytes are never loaded as text, a non-image is unaffected, and
-- turning the option off puts the raw bytes back.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open a file and settle the read.
local function open(t, path)
  t:cmd("e " .. path)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/image-preview", function()
  btv.test.it("the config turns previews on", function(t)
    btv.test.expect(btv.o.imagepreview).to_be(true)
    -- "A line-number gutter is left off here so the picture fills the window body."
    btv.test.expect(btv.o.number).to_be(false)
  end)

  -- "opening a file whose extension is a known image type shows the *picture*
  --  instead of the file's raw bytes"
  btv.test.it("try-it — an image opens as a preview, not as bytes", function(t)
    open(t, DIR .. "/sample.png")
    btv.test.expect(btv.buf.name(0)).to_contain("sample.png")
    -- The PNG's bytes are never loaded as text: the buffer holds none of them.
    local text = table.concat(t:lines(), "\n")
    btv.test.expect(text).never.to_contain("PNG")
    btv.test.expect(text).never.to_contain("IHDR")
  end)

  -- "The buffer is inert — its bytes are never loaded as text — so it is a
  --  preview, not an editable buffer."
  btv.test.it("try-it — the preview buffer is inert", function(t)
    open(t, DIR .. "/sample.png")
    local before = t:lines()
    t:feed("ihello<Esc>")
    t:feed("dd")
    t:feed("x")
    btv.test.expect(t:lines()).to_equal(before)
    btv.test.expect(btv.bo.modified).to_be(false)
  end)

  -- ":e init.lua  a NON-image opens as ordinary text, unchanged"
  btv.test.it("try-it — a non-image is unaffected", function(t)
    open(t, DIR .. "/init.lua")
    btv.test.expect(btv.bo.filetype).to_be("lua")
    btv.test.expect(t:line(1)).to_contain("bemtvi image previews")
    btv.test.expect(btv.bo.modifiable).to_be(true)
    t:feed("ggOtyped<Esc>")
    btv.test.expect(t:line(1)).to_be("typed")
    t:feed("u")
  end)

  -- ":set noimagepreview  turn it off; now :e sample.png shows the raw bytes"
  btv.test.it("try-it — with the option off, the raw bytes come back", function(t)
    t:cmd("set noimagepreview")
    open(t, DIR .. "/sample.png")
    local text = table.concat(t:lines(), "\n")
    -- A PNG's magic is right at the start of the file.
    btv.test.expect(text).to_contain("PNG")
    t:cmd("set imagepreview")
  end)

  btv.test.it("…and turning it back on previews again", function(t)
    t:cmd("set noimagepreview")
    open(t, DIR .. "/sample.png")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("PNG")
    t:cmd("set imagepreview")
    open(t, DIR .. "/sample.png")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("PNG")
  end)

  -- "The option is a normal btv.* option, so `btv.o` / `vim.o` / `:set` all reach it."
  btv.test.it("all three surfaces reach the option", function(t)
    -- Lua writes are queued, so settle a tick before reading the mirror back.
    btv.o.imagepreview = false
    t:feed("<Esc>")
    btv.test.expect(vim.o.imagepreview).to_be(false)
    vim.o.imagepreview = true
    t:feed("<Esc>")
    btv.test.expect(btv.o.imagepreview).to_be(true)
    t:cmd("set noimagepreview")
    btv.test.expect(btv.o.imagepreview).to_be(false)
    t:cmd("set imagepreview")
    btv.test.expect(btv.o.imagepreview).to_be(true)
  end)

  -- The extension list the notes give.
  btv.test.it("a NEW file with an image extension is an ordinary buffer", function(t)
    local missing = btv.test.tempdir() .. "/nope.png"
    t:cmd("e " .. missing)
    -- There is nothing to preview, so it is the file you asked to create.
    t:feed("itext<Esc>")
    btv.test.expect(t:line(1)).to_be("text")
    t:cmd("w")
    btv.test.expect(btv.await(btv.fs.read(missing))).to_be("text\n")
  end)
end)
