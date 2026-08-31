-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/promise
--
-- Everything runs at startup with no keypresses, so the spec mostly waits — on
-- `_G.promise_demo`, the very table the notes tell a reader to inspect — and then
-- re-drives each combinator directly, since the point of the demos is the exact
-- semantics rather than one particular run.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

btv.test.describe("examples/promise", function()
  -- 1. "resolve → :next → :next. Reactions run as MICROTASKS."
  btv.test.it("§1 — a chain threads each handler's return into the next", function(t)
    t:wait_for(function()
      return _G.promise_demo.basic ~= nil
    end, { message = "the basic chain never settled" })
    btv.test.expect(_G.promise_demo.basic).to_be(21)
  end)

  btv.test.it("§1 — :next is async even for an already-resolved promise", function(t)
    local ran = false
    btv.promise.resolve(1):next(function()
      ran = true
    end)
    -- Off the current stack: nothing has run yet.
    btv.test.expect(ran).to_be(false)
    t:wait_for(function()
      return ran
    end, { message = "the microtask never ran" })
  end)

  -- 2. "a throw anywhere in a chain skips later :next handlers and lands in the
  --     trailing :catch."
  btv.test.it("§2 — a throw skips the rest of the chain and lands in :catch", function(t)
    t:wait_for(function()
      return _G.promise_demo.caught ~= nil
    end, { message = "the chain never rejected" })
    btv.test.expect(tostring(_G.promise_demo.caught)).to_contain("disk on fire")

    -- …and the skipped handler really is skipped.
    local skipped, caught = false, nil
    btv.promise
      .resolve("x")
      :next(function()
        error("boom")
      end)
      :next(function()
        skipped = true
      end)
      :catch(function(err)
        caught = tostring(err)
      end)
    t:wait_for(function()
      return caught ~= nil
    end, { message = "the second chain never rejected" })
    btv.test.expect(skipped).to_be(false)
    btv.test.expect(caught).to_contain("boom")
  end)

  -- 3. "btv.promise.delay — an await-able sleep on the loop."
  btv.test.it("§3 — delay resolves with its value, off the input tick", function(t)
    t:wait_for(function()
      return _G.promise_demo.delayed ~= nil
    end, { tries = 200, interval = 20, message = "the delay never fired" })
    btv.test.expect(_G.promise_demo.delayed).to_be("woke up")
  end)

  -- 4. "all() waits for every input (in order); race() takes the first to settle.
  --     Mix promises and plain values freely."
  btv.test.it("§4 — all() waits for every input, in order", function(t)
    t:wait_for(function()
      return _G.promise_demo.all ~= nil
    end, { tries = 200, interval = 20, message = "all() never settled" })
    btv.test.expect(_G.promise_demo.all).to_equal({ 1, 2, 3 })
  end)

  btv.test.it("§4 — race() takes the first to settle", function(t)
    local winner
    btv.promise
      .race({ btv.promise.delay(300, "slow"), btv.promise.delay(20, "fast") })
      :next(function(w)
        winner = w
      end)
    t:wait_for(function()
      return winner ~= nil
    end, { tries = 200, interval = 20, message = "race() never settled" })
    btv.test.expect(winner).to_be("fast")
  end)

  -- 5. "Inside an btv.async function, btv.await(p) suspends until `p` settles."
  btv.test.it("§5 — async/await reads top-to-bottom and returns a promise", function(t)
    t:wait_for(function()
      return _G.promise_demo.async ~= nil
    end, { tries = 200, interval = 20, message = "the async function never settled" })
    btv.test.expect(_G.promise_demo.async).to_be(15)
  end)

  btv.test.it("§5 — a rejected await raises inside the coroutine", function(t)
    local caught
    btv.async(function()
      local ok, err = pcall(btv.await, btv.promise.reject("nope"))
      caught = ok and "no error" or tostring(err)
    end)()
    t:wait_for(function()
      return caught ~= nil
    end, { message = "the await never rejected" })
    btv.test.expect(caught).to_contain("nope")
  end)

  btv.test.it("§5 — …and :catch on the RESULT catches it too", function(t)
    local caught
    btv
      .async(function()
        btv.await(btv.promise.reject("bang"))
      end)()
      :catch(function(err)
        caught = tostring(err)
      end)
    t:wait_for(function()
      return caught ~= nil
    end, { message = "the result promise never rejected" })
    btv.test.expect(caught).to_contain("bang")
  end)

  -- "the all / all_settled / race / any / resolve / reject / try combinators"
  btv.test.it("the whole browser-shaped combinator set is there", function(t)
    for _, name in ipairs({
      "new",
      "resolve",
      "reject",
      "all",
      "all_settled",
      "race",
      "any",
      "try",
      "delay",
    }) do
      btv.test.expect(type(btv.promise[name])).to_be("function")
    end
  end)

  btv.test.it("all_settled reports each outcome rather than short-circuiting", function(t)
    local got
    btv.promise
      .all_settled({ btv.promise.resolve(1), btv.promise.reject("no") })
      :next(function(results)
        got = results
      end)
    t:wait_for(function()
      return got ~= nil
    end, { message = "all_settled never settled" })
    btv.test.expect(#got).to_be(2)
    btv.test.expect(got[1].status).to_be("fulfilled")
    btv.test.expect(got[1].value).to_be(1)
    btv.test.expect(got[2].status).to_be("rejected")
  end)

  btv.test.it("any() takes the first to FULFIL, not merely to settle", function(t)
    local got
    btv.promise
      .any({ btv.promise.reject("first fails"), btv.promise.delay(20, "ok") })
      :next(function(v)
        got = v
      end)
    t:wait_for(function()
      return got ~= nil
    end, { tries = 200, interval = 20, message = "any() never settled" })
    btv.test.expect(got).to_be("ok")
  end)

  -- ":finally"
  btv.test.it(":finally runs on both paths", function(t)
    local ran = 0
    btv.promise.resolve(1):finally(function()
      ran = ran + 1
    end)
    btv.promise
      .reject("x")
      :finally(function()
        ran = ran + 1
      end)
      :catch(function() end)
    t:wait_for(function()
      return ran == 2
    end, { message = ":finally did not run on both paths" })
  end)
end)
