-- ~~~ bemtvi btv.http.mount playground: a live markdown preview, served from the editor ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/btv-http-mount \
--       cargo run -p bemtvi -- examples/btv-http-mount/sample.md
--
-- Then press  \p  to open the preview in your browser, and edit the buffer — the page
-- follows along. (`\u` prints the URL, `\c` closes the mount.)
--
-- `btv.http.mount(opts)` publishes a plugin's subroute on the editor's ONE origin and
-- resolves with a handle whose `:url()` you can hand to a browser. A plugin never binds
-- a port: it mounts at `/plugin/<name>/*` and the editor owns the listener. That is what
-- lets the same plugin code run in the browser build too, where a tab cannot bind a port
-- and a Service Worker serves the same routes instead.
--
-- Nothing starts until a plugin asks — the listener binds lazily on the first mount, so
-- a config without this file opens no port at all.
--
-- Two shapes in one API, on purpose:
--   * the MOUNT is one-shot, so it returns a PROMISE (it is also what makes an ephemeral
--     port usable: the resolved port arrives already settled, nothing polls for it);
--   * the REQUESTS are a persistent stream, so they are a handler (`on_request`).
--
-- WHERE it listens is the USER's call, not this plugin's — the `'httphost'` /
-- `'httpport'` options. Uncomment to pin a stable, bookmarkable port instead of the
-- ephemeral default:
--
--     btv.o.httpport = 8080          -- 0 (default) picks a free one
--     btv.o.httphost = "0.0.0.0"     -- careful: exposes every mount to the LAN
--
-- NOTE — markdown rendering lives HERE, in the plugin, not in the editor. The mount
-- serves the raw buffer text at `/source` and the page renders it client-side; a real
-- plugin would use whatever frontend framework it likes (and let that framework own
-- live-reload). The editor's job is to serve bytes.

vim.g.mapleader = "\\"

local function show(msg)
  btv.notify(msg)
end

--------------------------------------------------------------------------------
-- The page. A self-contained document: inline CSS, inline JS, no CDN — so the demo
-- works offline. It polls `/source` and re-renders when the buffer text changes; the
-- mount itself is plain request/response, with no push involved.
--------------------------------------------------------------------------------
-- NOTE the [==[ ]==] long bracket: the JS below contains `]]` (in the link regex),
-- which would close a plain [[ ]] string right in the middle of the page.
local PAGE = [==[
<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>bemtvi — live markdown preview</title>
<style>
  :root { color-scheme: light dark; }
  body {
    margin: 0 auto; padding: 2rem 1.5rem; max-width: 44rem;
    font: 16px/1.65 ui-sans-serif, system-ui, -apple-system, sans-serif;
  }
  #status {
    position: fixed; top: .6rem; right: .8rem; font-size: .72rem;
    opacity: .55; font-family: ui-monospace, monospace;
  }
  h1, h2, h3 { line-height: 1.25; margin: 1.6em 0 .5em; }
  h1 { border-bottom: 1px solid color-mix(in srgb, currentColor 18%, transparent); padding-bottom: .25em; }
  code {
    font: .88em ui-monospace, SFMono-Regular, Menlo, monospace;
    background: color-mix(in srgb, currentColor 10%, transparent);
    padding: .15em .35em; border-radius: 4px;
  }
  pre {
    background: color-mix(in srgb, currentColor 7%, transparent);
    padding: .9rem 1rem; border-radius: 8px; overflow-x: auto;
  }
  pre code { background: none; padding: 0; }
  blockquote {
    margin: 1em 0; padding: .1em 1em;
    border-left: 3px solid color-mix(in srgb, currentColor 25%, transparent);
    opacity: .8;
  }
  hr { border: none; border-top: 1px solid color-mix(in srgb, currentColor 20%, transparent); margin: 2em 0; }
  a { color: inherit; }
  table { border-collapse: collapse; }
  td, th { border: 1px solid color-mix(in srgb, currentColor 20%, transparent); padding: .35em .7em; }
</style>
</head>
<body>
<div id="status">connecting…</div>
<article id="out"></article>
<script>
// A deliberately small markdown renderer — the point of this demo is the SERVER, and
// rendering is the plugin's business. Swap in your framework of choice.
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);

function inline(s) {
  return esc(s)
    .replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>")
    .replace(/~~([^~]+)~~/g, "<del>$1</del>")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2">$1</a>');
}

