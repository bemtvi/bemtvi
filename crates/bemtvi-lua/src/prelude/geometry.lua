-- btv._geom — the shared window-geometry normalizer.
--
-- One place validates the unified geometry vocabulary every surface (floats /
-- `nvim_open_win`, `btv.view`, pickers, the panel) accepts, and emits the wire
-- shape the Rust bridges marshal: size specs become strings the server parses into
-- an `Extent` (cells or a `vw`/`vh`/`%` fraction), the alignment word is validated
-- against the closed 9-grid set, and `margin` is expanded to the
-- `[top, right, bottom, left]` array the ops carry. Each helper fails *loud* on a
-- bad value (per the no-silent-fallback rule) rather than mispositioning silently.

btv._geom = {}

-- The closed set of high-level alignment words. `centre` is a spelling alias for
-- `center` (matched server-side too).
local ALIGN = {
  ["top-left"] = true,
  ["top"] = true,
  ["top-right"] = true,
  ["left"] = true,
  ["center"] = true,
  ["centre"] = true,
  ["right"] = true,
  ["bottom-left"] = true,
  ["bottom"] = true,
  ["bottom-right"] = true,
}

local ALIGN_LIST =
  "top-left, top, top-right, left, center, right, bottom-left, bottom, bottom-right"

-- Validate an alignment word, returning it unchanged (or nil for nil). A wrong
-- type / unknown word errors at the caller's level.
function btv._geom.align(a)
  if a == nil then
    return nil
  end
  if type(a) ~= "string" or not ALIGN[a] then
    error("invalid align " .. btv._geom._show(a) .. " (expected one of: " .. ALIGN_LIST .. ")", 3)
  end
  return a
end

-- Normalize a size spec to the wire string: a number is a cell count (floored), a
-- string ("40" / "50vw" / "30vh" / "50%") passes through. `nil` ⇒ nil (the surface
-- decides what an omitted size means — a picker default, or a loud error).
function btv._geom.size(s)
  if s == nil then
    return nil
  end
  if type(s) == "number" then
    return tostring(math.floor(s))
  end
  if type(s) == "string" then
    return s
  end
  error(
    "invalid size " .. btv._geom._show(s) .. " (expected a number or a 'NNvw'/'NNvh'/'NN%' string)",
    3
  )
end

-- Terminal cells are about twice as tall as they are wide, so an equal *cell*
-- count reads as a bigger gap vertically than horizontally. A single `margin`
-- number is the "visually uniform" shorthand: the vertical sides get this many
-- cells and the horizontal sides get twice as many, so the gap looks even on
-- screen. The explicit forms (`{vertical, horizontal}` / `{t,r,b,l}` / `{top=, …}`)
-- are taken literally — no aspect correction.
local CELL_ASPECT = 2

-- Normalize a margin to the `[top, right, bottom, left]` array the wire carries.
-- Accepts `nil` (no margin), a number (vertical cells; horizontal is 2x — see
-- above), `{vertical, horizontal}`, `{top, right, bottom, left}`, or
-- `{top=, right=, bottom=, left=}`.
function btv._geom.margin(m)
  if m == nil then
    return { 0, 0, 0, 0 }
  end
  if type(m) == "number" then
    -- Vertical = `m` cells; horizontal = 2x, so both look like the same gap.
    local horiz = m * CELL_ASPECT
    return { m, horiz, m, horiz }
  end
  if type(m) == "table" then
    if m.top or m.right or m.bottom or m.left then
      return { m.top or 0, m.right or 0, m.bottom or 0, m.left or 0 }
    end
    local n = #m
    if n == 2 then
      return { m[1], m[2], m[1], m[2] }
    end
    if n == 4 then
      return { m[1], m[2], m[3], m[4] }
    end
  end
  error(
    "invalid margin (expected a number, {vertical, horizontal}, {top, right, bottom, left}, "
      .. "or {top=, right=, bottom=, left=})",
    3
  )
end

-- A short, safe display of a bad value for an error message.
function btv._geom._show(v)
  if type(v) == "string" then
    return "'" .. v .. "'"
  end
  return tostring(v)
end
