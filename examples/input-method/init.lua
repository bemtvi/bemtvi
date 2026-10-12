--------------------------------------------------------------------------------
-- 'imdisable' — turning the input method (IME) off and on from a config.
--
-- Run (the GUI: the TUI can't do this, since the terminal owns the IME):
--   BEMTVI_CONFIG=examples/input-method \
--     cargo run -p bemtvi-gui -- examples/input-method/sample.txt
--
-- An input method (Japanese, Chinese, Korean…) composes the keys typed into it:
-- `nihon` becomes 日本. That is what you want while typing text, and not what you
-- want while keys are commands: in normal mode a Japanese IME turns `hjkl` into
-- kana. bemtvi doesn't decide that for you. 'imdisable' (vim's option, `imd` for
-- short) keeps the IME off while it's set, and the config decides when to set it.
--
-- Switch your OS input to a Japanese IME (Microsoft IME, or Mozc / fcitx5 on
-- Linux) in hiragana mode before trying these.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. The default: the IME is on in every mode.
--
-- Type-this:  :ImeAuto<CR>   (this config starts with §3 on; turn it off first)
--             inihon<Space><CR>
-- See-that:   日本 (or the IME's other candidate) is typed.
--
-- Type-this:  <Esc>jj
-- See-that:   the IME composes `jj` into kana instead of moving down: the
--             default leaves it on in normal mode too.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. Toggle it from a keymap.
--
-- Type-this:  <F7>
-- See-that:   "imdisable=true", and `jj` moves the cursor again: the IME is off.
--             <F7> once more turns it back on.
--
-- The plain ex form works as a map too, or typed directly:
--     btv.keymap.set("n", "<F7>", ":set imdisable!<CR>")
--------------------------------------------------------------------------------
btv.keymap.set("n", "<F7>", function()
  btv.o.imdisable = not btv.o.imdisable
  print("imdisable=" .. tostring(btv.o.imdisable))
end, { desc = "toggle the input method" })

--------------------------------------------------------------------------------
-- 3. Only where keys type text: off in normal and visual mode, on in insert,
--    replace, select and terminal mode and on the command line.
--
-- `ModeChanged` fires on every mode change with `old:new` mode codes in
-- `args.match`. The handler sets the option from the new one.
--
-- Type-this:  (with this config as shipped, :ImeAuto is on)
--             jj
-- See-that:   the cursor moves: the IME is off in normal mode.
--
-- Type-this:  inihon<Space><CR><Esc>
-- See-that:   日本 is typed in insert mode, and `jj` moves again after <Esc>.
--
-- Type-this:  :ImeAuto<CR>
-- See-that:   "ImeAuto off", and the IME is back on everywhere (§1).
--------------------------------------------------------------------------------
local TYPES_TEXT = { i = true, R = true, s = true, c = true, t = true }

--- Set 'imdisable' for the mode code `mode` (`n`, `i`, `v`, …).
local function follow(mode)
  btv.o.imdisable = not TYPES_TEXT[mode:sub(1, 1)]
end

btv.autocmd.create("ModeChanged", {
  callback = function(args)
    if btv.g.ime_auto then
      follow(args.match:match(":(.*)$"))
    end
  end,
})

btv.command("ImeAuto", function()
  btv.g.ime_auto = not btv.g.ime_auto
  if btv.g.ime_auto then
    follow(btv.mode().mode)
  else
    btv.o.imdisable = false
  end
  print("ImeAuto " .. (btv.g.ime_auto and "on" or "off"))
end, { desc = "keep the input method to the modes that type text" })

-- Start with §3 on.
btv.g.ime_auto = true
follow("n")