function render(md) {
  const out = [];
  const lines = md.split("\n");
  let i = 0, list = null;
  const closeList = () => { if (list) { out.push(`</${list}>`); list = null; } };
  // A list item may wrap onto following lines; swallow them into this item rather than
  // letting them fall out as a stray paragraph.
  const takeItem = (first) => {
    const buf = [first];
    for (i++; i < lines.length; i++) {
      const l = lines[i];
      if (l.trim() === "" || /^\s*([-*+]|\d+[.)])\s+/.test(l) || /^(#|>|```|---|\s*\|)/.test(l)) break;
      buf.push(l.trim());
    }
    return buf.join(" ");
  };
  while (i < lines.length) {
    const line = lines[i];
    let m;
    if (/^```/.test(line)) {                       // fenced code
      closeList();
      const lang = line.slice(3).trim();
      const buf = [];
      for (i++; i < lines.length && !/^```/.test(lines[i]); i++) buf.push(lines[i]);
      i++;
      out.push(`<pre><code data-lang="${esc(lang)}">${esc(buf.join("\n"))}</code></pre>`);
    } else if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      closeList();
      out.push(`<h${m[1].length}>${inline(m[2])}</h${m[1].length}>`);
      i++;
    } else if (/^(---|\*\*\*)\s*$/.test(line)) {
      closeList(); out.push("<hr>"); i++;
    } else if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] || "")) {
      closeList();                                 // GFM table: header, separator, rows
      const cells = (row) => row.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(line);
      i += 2;
      const body = [];
      for (; i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i]); i++) body.push(cells(lines[i]));
      out.push(
        "<table><thead><tr>" + head.map((c) => `<th>${inline(c)}</th>`).join("") +
        "</tr></thead><tbody>" +
        body.map((r) => "<tr>" + r.map((c) => `<td>${inline(c)}</td>`).join("") + "</tr>").join("") +
        "</tbody></table>"
      );
    } else if ((m = line.match(/^\s*[-*+]\s+(.*)$/))) {
      if (list !== "ul") { closeList(); out.push("<ul>"); list = "ul"; }
      out.push(`<li>${inline(takeItem(m[1]))}</li>`);
    } else if ((m = line.match(/^\s*\d+[.)]\s+(.*)$/))) {
      if (list !== "ol") { closeList(); out.push("<ol>"); list = "ol"; }
      out.push(`<li>${inline(takeItem(m[1]))}</li>`);
    } else if (/^>/.test(line)) {
      closeList();                                 // one blockquote per RUN of > lines
      const buf = [];
      for (; i < lines.length && /^>/.test(lines[i]); i++) buf.push(lines[i].replace(/^>\s?/, ""));
      out.push(`<blockquote>${inline(buf.join(" "))}</blockquote>`);
    } else if (line.trim() === "") {
      closeList(); i++;
    } else {
      closeList();
      const buf = [];
      for (; i < lines.length && lines[i].trim() !== "" && !/^(#|>|```|---|\s*\|)/.test(lines[i]); i++) {
        buf.push(lines[i]);
      }
      out.push(`<p>${inline(buf.join(" "))}</p>`);
    }
  }
  closeList();
  return out.join("\n");
}

// Live-reload is the FRONTEND's job (the mount is plain request/response): poll the
// source and re-render when it changes.
const status = document.getElementById("status");
const out = document.getElementById("out");
let last = null;

async function tick() {
  try {
    // Relative to this page — the mount's own prefix, whatever origin it is served from.
    const res = await fetch("source", { cache: "no-store" });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const md = await res.text();
    if (md !== last) {
      last = md;
      out.innerHTML = render(md);
    }
    status.textContent = "live";
  } catch (e) {
    status.textContent = "editor gone (" + e.message + ")";
  }
}
tick();
setInterval(tick, 400);
</script>
</body>
</html>
]==]

--------------------------------------------------------------------------------
-- The mount. One handler, routing on `req.path` — which is MOUNT-RELATIVE: a GET of
-- /plugin/preview/source arrives here as "/source". That is what lets this same handler
-- work under any prefix or origin without knowing where it was mounted.
--------------------------------------------------------------------------------
local preview

btv.http
  .mount({
    name = "preview",

    on_request = function(req, respond)
      -- "/" (the mount root, with or without a trailing slash) — the page shell.
      if req.path == "/" then
        respond({
          headers = { ["content-type"] = "text/html; charset=utf-8" },
          body = PAGE,
        })

      -- "/source" — the current buffer's text, which the page polls and renders.
      elseif req.path == "/source" then
        respond({
          headers = {
            ["content-type"] = "text/plain; charset=utf-8",
            -- The page polls; never let a proxy or the browser serve a stale buffer.
            ["cache-control"] = "no-store",
          },
          body = table.concat(btv.buf.lines(0, 0, -1), "\n"),
        })

      -- "/info" — a small JSON endpoint, to show a second content type + `req.query`.
      -- Try:  <url>info?pretty=1
      elseif req.path == "/info" then
        local info = {
          name = btv.buf.name(0),
          lines = #btv.buf.lines(0, 0, -1),
          mount = req.name,
          method = req.method,
        }
        respond({
          headers = { ["content-type"] = "application/json" },
          body = btv.json.encode(info, { pretty = req.query.pretty == "1" }),
        })

      -- Anything else under this mount is the plugin's own 404 (the editor only 404s
      -- names that are not mounted at all).
      else
        respond({
          status = 404,
          headers = { ["content-type"] = "text/plain" },
          body = "no such page: " .. req.path .. "\n",
        })
      end
    end,
  })
  :next(function(mount)
    preview = mount
    show("markdown preview mounted at " .. mount:url() .. "  —  press \\p to open it")
  end)
  :catch(function(err)
    -- A bind failure (the port is taken) or a duplicate name REJECTS — it never falls
    -- back to some other port behind your back.
    show("could not mount the preview: " .. tostring(err.message))
  end)

--------------------------------------------------------------------------------
-- Keys
--------------------------------------------------------------------------------

-- \p — open the preview in the real browser.
btv.keymap.set("n", "<leader>p", function()
  if not preview then
    return show("the preview is not mounted (yet?)")
  end
  btv.ui.open(preview:url())
end)

-- \u — print the URL (and the editor's origin, shared by every mount).
btv.keymap.set("n", "<leader>u", function()
  if not preview then
    return show("the preview is not mounted (yet?)")
  end
  show(("%s   (origin: %s)"):format(preview:url(), btv.http.origin()))
end)

-- \c — close the mount. The URL starts 404ing; the listener stays up (an idle listener
-- costs nothing, and the origin survives a plugin reload).
btv.keymap.set("n", "<leader>c", function()
  if not preview then
    return show("the preview is not mounted")
  end
  preview:close()
  show("preview closed — reload the page to see the editor's 404")
end)

show("btv.http.mount demo loading — mounting the preview…")
