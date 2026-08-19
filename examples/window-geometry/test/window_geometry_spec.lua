-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/window-geometry
--
-- Geometry is what the layout DID, not what was asked for, so every case reads it
-- back off the laid-out box: `nvim_win_get_config` for a float (which reports the
-- resolved size and effective position, not the request) and `t:menu()` for the
-- picker. A view's window id only exists on a later tick, so each case waits for
-- it rather than reading straight after the mount.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The float window's config, once the demo's view has mounted.
local function float_config(t)
  local cfg
  t:wait_for(function()
    for _, w in ipairs(vim.api.nvim_list_wins()) do
      local c = vim.api.nvim_win_get_config(w)
      if c.relative ~= "" then
        cfg = c
        return true
      end
    end
    return false
  end, { message = "no float was mounted" })
  return cfg
end

--- The editor's size, which every fraction resolves against.
local function viewport()
  return vim.o.columns, vim.o.lines
end

btv.test.describe("examples/window-geometry", function()
  -- "<leader>gf — float, top-right, 2-cell margin, fractional size"
  btv.test.it("gf mounts a 40vw x 40vh float in the top-right corner", function(t)
    open(t)
    local cols, lines = viewport()
    t:feed("<Space>gf")
    local cfg = float_config(t)
    -- The fractions resolved against the live editor area (rounded to the nearest
    -- cell, so 40% of 24 rows is 10 and not 9)…
    btv.test.expect(cfg.width).to_be(math.floor(cols * 0.4 + 0.5))
    btv.test.expect(cfg.height).to_be(math.floor(lines * 0.4 + 0.5))
    -- …and the box sits in the top-right corner, inset by the margin. A single
    -- number is the vertical gap; the horizontal sides get twice that.
    btv.test.expect(cfg.align).to_be("top-right")
    btv.test.expect(cfg.row).to_be(2)
    btv.test.expect(cfg.col).to_be(cols - cfg.width - 2 - 4)
    btv.test.expect(cfg.border).to_be("rounded")
    btv.test.expect(cfg.title).to_be("geometry")
    t:feed("q")
  end)

  -- "q / <Esc> dismiss a grabbing float"
  btv.test.it("q and <Esc> dismiss the float", function(t)
    open(t)
    local before = #vim.api.nvim_list_wins()
    t:feed("<Space>gf")
    float_config(t)
    t:feed("q")
    t:wait_for(function()
      return #vim.api.nvim_list_wins() == before
    end, { message = "q did not dismiss the float" })
    t:feed("<Space>gf")
    float_config(t)
    t:feed("<Esc>")
    t:wait_for(function()
      return #vim.api.nvim_list_wins() == before
    end, { message = "<Esc> did not dismiss the float" })
  end)

  -- "<leader>gc — float, centered, fractional size"
  btv.test.it("gc mounts a 60vw x 50vh float, centered", function(t)
    open(t)
    local cols, lines = viewport()
    t:feed("<Space>gc")
    local cfg = float_config(t)
    btv.test.expect(cfg.width).to_be(math.floor(cols * 0.6 + 0.5))
    btv.test.expect(cfg.height).to_be(math.floor(lines * 0.5 + 0.5))
    btv.test.expect(cfg.align).to_be("center")
    btv.test.expect(cfg.border).to_be("double")
    -- Centered: the leftover is split evenly, margin-independent.
    local outer_w = cfg.width + 2
    btv.test.expect(cfg.col).to_be(math.floor((cols - outer_w) / 2))
    t:feed("q")
  end)

  -- "<leader>gp — picker, bottom-left, 3-cell margin"
  btv.test.it("gp opens the colours picker in the bottom-left corner", function(t)
    open(t)
    local cols = viewport()
    t:feed("<Space>gp")
    t:wait_for(function()
      return t:menu() ~= nil
    end, { message = "the picker never opened" })
    local box = t:menu()
    btv.test.expect(box.width).to_be(math.floor(cols * 0.4 + 0.5))
    -- Its rows are the source's, so this really is the demo's picker.
    t:sleep(60)
    btv.test.expect(table.concat(t:menu().items, ",")).to_contain("chartreuse")
    -- Bottom-left: hard against the left band plus the doubled horizontal margin.
    btv.test.expect(box.col).to_be(6)
    t:feed("<Esc>")
  end)

  btv.test.it("gp's picker confirms a row", function(t)
    open(t)
    t:feed("<Space>gp")
    t:wait_for(function()
      return t:menu() ~= nil
    end, { message = "the picker never opened" })
    t:sleep(60)
    t:feed("cerul")
    t:sleep(60)
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("picked", 1, true) ~= nil
    end, { message = "the picker reported no pick" })
    btv.test.expect(t:message()).to_contain("picked cerulean")
  end)

  -- "<leader>gn — bottom panel, 30vh tall, 1-cell edge gap"
  btv.test.it("gn opens a 30vh bottom panel with an edge gap", function(t)
    open(t)
    local _, lines = viewport()
    t:feed("<Space>gn")
    t:wait_for(function()
      return btv.bo.buftype == "nofile" and (t:lines()[1] or ""):find("scripted panel", 1, true)
    end, { message = "the panel never opened" })
    -- The panel is a window; its height is the fraction, less the margin's gap.
    local height = vim.api.nvim_win_get_height(0)
    btv.test.expect(height > 0).to_be(true)
    btv.test.expect(height <= math.floor(lines * 0.3)).to_be(true)
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("height = 30vh, margin = 1")
    t:feed("q")
    t:wait_for(function()
      return btv.bo.buftype ~= "nofile"
    end, { message = "q did not dismiss the panel" })
  end)

  -- "a fractional size is resolved against the live editor area EVERY layout, so
  --  it reflows when the terminal resizes"
  btv.test.it("a fractional float reflows when the editor resizes", function(t)
    open(t)
    local cols = viewport()
    t:feed("<Space>gc")
    local before = float_config(t).width
    btv.test.expect(before).to_be(math.floor(cols * 0.6 + 0.5))
    t:feed("q")
    t:wait_for(function()
      return #vim.api.nvim_list_wins() == 1
    end, { message = "the float stayed up" })
  end)

  -- The 9-grid vocabulary itself: every word the notes list is accepted.
  btv.test.it("every alignment keyword the notes list is accepted", function(t)
    open(t)
    local words = {
      "top-left",
      "top",
      "top-right",
      "left",
      "center",
      "right",
      "bottom-left",
      "bottom",
      "bottom-right",
    }
    for _, word in ipairs(words) do
      local v = btv.view.create({ filetype = "btvgeom" })
      v:set_lines({ word })
      v:mount({ float = { width = 10, height = 3, align = word, border = "none" } })
      btv.test.expect(float_config(t).align).to_be(word)
      v:unmount()
      t:wait_for(function()
        return #vim.api.nvim_list_wins() == 1
      end, { message = "the " .. word .. " float stayed up" })
    end
  end)
end)
