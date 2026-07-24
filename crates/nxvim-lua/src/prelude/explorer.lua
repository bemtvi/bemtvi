-- nx.explorer visuals: highlighting for the in-window directory listing (vim's
-- netrw, `filetype=nxdir`). The listing itself — reading entries, `<CR>`/`-`
-- navigation — lives in the core (`editor/explorer.rs`), and its activation keys
-- are wired as buffer-local maps by the `FileType nxdir` autocmd in
-- `prelude/keymap.lua`. This file adds the *decoration* layer, and it does so the
-- way a plugin would: a viewport-scoped `nx.decor` provider (the new native
-- decoration API), not a special case baked into the renderer.
--
-- Loads AFTER `prelude/decor.lua` (it calls `nx.decor.provider`) — see the prelude
-- order in `runtime.rs`.

nx.explorer = nx.explorer or {}

-- The highlight groups the listing paints with. Concrete catppuccin-mocha colours
-- (the same convention as the `:Plugins` UI in `prelude/plugins_ui.lua`) so the
-- listing reads well on the default dark background even before a colorscheme
-- loads; a colorscheme can redefine these groups to match its own palette.
local HL = {
  NxDirDirectory = { fg = "#89b4fa", bold = true }, -- a sub-directory row (suffixed `/`)
  NxDirParent = { fg = "#94e2d5" }, -- the leading `../` up-entry
}
for name, spec in pairs(HL) do
  nx.hl.define(0, name, spec)
end

