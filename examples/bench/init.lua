-- ~~~ bemtvi Lua micro-benchmarks: PUC Lua (5.4, default) vs alternative backends ~~~
--
-- A backend-agnostic suite of the operations plugins do on hot paths
-- (tokenizing, string building, sorting candidates, fuzzy scoring, OOP
-- dispatch, coroutines …). Run the SAME file under each backend and compare.
--
-- Run it (from the repo root) — use --release, debug timings are meaningless:
--
--     BEMTVI_CONFIG=examples/bench cargo run --release -p bemtvi -- examples/bench/sample.txt
--
-- Then, in the editor:
--
--     :bench1 … :bench10     run one benchmark        (result on the message line)
--     :benchall              run them all             (full table via :messages)
--     :messages             review the whole table
--
-- HOW THE A/B WORKS
--   1. Build + run with the default PUC backend, `:benchall`, record the table.
--   2. Switch mlua to Luau (see examples/bench/README.md), rebuild, `:benchall`.
--   3. Compare. The work is identical across backends — each line prints a
--      `chk=` checksum; if the checksums match, both VMs computed the same thing,
--      so the only difference is speed. `_VERSION` in the header tells you which
--      backend produced the numbers ("Lua 5.4" vs "Luau").
--
-- Timing uses os.clock() (CPU seconds, present on both backends). Workloads are
-- sized to run for a meaningful duration; if a bench is too fast/slow on your
-- machine, bump SCALE below — it multiplies every iteration count equally, so
-- the comparison stays fair.

local SCALE = 1

-- ----------------------------------------------------------------------------
-- Deterministic data (built once). A minstd LCG keeps every product under 2^53
-- so the IEEE-double arithmetic is bit-identical on both backends → identical
-- data → identical work → comparable timings.
-- ----------------------------------------------------------------------------
local seed = 1
local function rnd(m)
  seed = (seed * 16807) % 2147483647
  return seed % m
end

-- A word list, wrapped by hand to stay four readable rows.
-- stylua: ignore
local KEYWORDS = {
  "local", "function", "return", "end", "if", "then", "else", "for", "in",
  "pairs", "ipairs", "require", "vim", "btv", "buffer", "window", "config",
  "setup", "opts", "true", "false", "nil", "string", "format", "table",
  "insert", "concat", "gsub", "match", "find", "pcall", "select", "type",
}

