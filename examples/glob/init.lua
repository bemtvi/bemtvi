-- ~~~ bemtvi btv.glob: shell-style path patterns, compiled to a cached regex ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/glob \
--       cargo run -p bemtvi -- examples/glob/sample.txt
--
-- Lua has no glob support and its patterns are not a substitute (no alternation, no
-- `**`, a different escaping model). `btv.glob` is the real thing: `globset` parses
-- the pattern, the translation is compiled as a Rust regex, and that regex is CACHED
-- by pattern + options — so matching one glob over thousands of paths costs a single
-- parse.
--
--     btv.glob.match(pattern, path, opts) -> boolean      one-shot, cache-backed
--     btv.glob.compile(pattern, opts)     -> :test/:regex/:pattern
--     btv.glob.set(patterns, opts)        -> :test/:matches  (one RegexSet pass)
--     btv.glob.any / .filter / .to_regex / .is_glob
--
-- Two things to know, both demonstrated below:
--   * `*` STOPS at `/` (shell/gitignore/LSP semantics). `**` crosses it.
--   * path style is an explicit option, NOT the machine you're running on.
--
-- The sample buffer is a list of paths, one per line — the commands below match
-- against those lines, so you can edit them and re-run.

-- Read the buffer's non-empty lines as the path list every command works over.
local function paths()
  local out = {}
  for _, line in ipairs(btv.buf.lines(0, 0, -1)) do
    local trimmed = btv.str.trim(line)
    if trimmed ~= "" and not trimmed:match("^#") then
      out[#out + 1] = trimmed
    end
  end
  return out
end

-- 1. :Glob <pattern> — match every path in the buffer against one glob.
--    Type this, then see the verdicts split into matched / not matched:
--
--        :Glob **/*.rs
--        :Glob *.rs          ( note: matches ONLY the root-level .rs files )
--        :Glob src/**/*.{rs,toml}
--        :Glob **/[!_]*.lua
vim.api.nvim_create_user_command("Glob", function(a)
  local hit, miss = {}, {}
  for _, p in ipairs(paths()) do
    table.insert(btv.glob.match(a.args, p) and hit or miss, p)
  end
  vim.notify(
    ("glob %q\n  matched (%d):\n    %s\n  not matched (%d):\n    %s"):format(
      a.args,
      #hit,
      table.concat(hit, "\n    "),
      #miss,
      table.concat(miss, "\n    ")
    )
  )
end, { nargs = 1, desc = "Match every path in the buffer against a glob (btv.glob.match)" })

-- 2. :GlobRegex <pattern> — show the regex a glob translates to. This is the whole
--    implementation strategy made visible, and the fastest way to debug a glob that
--    isn't matching what you expected. Type this, and see how the `*` case carries a
--    `[^/]` separator exclusion that the `**` case does not:
--
--        :GlobRegex *.lua
--        :GlobRegex **/*.lua
vim.api.nvim_create_user_command("GlobRegex", function(a)
  vim.notify(
    ("glob %q translates to:\n  %s\n\nwith literal_separator = false:\n  %s"):format(
      a.args,
      btv.glob.to_regex(a.args),
      btv.glob.to_regex(a.args, { literal_separator = false })
    )
  )
end, { nargs = 1, desc = "Show the regex a glob compiles to (btv.glob.to_regex)" })

-- 3. :GlobStar — the `*`-stops-at-`/` default, side by side with its two escapes.
--    Type it and see one path judged three ways.
vim.api.nvim_create_user_command("GlobStar", function()
  local path = "conf/nvim/init.lua"
  vim.notify(
    ([[
path = %q

  btv.glob.match("*.lua", path)                              --> %s
      `*` does not cross `/`  (shell / gitignore / LSP)

  btv.glob.match("**/*.lua", path)                           --> %s
      `**` crosses separators, and spans zero of them too

  btv.glob.match("*.lua", path, { basename = true })         --> %s
      a separator-less pattern matches the path's last component (vim's rule)

  btv.glob.match("*.lua", path, { literal_separator = false }) --> %s
      the opt-out: `*` crosses `/` again (vim's autocmd rule)]]):format(
      path,
      btv.glob.match("*.lua", path),
      btv.glob.match("**/*.lua", path),
      btv.glob.match("*.lua", path, { basename = true }),
      btv.glob.match("*.lua", path, { literal_separator = false })
    )
  )
end, { desc = "The `*` vs `/` default and its two escapes" })

