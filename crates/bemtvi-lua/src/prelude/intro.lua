-- The intro screen: what an editor started with no file shows, the way vim and neovim
-- do. The app icon is drawn in the terminal itself, two pixels per cell with half-block
-- characters, above the name, the version and the few commands a newcomer needs. It
-- goes away for good the moment the editor is used.
--
-- Drawn as `virt_lines` on the empty startup buffer, in its own namespace, so the buffer
-- is never modified: the cursor stays on line 1 and the block is centred in the text area
-- below it. The pixels come from `intro_logo.lua`, generated from the icon PNG. See
-- docs/plans/2026-10-21-intro-screen.md.
--
-- Armed only by the interactive binaries (`ServerInit.show_intro` natively, `eh_new` on
-- the web), so headless runs and the test suites never see it. A config opts out with
-- `btv.g.intro = false`.

btv.intro = btv.intro or {}

local NS = btv.ns.create("bemtvi-intro")
local LOGO = btv._intro_logo

-- `start_buf` / `start_win`: the startup buffer and window, recorded when the intro is
-- armed (before `init.lua`), so it is drawn there even if something else has focus by
-- the time the client attaches (the first-run plugin offer is a focused float). `buf`:
-- the buffer the intro is drawn on while it shows. `subs`: the subscriptions that
-- redraw or dismiss it, dropped on dismissal.
local state = { armed = false, start_buf = nil, start_win = nil, buf = nil, subs = {} }

-- The text under the logo: the name, the version, then the commands, each a
-- `{ command, what }` pair. `:help` is listed only when `bemtvi-help` provides it
-- (otherwise the command is just a pointer to that plugin).
local function command_rows()
  local rows = {
    { ":q<Enter>", "to exit" },
    { ":Plugins<Enter>", "to manage plugins" },
  }
  if btv._user_commands["help"] then
    table.insert(rows, 2, { ":help<Enter>", "for help" })
  end
  return rows
end

-- The highlight group for one cell of the logo, defined on use. A cell is two pixels:
-- `top` and `bottom` are palette indices, `0` for transparent. A transparent half takes
-- `bg`, the box's background (`nil` when the scheme has none), so nothing behind the
-- intro shows through it.
local function logo_group(top, bottom, bg)
  local name = "BtvIntroLogo" .. top .. "_" .. bottom
  local spec
  if top > 0 and bottom > 0 then
    spec = { fg = LOGO.palette[top], bg = LOGO.palette[bottom] }
  elseif top > 0 then
    spec = { fg = LOGO.palette[top], bg = bg }
  else
    spec = { fg = LOGO.palette[bottom], bg = bg }
  end
  btv.hl.define(0, name, spec)
  return name
end

-- The palette index of pixel `x` (1-based) in pixel row `y` (1-based); `0` past the
-- bottom edge or for a transparent pixel.
local function pixel(y, x)
  local row = LOGO.rows[y]
  if not row then
    return 0
  end
  return tonumber(row:sub(2 * x - 1, 2 * x), 16)
end

