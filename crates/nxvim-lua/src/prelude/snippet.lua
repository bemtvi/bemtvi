-- nx.snippet: the native snippet engine (docs/plans/2026-06-15-nx-snippet-engine.md,
-- docs/specs/2026-06-11-native-plugin-api.md §4). The server owns the LSP snippet
-- grammar, expansion, and the tabstop session (`<Tab>`/`<S-Tab>` jump between
-- tabstops, mirrors kept in sync); this Lua surface only configures the jump keys,
-- registers snippet *data* per filetype, and exposes a manual `expand`.
--
-- Snippet bodies are LSP snippet syntax: `$1` / `${1:default}` / `$0` tabstops,
-- `${1|a,b|}` choices, and mirrors (the same number repeated). Unsupported
-- constructs (variables, transforms) error loud at expansion rather than inserting
-- raw `$1` — the project's no-silent-stubs rule.

nx.snippet = nx.snippet or {}
-- Registered snippet data, keyed by filetype: `_byft[ft] = { {trigger, body}, ... }`.
-- The `snippets` completion source offers these for the current buffer's filetype.
nx.snippet._byft = nx.snippet._byft or {}

-- nx.snippet.setup { jump_next = "<Tab>", jump_prev = "<S-Tab>" }
-- Configure the tabstop-jump keys. Either may be a string or a list of strings; an
-- omitted key keeps its default (`<Tab>` / `<S-Tab>`).
function nx.snippet.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.snippet.setup: expected a table, got " .. type(opts))
  end
  local function key_list(spec, name)
    if spec == nil then
      return {}
    elseif type(spec) == "string" then
      return { spec }
    elseif type(spec) == "table" then
      for _, k in ipairs(spec) do
        if type(k) ~= "string" then
          error("nx.snippet.setup: " .. name .. " must be string(s), got " .. type(k))
        end
      end
      return spec
    end
    error("nx.snippet.setup: " .. name .. " must be a string or list of strings")
  end
  nx._snippet_setup(key_list(opts.jump_next, "jump_next"), key_list(opts.jump_prev, "jump_prev"))
end

-- nx.snippet.add("rust", { { trigger = "fn", body = "fn ${1:name}() {\n\t$0\n}" }, ... })
-- Register snippets for a filetype. `trigger` and `body` are required strings.
-- Function bodies (dynamic / context-aware) are not supported yet and error loud
-- rather than silently dropping the snippet (deferred to a later phase).
function nx.snippet.add(ft, list)
  if type(ft) ~= "string" then
    error("nx.snippet.add: filetype (arg 1) must be a string, got " .. type(ft))
  end
  if type(list) ~= "table" then
    error("nx.snippet.add: snippets (arg 2) must be a list of { trigger, body }")
  end
  local triggers, bodies = {}, {}
  for i, s in ipairs(list) do
    if type(s) ~= "table" or type(s.trigger) ~= "string" then
      error("nx.snippet.add: entry " .. i .. " needs a string `trigger`")
    end
    if type(s.body) == "function" then
      error(
        "nx.snippet.add: function snippet bodies are not supported yet (entry '"
          .. s.trigger
          .. "'); use a string body for now"
      )
    end
    if type(s.body) ~= "string" then
      error("nx.snippet.add: entry '" .. s.trigger .. "' needs a string `body`")
    end
    triggers[#triggers + 1] = s.trigger
    bodies[#bodies + 1] = s.body
    nx.snippet._byft[ft] = nx.snippet._byft[ft] or {}
    table.insert(nx.snippet._byft[ft], { trigger = s.trigger, body = s.body })
  end
  nx._snippet_add(ft, triggers, bodies)
end

-- nx.snippet.expand("for ${1:i} = 1, ${2:n} do\n\t$0\nend")
-- Expand a snippet body at the cursor immediately, entering Insert mode at the
-- first tabstop. Errors loud on a malformed / unsupported body.
function nx.snippet.expand(body)
  if type(body) ~= "string" then
    error("nx.snippet.expand: body must be a string, got " .. type(body))
  end
  nx._snippet_expand(body)
end
