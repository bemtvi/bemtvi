-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/commenting
--
-- Every numbered TRY-IT is typed exactly as written, against the same sample.rs
-- the notes describe — so the demo cannot rot into an instruction that no longer
-- works.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open one of the example's files, re-read so each test starts from the same text.
local function open(t, name)
  t:cmd("e " .. DIR .. "/" .. name)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/commenting", function()
  btv.test.it("the filetype default supplies the template", function(t)
    open(t, "sample.rs")
    btv.test.expect(btv.bo.filetype).to_be("rust")
    btv.test.expect(btv.bo.commentstring).to_be("// %s")
  end)

  -- 1. "gcc -> '// let greeting = ...'  … gcc -> back to uncommented"
  btv.test.it("try-it 1 — gcc comments the line, and gcc again uncomments it", function(t)
    open(t, "sample.rs")
    t:feed("2Ggcc")
    btv.test.expect(t:line(2)).to_be('    // let greeting = "hello";')
    t:feed("gcc")
    btv.test.expect(t:line(2)).to_be('    let greeting = "hello";')
  end)

  -- 2. "gc2j -> comment this line and the next two"
  btv.test.it("try-it 2 — gc takes a motion, always linewise", function(t)
    open(t, "sample.rs")
    t:feed("2Ggc2j")
    btv.test.expect(t:line(2)).to_contain("// let greeting")
    btv.test.expect(t:line(3)).to_contain("// let target")
    -- Line 4 is blank; the operator covered it as the third line.
    btv.test.expect(t:line(5)).never.to_contain("//")
  end)

  btv.test.it("try-it 2 — gcip comments the whole paragraph", function(t)
    open(t, "sample.rs")
    t:feed("2Ggcip")
    -- The paragraph is lines 1-3, and the markers align to the block's SHALLOWEST
    -- indent — which `fn main() {` puts at column 0.
    btv.test.expect(t:line(1)).to_be("// fn main() {")
    btv.test.expect(t:line(2)).to_be('//     let greeting = "hello";')
    btv.test.expect(t:line(3)).to_be('//     let target = "world";')
    -- It ends at the blank line, so the `if` below is untouched.
    btv.test.expect(t:line(5)).never.to_contain("//")
  end)

  btv.test.it("try-it 2 — gcG comments to the end of the file", function(t)
    open(t, "sample.rs")
    t:feed("5GgcG")
    for n = 5, #t:lines() do
      btv.test.expect(t:line(n)).to_contain("//")
    end
    btv.test.expect(t:line(4)).never.to_contain("//")
  end)

  -- 3. "Indent-aware: the markers align to the block's indent, each line keeps
  --     its own."
  btv.test.it("try-it 3 — the markers align to the block's indent", function(t)
    open(t, "sample.rs")
    t:feed("6GVjgc")
    btv.test.expect(t:line(6)).to_be('        // println!("{greeting}, {target}!");')
    btv.test.expect(t:line(7)).to_be('        // println!("commenting demo");')
  end)

  -- 4. "3gcc -> toggle three lines from the cursor down"
  btv.test.it("try-it 4 — a counted gcc toggles that many lines", function(t)
    open(t, "sample.rs")
    t:feed("2G3gcc")
    btv.test.expect(t:line(2)).to_contain("//")
    btv.test.expect(t:line(3)).to_contain("//")
    btv.test.expect(t:line(4)).to_contain("//")
    btv.test.expect(t:line(5)).never.to_contain("//")
  end)

  -- 5. "Vjj then gc -> comment the selected lines"
  btv.test.it("try-it 5 — gc in visual comments the selection", function(t)
    open(t, "sample.rs")
    t:feed("1GVjjgc")
    btv.test.expect(t:line(1)).to_contain("// fn main")
    btv.test.expect(t:line(3)).to_contain("// ")
    btv.test.expect(t:line(4)).never.to_contain("//")
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- 6. ":set commentstring?" / ":set commentstring=//\ %s"
  btv.test.it("try-it 6 — :set commentstring? echoes the template", function(t)
    open(t, "sample.rs")
    t:cmd("set commentstring?")
    btv.test.expect(t:message()).to_contain("commentstring=// %s")
  end)

  btv.test.it("try-it 6 — setting the template by hand changes what gcc writes", function(t)
    open(t, "sample.rs")
    t:cmd([[set commentstring=;;\ %s]])
    t:feed("2Ggcc")
    btv.test.expect(t:line(2)).to_contain(";; let greeting")
  end)

  -- 6. ":e notes.sh -> a bash buffer; gcc -> '#  echo ...'"
  btv.test.it("try-it 6 — the FileType override reaches the bash buffer", function(t)
    open(t, "notes.sh")
    btv.test.expect(btv.bo.filetype).to_be("bash")
    -- Two spaces: the autocmd's "#  %s" override, not the "# %s" default.
    btv.test.expect(btv.bo.commentstring).to_be("#  %s")
    t:feed("3Ggcc")
    btv.test.expect(t:line(3)).to_be('#  echo "first"')
    t:feed("gcc")
    btv.test.expect(t:line(3)).to_be('echo "first"')
  end)

  -- The muscle-memory aliases the config adds.
  btv.test.it("<leader>/ toggles the line, and the selection in visual", function(t)
    open(t, "sample.rs")
    t:feed("2G<Bslash>/")
    btv.test.expect(t:line(2)).to_contain("// let greeting")
    t:feed("<Bslash>/")
    btv.test.expect(t:line(2)).never.to_contain("//")
    t:feed("2GVj<Bslash>/")
    btv.test.expect(t:line(2)).to_contain("//")
    btv.test.expect(t:line(3)).to_contain("//")
  end)
end)