-- The logo as virtual-line chunk lists, one per cell row: the upper half block `▀`
-- carries the top pixel as its foreground and the bottom one as its background; a cell
-- whose top pixel is transparent uses `▄` instead, and an empty cell is a space in
-- `box` (the box's background group). Runs of the same group share one chunk.
local function logo_lines(bg, box)
  local lines = {}
  for y = 1, LOGO.height, 2 do
    local chunks = {}
    for x = 1, LOGO.width do
      local top, bottom = pixel(y, x), pixel(y + 1, x)
      local text, group = " ", box
      if top > 0 then
        text, group = "▀", logo_group(top, bottom, bg)
      elseif bottom > 0 then
        text, group = "▄", logo_group(0, bottom, bg)
      end
      local last = chunks[#chunks]
      if last and last[2] == group then
        last[1] = last[1] .. text
      else
        chunks[#chunks + 1] = { text, group }
      end
    end
    lines[#lines + 1] = chunks
  end
  return lines
end

-- The text block: each line as `{ width, chunks, centred }`, the width in cells. The
-- name and version are centred on their own; the commands share one indent, so their
-- descriptions line up.
local function text_lines()
  local version = (btv.version():match("(%d[%w.%-]*)") or "")
  local lines = {
    { 6, { { "bemtvi", "BtvIntroTitle" } }, true },
    { 8 + #version, { { "version " .. version, "BtvIntroText" } }, true },
    { 0, {}, true },
  }
  local rows = command_rows()
  local widest = 0
  for _, row in ipairs(rows) do
    widest = math.max(widest, #row[1])
  end
  for _, row in ipairs(rows) do
    local gap = string.rep(" ", widest - #row[1] + 2)
    local width = 6 + widest + 2 + #row[2]
    lines[#lines + 1] = {
      width,
      {
        { "type  ", "BtvIntroText" },
        { row[1], "BtvIntroKey" },
        { gap .. row[2], "BtvIntroText" },
      },
      false,
    }
  end
  return lines
end

-- The editor background as `"#rrggbb"`, or `nil` when the scheme sets none.
local function normal_bg()
  local bg = btv.hl.get(0, { name = "Normal", link = false }).bg
  return bg and string.format("#%06x", bg) or nil
end

-- The box variant of a text group: its colours and attributes on the box background,
-- defined on use (and again on every draw, so it follows the scheme).
local function boxed(group, bg)
  local def = btv.hl.get(0, { name = group, link = false })
  local name = group .. "Box"
  btv.hl.define(0, name, {
    fg = def.fg,
    bold = def.bold,
    italic = def.italic,
    underline = def.underline,
    bg = bg,
  })
  return name
end

-- How many blank cells the box keeps around its content, on each side and above and
-- below.
local MARGIN = 2

-- Draw the intro in `win` (on `buf`), centred in its text area, replacing any earlier
-- drawing. It is a solid box in the editor background, so nothing under it shows
-- through (the 'colorcolumn' rulers, a dock's edge). The logo needs a truecolor
-- client and room; without either only the text is drawn, and with no room even for
-- that, nothing.
local function draw(buf, win)
  btv.buf.clear_namespace(buf, NS, 0, -1)
  local width = btv.win.width(win) - btv.win.textoff(win)
  local height = btv.win.height(win)
  local bg = normal_bg()
  local box = nil
  local groups = {}
  if bg then
    box = "BtvIntroBox"
    btv.hl.define(0, box, { bg = bg })
    for _, g in ipairs({ "BtvIntroTitle", "BtvIntroText", "BtvIntroKey" }) do
      groups[g] = boxed(g, bg)
    end
  end
  local text = text_lines()
  local text_width = 0
  for _, line in ipairs(text) do
    text_width = math.max(text_width, line[1])
  end

  -- The rows of the box, each `{ width, chunks, centred }`: the logo (when it fits)
  -- and a blank row, then the text.
  local rows = {}
  local logo = btv.ui.caps().truecolor and logo_lines(bg, box) or nil
  local content_width = text_width
  if logo then
    local box_width = math.max(LOGO.width, text_width) + 2 * MARGIN
    if width >= box_width and height > #text + #logo + 1 + 2 * MARGIN then
      for _, chunks in ipairs(logo) do
        rows[#rows + 1] = { LOGO.width, chunks, true }
      end
      rows[#rows + 1] = { 0, {}, true }
      content_width = math.max(LOGO.width, text_width)
    end
  end
  local box_width = content_width + 2 * MARGIN
  if width < box_width or height <= #text + #rows + 2 * MARGIN then
    return
  end
  for _, line in ipairs(text) do
    rows[#rows + 1] = line
  end

  -- The text block is laid out once (the commands share one indent, so their columns
  -- line up); its rows are offset inside the content area by that block's indent.
  local text_indent = math.floor((content_width - text_width) / 2)
  local left = math.floor((width - box_width) / 2)
  local function box_row(offset, row_width, chunks)
    local out = { { string.rep(" ", left) }, { string.rep(" ", MARGIN + offset), box } }
    for _, chunk in ipairs(chunks) do
      out[#out + 1] = { chunk[1], groups[chunk[2]] or chunk[2] }
    end
    out[#out + 1] = { string.rep(" ", box_width - MARGIN - offset - row_width), box }
    return out
  end

  local total = #rows + 2 * MARGIN
  -- Line 1 (the empty buffer line) holds row 0; the box's top row is the one that
  -- centres it in the window, and never above row 1.
  local first = math.max(1, math.floor((height - total) / 2))
  local virt = {}
  for _ = 2, first do
    virt[#virt + 1] = { { " " } }
  end
  for _ = 1, MARGIN do
    virt[#virt + 1] = box_row(0, 0, {})
  end
  for _, row in ipairs(rows) do
    local offset = row[3] and math.floor((content_width - row[1]) / 2) or text_indent
    virt[#virt + 1] = box_row(offset, row[1], row[2])
  end
  for _ = 1, MARGIN do
    virt[#virt + 1] = box_row(0, 0, {})
  end
  btv.buf.set_extmark(buf, NS, 0, 0, { virt_lines = virt })
end

-- Remove the intro for good: clear its drawing and drop every subscription.
local function dismiss()
  if state.buf then
    btv.buf.clear_namespace(state.buf, NS, 0, -1)
  end
  for _, id in ipairs(state.subs) do
    btv.off(id)
  end
  state.subs, state.buf = {}, nil
end

-- Whether this is a session the intro belongs to: started with no file, still on the
-- untouched unnamed buffer, and not opted out by the config.
local function wanted(buf)
  if btv.g.intro == false or #btv.argv() > 0 then
    return false
  end
  if btv.buf.name(buf) ~= "" or btv.bo[buf].modified then
    return false
  end
  local lines = btv.buf.lines(buf, 0, -1)
  return #lines == 1 and lines[1] == ""
end

-- How many windows show `buf`.
local function windows_showing(buf)
  local n = 0
  for _, w in pairs(btv._wins or {}) do
    if w.buffer == buf then
      n = n + 1
    end
  end
  return n
end

local function show()
  local buf, win = state.start_buf, state.start_win
  if not buf or btv.win.buf(win) ~= buf or not wanted(buf) then
    return
  end
  btv.hl.fallback("BtvIntroTitle", { link = "Title" })
  btv.hl.fallback("BtvIntroText", { link = "Comment" })
  btv.hl.fallback("BtvIntroKey", { link = "Special" })
  state.buf = buf
  draw(buf, win)
  local function redraw()
    if state.buf and btv.win.buf(win) == state.buf then
      draw(state.buf, win)
    end
  end
  state.subs = {
    btv.on("TextChanged", { buffer = buf }, dismiss),
    btv.on("TextChangedI", { buffer = buf }, dismiss),
    btv.on("BufLeave", { buffer = buf }, dismiss),
    -- `:e file` on the untouched startup buffer reuses it rather than leaving it.
    btv.on("BufReadPost", { buffer = buf }, dismiss),
    btv.on("BufNewFile", { buffer = buf }, dismiss),
    btv.on("BufFilePost", { buffer = buf }, dismiss),
    -- A split of the startup window would show the intro twice; another window (a
    -- dock, a popup) leaves it be, and the resize re-centres it.
    btv.on("WinNew", function()
      if windows_showing(buf) > 1 then
        dismiss()
      end
    end),
    btv.on("WinResized", redraw),
    -- A scheme that opens with `hi clear` drops the logo's groups; draw them again.
    btv.on("ColorScheme", redraw),
  }
end

-- Arm the intro for this session: it shows at the first client attach (`UIEnter`), the
-- first point the window size and the client's colours are known. Called by the
-- interactive binaries before `init.lua`, so a config can still set `btv.g.intro = false`.
function btv.intro._arm()
  if state.armed then
    return
  end
  state.armed = true
  state.start_buf, state.start_win = btv.buf.current(), btv.win.current()
  local id
  id = btv.on("UIEnter", function()
    btv.off(id)
    show()
  end)
end