-- The provider: wake once per visible-range change of an `nxdir` window and colour
-- each directory row across the visible slice — the `../` up-entry one way, every
-- sub-directory (a line ending in `/`) another. File rows publish no mark, so they
-- render in the normal text colour, exactly like netrw. Re-listing in place
-- (descend / `-`) clears the old marks (the wholesale-rope-replace drops the
-- namespace) and re-dispatches this provider, so the colours always track the
-- current directory.
nx.decor.provider({
  name = "explorer",
  bufs = { filetype = { "nxdir" } },
  on_range = function(ctx, publish)
    local marks = {}
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      if line == "../" then
        marks[#marks + 1] = { row = row, col = 0, end_col = #line, hl = "NxDirParent" }
      elseif line:sub(-1) == "/" then
        marks[#marks + 1] = { row = row, col = 0, end_col = #line, hl = "NxDirDirectory" }
      end
    end
    publish(marks)
  end,
})

-- ============================================================================
-- The explorer itself — the in-window directory listing as a pure-Lua plugin.
--
-- This is the netrw mechanism, built on `nx.*`. A local `:e dir` is claimed by a
-- `BufReadCmd` handler (Primitive B of docs/plans/2026-06-25-explorer-lua-port.md),
-- which reads the entries with `nx.fs` and renders them into the opened buffer. A
-- *remote* directory (a daemon / web session) is filled by the server from the entries
-- its off-tick fetch already read (the `nx.fs` op and the file-open fetch are separate
-- legs, and a daemon may wire only the latter) — but in BOTH cases the result is the
-- same: a `nomodifiable`, `filetype=nxdir` buffer named for the directory.
--
-- Navigation is therefore **stateless**: it derives the directory from the buffer's
-- name and the entry from the row text (a trailing `/` marks a sub-directory), so it
-- works whoever filled the listing. `<CR>` / `<2-LeftMouse>` open the entry under the
-- cursor (descend in place, or edit a file), `-` goes to the parent; everything else is
-- plain normal-mode motion on the read-only buffer, and the decor provider above
-- colours the rows. The activation maps are installed by the `FileType nxdir` autocmd
-- (prelude/keymap.lua), which fires the moment either fill path sets the filetype.
-- ============================================================================

-- Escaping a path for use as a bare `:edit` argument is `nx.fname.escape`
-- (vimfn.lua — loaded after this module, referenced only at open time).
local function edit_escape(path)
  return nx.fname.escape(path)
end

-- Render a `nx.fs.readdir` result into listing lines — the Lua twin of the server's
-- `nxvim_core::dir_listing` (off-tick): `../` first, then sub-directories (suffixed
-- `/`), then files, each group sorted case-insensitively (netrw's default).
local function render(entries)
  local dirs, files = {}, {}
  for _, e in ipairs(entries) do
    if e.type == "directory" then
      dirs[#dirs + 1] = e.name
    else
      files[#files + 1] = e.name
    end
  end
  local byname = function(a, b)
    return a:lower() < b:lower()
  end
  table.sort(dirs, byname)
  table.sort(files, byname)
  local lines = { "../" }
  for _, name in ipairs(dirs) do
    lines[#lines + 1] = name .. "/"
  end
  for _, name in ipairs(files) do
    lines[#lines + 1] = name
  end
  return lines
end

-- Read `dir`'s entries (async) and render them into listing buffer `buf`: fill the
-- lines, lock the buffer (`nomodifiable`, and `modified = false` because the fill is a
-- read, not an edit — no `[+]`), and mark it `nxdir` (drives the decor provider + the
-- `FileType nxdir` activation maps). A failed read is surfaced loud and leaves the
-- buffer empty rather than a half listing. The LOCAL fill path (the `BufReadCmd`
-- handler); a remote directory is filled server-side (`nxvim_core::dir_listing`).
local function list_into(buf, dir)
  nx.async(function()
    local ok, entries = pcall(nx.await, nx.fs.readdir(dir))
    if not ok then
      local msg = type(entries) == "table" and entries.message or tostring(entries)
      return nx.notify("explorer: can't read " .. dir .. ": " .. msg, 4)
    end
    -- Fill, then lock. `nvim_buf_set_lines` refuses a `nomodifiable` buffer, so make it
    -- writable for the fill and lock it right after; clear `modified` since this fill is
    -- the file *read*, not an unsaved change.
    nx.bo[buf].modifiable = true
    vim.api.nvim_buf_set_lines(buf, 0, -1, false, render(entries))
    nx.bo[buf].modifiable = false
    nx.bo[buf].modified = false
    nx.bo[buf].filetype = "nxdir"
  end)()
end

-- The absolute directory a listing buffer represents — its name, canonicalised
-- (trailing slash trimmed except the root) so `../`/descent math is unambiguous however
-- it was spelled (`.`, a relative dir).
local function listing_dir(buf)
  local dir = vim.fn.fnamemodify(vim.api.nvim_buf_get_name(buf), ":p")
  if #dir > 1 then
    dir = dir:gsub("/$", "")
  end
  return dir
end

-- The row text under the cursor in listing buffer `buf` (the entry name, with a `/`
-- suffix for a sub-directory), or nil on a blank/garbled row.
local function entry_under_cursor(buf)
  local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-based
  local line = vim.api.nvim_buf_get_lines(buf, row - 1, row, false)[1]
  if not line or line == "" then
    return nil
  end
  return line
end

-- Replace the listing in window-current buffer `cur` with a fresh `:edit` of `path`,
-- then wipe `cur`. `:edit` switches the window synchronously (the listing/file fill is
-- deferred — a directory is re-claimed by the `BufReadCmd` handler / refilled by the
-- server, a file read normally), so by the time the wipe runs `cur` is no longer
-- current. Net buffer count is unchanged — the netrw "reuse the window, don't strand
-- the old listing" behaviour (descend in place; opening a file destroys the listing).
local function replace_with(cur, path)
  -- The explorer's navigation math is absolute (an unambiguous `dir .. "/" .. name`), but
  -- the buffer the user lands on reads better named relative to cwd — netrw's model, and
  -- what `:edit relative/path` would have produced by hand. `:.` returns the path relative
  -- to cwd when it lives under it, and leaves an outside path (a parent above cwd, a remote
  -- `/virtual/...`) absolute. Relativise before escaping so the `:edit` arg is the final name.
  path = vim.fn.fnamemodify(path, ":.")
  vim.cmd("edit " .. edit_escape(path))
  vim.cmd("bwipeout! " .. cur)
end

-- `<CR>` / double-click: open the entry under the cursor. `../` (or the cursor on the
-- top row) goes up; any other entry is edited — a sub-directory re-lists, a file opens
-- (destroying the listing). A no-op on a stale/blank buffer.
function nx.explorer._open(buf)
  local line = entry_under_cursor(buf)
  if not line then
    return
  end
  if line == "../" then
    return nx.explorer._up(buf)
  end
  local dir = listing_dir(buf)
  local name = line:gsub("/$", "") -- a sub-directory row carries a trailing `/`
  local target = (dir == "/" and "/" or dir .. "/") .. name
  replace_with(buf, target)
end

-- `-` (and `<CR>` on `../`): list the parent directory. At the filesystem root the
-- parent is the root itself, so the explorer stays put (as netrw does).
function nx.explorer._up(buf)
  local dir = listing_dir(buf)
  local parent = vim.fn.fnamemodify(dir, ":h")
  if not parent or parent == "" or parent == dir then
    return
  end
  replace_with(buf, parent)
end

-- `nx.explorer.actions` — the public, rebindable activation actions (current-buffer
-- wrappers over the stateless `_open`/`_up`). A user/plugin extends or rebinds the
-- explorer with an ordinary buffer-local map in a `FileType nxdir` autocmd, e.g.
--   `nx.autocmd.create("FileType", { pattern = "nxdir", callback = function(a)`
--     `nx.keymap.set("n", "<C-j>", nx.explorer.actions.open, { buffer = a.buf })`
--   `end })`
-- The default `<CR>`/`-`/`<2-LeftMouse>` maps (prelude/keymap.lua) are these too.
nx.explorer.actions = nx.explorer.actions or {}
nx.explorer.actions.open = function()
  nx.explorer._open(vim.api.nvim_get_current_buf())
end
nx.explorer.actions.up = function()
  nx.explorer._up(vim.api.nvim_get_current_buf())
end

-- `nx.explorer.enable()`: turn the explorer on. Registers the `BufReadCmd` handler that
-- claims directory opens — `pattern = "*"` deciding per path via `args.isdir`, so it
-- claims directories and lets every file read fall through to the editor's default load
-- (netrw's exact model). Idempotent. Calling it makes a `BufReadCmd` handler exist,
-- which is what tells the core to defer a `:e dir` so this handler gets the chance to
-- claim it (see `Editor::should_defer_open`). Called at the bottom of this file, so the
-- explorer is on by default; a config could turn it off by clearing the autocmd.
function nx.explorer.enable()
  if nx.explorer._enabled then
    return
  end
  nx.explorer._enabled = true
  nx.autocmd.create("BufReadCmd", {
    pattern = "*",
    callback = function(args)
      if not args.isdir then
        return -- a file (or non-directory): decline, the default read fills it
      end
      -- Claim the read and fill asynchronously. The directory path is canonicalised to
      -- an absolute path (trailing slash trimmed, except the root) so `../` / descent
      -- math is unambiguous however it was spelled (`.`, a relative dir).
      local dir = vim.fn.fnamemodify(args.file, ":p")
      if #dir > 1 then
        dir = dir:gsub("/$", "")
      end
      list_into(args.buf, dir)
      return true
    end,
  })
end

-- The explorer is a built-in: on by default, the netrw replacement.
nx.explorer.enable()