-- 4. :GlobWindows — path style is an OPTION, not the machine you're on. Running this
--    on Linux still matches Windows-spelled paths correctly, which is what makes it
--    work over a daemon/remote session that serves paths from the other convention.
vim.api.nvim_create_user_command("GlobWindows", function()
  local win = { style = "windows" }
  local uni = { style = "unix" }
  -- A `[==[ ]==]` long bracket, because the text itself shows `[[ ]]` Lua literals.
  vim.notify(
    ([==[
windows style — `\` is a SEPARATOR (so `src\*.rs` is a path):

  match([[src\*.rs]],    [[src\main.rs]])       --> %s
  match("src/*.rs",      [[src\main.rs]])       --> %s   (spellings normalize)
  match([[src\*.rs]],    [[src\a\main.rs]])     --> %s   (`*` stops at `\` too)
  match([[C:/**/*.txt]], [[C:\Users\me\a.txt]]) --> %s   (drive letter is a component)

unix style — `\` is the ESCAPE character (so `\*` is a literal asterisk):

  match([[a\*b]], "a*b")   --> %s   (matches the file literally named a*b)
  match([[a\*b]], "axyzb") --> %s   (an escaped `*` is NOT a wildcard)

Same host, same call, opposite answers — the convention belongs to the PATH.]==]):format(
      btv.glob.match([[src\*.rs]], [[src\main.rs]], win),
      btv.glob.match("src/*.rs", [[src\main.rs]], win),
      btv.glob.match([[src\*.rs]], [[src\a\main.rs]], win),
      btv.glob.match([[C:/**/*.txt]], [[C:\Users\me\a.txt]], win),
      btv.glob.match([[a\*b]], "a*b", uni),
      btv.glob.match([[a\*b]], "axyzb", uni)
    )
  )
end, { desc = "Windows vs unix path style on the same host (btv.glob style opt)" })

-- 5. :GlobIgnore — a glob SET: one compiled RegexSet tests a path against every
--    pattern in one pass, and `:matches` says which ones hit. The shape an ignore
--    list or a set of file-watcher patterns wants.
local ignore = btv.glob.set({ "**/target/**", "**/.git/**", "**/*.tmp" })

vim.api.nvim_create_user_command("GlobIgnore", function()
  local kept, dropped = {}, {}
  for _, p in ipairs(paths()) do
    local hits = ignore:matches(p)
    if #hits > 0 then
      dropped[#dropped + 1] = ("%s   (by %s)"):format(p, ignore:patterns()[hits[1]])
    else
      kept[#kept + 1] = p
    end
  end
  vim.notify(
    ("ignore set: %s\n\n  kept (%d):\n    %s\n\n  dropped (%d):\n    %s"):format(
      table.concat(ignore:patterns(), "  "),
      #kept,
      table.concat(kept, "\n    "),
      #dropped,
      table.concat(dropped, "\n    ")
    )
  )
end, { desc = "Filter the buffer's paths through a glob set (btv.glob.set)" })

-- 6. :GlobBench — the cache, made measurable. 50k one-shot matches of the same glob.
--    Only the first parses and compiles; every later call is a regex run. Type it and
--    watch it finish in milliseconds.
vim.api.nvim_create_user_command("GlobBench", function()
  local n, pattern, path = 50000, "src/**/*.{rs,toml}", "src/a/b/mod.rs"
  local started = os.clock()
  local hits = 0
  for _ = 1, n do
    if btv.glob.match(pattern, path) then
      hits = hits + 1
    end
  end
  local ms = (os.clock() - started) * 1000
  vim.notify(
    ("%d matches of %q in %.1f ms (%.2f us each)\n\z
    one parse + one regex build total — the rest came from the cache"):format(
      hits,
      pattern,
      ms,
      ms * 1000 / n
    )
  )
end, { desc = "50k cached glob matches (btv.glob compile cache)" })

-- 7. :GlobIsGlob — the canonical "is this a pattern or a plain path" predicate, for
--    code that branches on whether the user typed a glob.
vim.api.nvim_create_user_command("GlobIsGlob", function()
  local out = {}
  for _, s in ipairs(paths()) do
    out[#out + 1] = ("  %-34s %s"):format(s, btv.glob.is_glob(s) and "glob" or "plain path")
  end
  vim.notify("is_glob:\n" .. table.concat(out, "\n"))
end, { desc = "Which buffer lines are globs (btv.glob.is_glob)" })

vim.notify(table.concat({
  "btv.glob example loaded. Try:",
  "  :Glob **/*.rs        match every buffer path against a glob",
  "  :Glob *.rs           ... and see `*` stop at `/`",
  "  :GlobRegex *.lua     the regex a glob compiles to",
  "  :GlobStar            the `*` vs `/` default and its escapes",
  "  :GlobWindows         windows vs unix path style, same host",
  "  :GlobIgnore          filter the buffer through a glob set",
  "  :GlobBench           50k cached matches",
  "  :GlobIsGlob          glob-or-plain-path per line",
}, "\n"))