-- A synthetic source-like blob (~30 KB): words, newlines, and `name123()` calls.
local function make_blob()
  local parts = {}
  for i = 1, 4000 do
    parts[#parts + 1] = KEYWORDS[rnd(#KEYWORDS) + 1]
    if i % 8 == 0 then
      parts[#parts + 1] = "\n"
    elseif i % 5 == 0 then
      parts[#parts + 1] = " name" .. i .. "() "
    else
      parts[#parts + 1] = " "
    end
  end
  return table.concat(parts)
end
local BLOB = make_blob()

-- A pool of path-like candidates for the fuzzy-matcher benchmark.
local CANDS = {}
for i = 1, 3000 do
  CANDS[i] = "src/module_" .. rnd(50) .. "/widget_" .. i .. ".lua"
end

-- ----------------------------------------------------------------------------
-- Benchmark bodies. Each returns a number (folded into a checksum so nothing is
-- dead-code-eliminated and so both backends can be proven to agree).
-- ----------------------------------------------------------------------------

-- 1. Lua-pattern tokenizing — what syntax/comment/completion code does per line.
local function b1_tokenize()
  local n = 0
  for tok in string.gmatch(BLOB, "%a+") do
    n = n + #tok
  end
  return n
end

-- 2. Search sweeps — plain string.find (grep-style) + a pattern find.
local function b2_find()
  local n, pos = 0, 1
  while true do
    local s, e = string.find(BLOB, "function", pos, true)
    if not s then
      break
    end
    n = n + 1
    pos = e + 1
  end
  pos = 1
  while true do
    local s, e = string.find(BLOB, "name%d+", pos)
    if not s then
      break
    end
    n = n + 1
    pos = e + 1
  end
  return n
end

-- 3. gsub transforms — collapse whitespace + uppercase words (function
--    replacement, so no `%`-in-replacement dialect differences).
local function b3_gsub()
  local out, c1 = string.gsub(BLOB, "%s+", " ")
  local _, c2 = string.gsub(out, "%a+", string.upper)
  return c1 + c2
end

-- 4. String building via table.concat — statusline / virt-text assembly.
local function b4_concat()
  local t = {}
  for i = 1, 5000 do
    t[i] = "[" .. i .. ":" .. (i % 7) .. "]"
  end
  return #table.concat(t, ",")
end

-- 5. Sorting records with a comparator closure — ordering picker results.
local function b5_sort()
  seed = 1 -- reseed so every call builds identical data (stable, order-independent checksum)
  local t = {}
  for i = 1, 4000 do
    t[i] = { key = rnd(100000), name = "item" .. i }
  end
  table.sort(t, function(a, b)
    return a.key < b.key
  end)
  return t[1].key + t[#t].key
end

-- 6. Hash-table insert + lookup — dedup / "seen" sets / memoization caches.
local function b6_hashmap()
  local m = {}
  for i = 1, 5000 do
    m["k" .. i] = i
  end
  local sum = 0
  for i = 1, 5000 do
    sum = sum + (m["k" .. i] or 0)
  end
  return sum
end

-- 7. Function-call / closure overhead — callback-heavy iterator code.
local function b7_calls()
  local function add(a, b)
    return a + b
  end
  local s = 0
  for i = 1, 200000 do
    s = add(s, i)
  end
  return s
end

-- 8. Metatable OOP dispatch — the class systems most plugins are built on.
local Vec = {}
Vec.__index = Vec
function Vec.new(x, y)
  return setmetatable({ x = x, y = y }, Vec)
end
function Vec:add(o)
  return Vec.new(self.x + o.x, self.y + o.y)
end
function Vec:len2()
  return self.x * self.x + self.y * self.y
end
local function b8_oop()
  local acc = Vec.new(0, 0)
  for i = 1, 50000 do
    acc = acc:add(Vec.new(i % 10, i % 7))
    if acc.x > 1e6 then
      acc = Vec.new(0, 0)
    end
  end
  return acc:len2()
end

-- 9. Fuzzy subsequence scoring — fzf/telescope-style candidate ranking, the
--    most CPU-bound thing a picker does. Byte-level, no allocation in the loop.
local function score(needle, hay)
  local ni, s, last = 1, 0, 0
  local nl = #needle
  for hi = 1, #hay do
    if ni > nl then
      break
    end
    if string.byte(hay, hi) == string.byte(needle, ni) then
      s = s + 1 + (hi == last + 1 and 2 or 0)
      last = hi
      ni = ni + 1
    end
  end
  if ni > nl then
    return s
  else
    return -1
  end
end
local function b9_fuzzy()
  local best, bi = -1, 0
  for i = 1, #CANDS do
    local sc = score("mwl", CANDS[i])
    if sc > best then
      best = sc
      bi = i
    end
  end
  return best + bi
end

-- 10. Coroutine create/resume churn — async runners / generators (plenary-style).
local function b10_coroutine()
  local total = 0
  for i = 1, 20000 do
    local co = coroutine.create(function(n)
      for j = 1, 5 do
        coroutine.yield(n + j)
      end
      return n
    end)
    while coroutine.status(co) ~= "dead" do
      local ok, w = coroutine.resume(co, i)
      if ok and w then
        total = total + (w % 3)
      end
    end
  end
  return total
end

-- ----------------------------------------------------------------------------
-- Harness + command registration.
-- ----------------------------------------------------------------------------
-- Iteration counts are tuned so each bench runs ~150-270 ms on a typical machine
-- (total ~2 s), keeping every bench individually measurable. SCALE multiplies
-- them all; the same counts run on both backends, so the comparison stays fair.
-- One benchmark per row, columns aligned so the table reads as a table.
-- stylua: ignore
local BENCHES = {
  { label = "1  pattern tokenize",  iters = 150, fn = b1_tokenize },
  { label = "2  string.find sweep", iters = 300, fn = b2_find },
  { label = "3  gsub transform",    iters = 80,  fn = b3_gsub },
  { label = "4  table.concat build", iters = 80, fn = b4_concat },
  { label = "5  table.sort records", iters = 25, fn = b5_sort },
  { label = "6  hashmap ins+lookup", iters = 50, fn = b6_hashmap },
  { label = "7  closure calls",      iters = 25, fn = b7_calls },
  { label = "8  metatable OOP",      iters = 6,  fn = b8_oop },
  { label = "9  fuzzy scoring",      iters = 14, fn = b9_fuzzy },
  { label = "10 coroutine churn",    iters = 5,  fn = b10_coroutine },
}

-- Run `fn` `iters * SCALE` times, return (elapsed_seconds, line_to_print).
local function timeit(b)
  local iters = b.iters * SCALE
  b.fn() -- warm up (not timed)
  local chk = 0
  local t0 = os.clock()
  for _ = 1, iters do
    chk = chk + (b.fn() or 0)
  end
  local dt = os.clock() - t0
  local line = string.format(
    "%-22s %5d it  %8.1f ms  %8.2f us/it  chk=%s",
    b.label,
    iters,
    dt * 1000,
    dt / iters * 1e6,
    tostring(chk)
  )
  return dt, line
end

for idx, b in ipairs(BENCHES) do
  btv.user_command.create("bench" .. idx, function()
    local _, line = timeit(b)
    print(line)
  end, {})
end

btv.user_command.create("benchall", function()
  print(string.format("== bemtvi Lua microbench  [%s]  SCALE=%d ==", _VERSION, SCALE))
  local total = 0
  for _, b in ipairs(BENCHES) do
    local dt, line = timeit(b)
    total = total + dt
    print(line)
  end
  print(
    string.format("-- total CPU: %.1f ms  (run :messages to see the full table) --", total * 1000)
  )
end, {})

print("bench suite loaded: :bench1 … :bench10, :benchall  (backend: " .. _VERSION .. ")")
