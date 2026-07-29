use crate::support::*;

// ----- statusline option plumbing (string-valued global option, Phase 1) -----

#[tokio::test]
async fn vim_opt_statusline_round_trips_through_core() {
    let (rpc, _incoming) = start(None).await;
    // The `statusline` string global written through vim.opt reaches the core and
    // reads back the same value via vim.o / vim.opt and the `stl` abbreviation —
    // proving the String OptionValue threads through the Lua bridge and mirror,
    // not just a Lua-side table.
    exec_lua(&rpc, r#"vim.opt.statusline = "%f %l,%c""#).await;
    let via_o = exec_lua(&rpc, r#"return vim.o.statusline"#).await;
    assert_eq!(via_o.as_str(), Some("%f %l,%c"));
    // vim.opt.statusline is an Option object (as in neovim), read via :get().
    let via_opt = exec_lua(&rpc, r#"return vim.opt.statusline:get()"#).await;
    assert_eq!(via_opt.as_str(), Some("%f %l,%c"));
    let via_abbrev = exec_lua(&rpc, r#"return vim.o.stl"#).await;
    assert_eq!(via_abbrev.as_str(), Some("%f %l,%c"));
}

#[tokio::test]
async fn vim_o_statusline_read_reflects_set_ex_command() {
    let (rpc, _incoming) = start(None).await;
    // Reading vim.o.statusline reflects a value set via the `:set` ex path (the
    // server-pushed mirror), the same home the Lua write reaches.
    feed(&rpc, ":set statusline=%f<CR>");
    let via_o = exec_lua(&rpc, r#"return vim.o.statusline"#).await;
    assert_eq!(via_o.as_str(), Some("%f"));
}

#[tokio::test]
async fn set_statusline_query_echoes_value_with_escaped_spaces() {
    let (rpc, mut incoming) = start(None).await;
    // `:set statusline=…` carries spaces via vim's `\ ` escaping (the value would
    // otherwise split into separate `:set` tokens). The escaped space survives as
    // a real space, and `:set statusline?` echoes the stored value back.
    feed(&rpc, r":set statusline=%f\ %l,%c<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set statusline?<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert_eq!(msg, "statusline=%f %l,%c");
}

#[tokio::test]
async fn set_statusline_reset_clears_to_default() {
    let (rpc, _incoming) = start(None).await;
    // `:set statusline&` resets the option to its default (empty).
    feed(&rpc, ":set statusline=%f<CR>");
    assert_eq!(
        exec_lua(&rpc, r#"return vim.o.statusline"#).await.as_str(),
        Some("%f")
    );
    feed(&rpc, ":set statusline&<CR>");
    assert_eq!(
        exec_lua(&rpc, r#"return vim.o.statusline"#).await.as_str(),
        Some("")
    );
}

// ----- statusline rendering (the %-format engine, Phase 3) -----
//
// These assert on the per-window `status` segment array the server now projects
// (text + a style-palette id per highlighted run), driven by the `'statusline'`
// option through the core engine. The default UI is 80 cols, so the short
// formats below never hit `%<` truncation.

/// The first window's `status` segments from a redraw, as `(text, style_id)` —
/// `style_id` is `None` for a segment painted in the base `StatusLine` look.
fn status_segments(map: &[(Value, Value)]) -> Vec<(String, Option<usize>)> {
    field(map, "status")
        .and_then(Value::as_array)
        .expect("a status segment array")
        .iter()
        .map(|seg| {
            let Value::Map(m) = seg else {
                panic!("status segment is not a map")
            };
            let get = |key: &str| {
                m.iter()
                    .find(|(k, _)| k.as_str() == Some(key))
                    .map(|(_, v)| v)
            };
            let text = get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let style = get("style").and_then(Value::as_u64).map(|n| n as usize);
            (text, style)
        })
        .collect()
}

/// The whole status line as one string — the analogue of nvim's
/// `nvim_eval_statusline(...).str`.
fn status_text(map: &[(Value, Value)]) -> String {
    status_segments(map).into_iter().map(|(t, _)| t).collect()
}

#[tokio::test]
async fn statusline_literal_renders_verbatim() {
    let (rpc, mut incoming) = start(None).await;
    // A literal format (no %-items) paints exactly its text — no fill without a
    // `%=`, so it isn't padded to the window width.
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=hello<CR>").await;
    assert_eq!(status_text(&map), "hello");
    assert_eq!(status_segments(&map), vec![("hello".to_string(), None)]);
}

#[tokio::test]
async fn statusline_fields_expand_from_window_state() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>defgh<Esc>gg"); // 2 lines; gg -> line 1, col 1
    let map = redraw_after(&rpc, &mut incoming, r":set statusline=%f\ %l,%c<CR>").await;
    assert_eq!(status_text(&map), "[No Name] 1,1");
}

#[tokio::test]
async fn statusline_highlight_group_resolves_to_palette_style() {
    let (rpc, mut incoming) = start(None).await;
    // `%#Group#` switches the highlight for the text that follows; the server
    // resolves it to a style-palette id (the base segment before it has none).
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'MyStl', { fg = '#ff0000', bg = '#00ff00' })",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=a%#MyStl#b<CR>").await;
    let segs = status_segments(&map);
    assert_eq!(segs[0], ("a".to_string(), None));
    assert_eq!(segs[1].0, "b");
    let id = segs[1]
        .1
        .expect("the %#MyStl# run carries a resolved style");

    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("style palette");
    let Value::Map(style) = &styles[id] else {
        panic!("style entry is not a map")
    };
    assert_eq!(hl_color(style, "fg"), Some(hex("ff0000")));
    assert_eq!(hl_color(style, "bg"), Some(hex("00ff00")));
}

#[tokio::test]
async fn statusline_whole_vlua_expression_renders_result() {
    let (rpc, mut incoming) = start(None).await;
    // `%!expr` — the whole statusline is the eval result. Only v:lua.* is
    // supported; the prefix is stripped to the bare Lua call.
    exec_lua(&rpc, "_G.my_stl = function() return 'HELLO' end").await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=%!v:lua.my_stl()<CR>").await;
    assert_eq!(status_text(&map), "HELLO");
}

#[tokio::test]
async fn statusline_embedded_vlua_expression_renders_result() {
    let (rpc, mut incoming) = start(None).await;
    // `%{expr}` — the result is literal text spliced into the surrounding format.
    exec_lua(&rpc, "_G.tag = function() return 'OK' end").await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=[%{v:lua.tag()}]<CR>").await;
    assert_eq!(status_text(&map), "[OK]");
}

#[tokio::test]
async fn statusline_default_shows_mode_file_and_ruler() {
    let (rpc, mut incoming) = start(None).await;
    // Empty 'statusline' renders the built-in default through the same engine:
    // ` MODE  file %= line,col `.
    let map = redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let text = status_text(&map);
    assert!(text.contains("NORMAL"), "default shows the mode: {text:?}");
    assert!(
        text.contains("[No Name]"),
        "default shows the file: {text:?}"
    );
    assert!(
        text.trim_end().ends_with("1,1"),
        "default ends with the line,col ruler: {text:?}"
    );
}

#[tokio::test]
async fn statusline_bare_variable_expression_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    // A bare variable in `%{}` is not evaluable in pure core (nxvim has no
    // Vimscript variables; those would need a `v:lua.` bridge). It renders a loud
    // error naming the variable rather than silently expanding to nothing.
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=%{somevar}<CR>").await;
    let text = status_text(&map);
    assert!(text.contains("E:"), "loud, not empty: {text:?}");
    assert!(text.contains("somevar"), "names the variable: {text:?}");
}

// ----- `%{&option}` expressions (Phase 5: option references in the statusline) -----
//
// `%{…}` items that aren't `v:lua.…` run through the pure core Vim-expression
// evaluator, which now understands `&option` references (resolved against the
// buffer-display state the statusline context carries), the ternary `?:`, and the
// comparison/logical operators — the faithful subset a real statusline format uses.

#[tokio::test]
async fn statusline_ampersand_fileencoding_renders_buffer_encoding() {
    let (rpc, mut incoming) = start(None).await;
    // A fresh buffer is utf-8; `%{&fileencoding}` (and the `&fenc` abbreviation)
    // splice that in like neovim, which has no `%`-letter for the encoding.
    let map = redraw_after(
        &rpc,
        &mut incoming,
        ":set statusline=[%{&fileencoding}]<CR>",
    )
    .await;
    assert_eq!(status_text(&map), "[utf-8]");

    // Changing the buffer's `'fileencoding'` flows straight through to the next paint.
    feed(&rpc, ":set fileencoding=latin1<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=[%{&fenc}]<CR>").await;
    assert_eq!(status_text(&map), "[latin1]");
}

#[tokio::test]
async fn statusline_ampersand_bomb_ternary() {
    let (rpc, mut incoming) = start(None).await;
    // The headline example: `&bomb` is a boolean option (0/1), and the ternary
    // turns it into a `[bom]` tag or nothing. `bomb` is off by default → empty.
    let map = redraw_after(
        &rpc,
        &mut incoming,
        r#":set statusline=enc%{&bomb?"[bom]":""}<CR>"#,
    )
    .await;
    assert_eq!(status_text(&map), "enc");

    // Turn `'bomb'` on and the same format now shows the tag.
    feed(&rpc, ":set bomb<CR>");
    let map = redraw_after(
        &rpc,
        &mut incoming,
        r#":set statusline=enc%{&bomb?"[bom]":""}<CR>"#,
    )
    .await;
    assert_eq!(status_text(&map), "enc[bom]");
}

#[tokio::test]
async fn statusline_option_comparison_and_concat() {
    let (rpc, mut incoming) = start(None).await;
    // String-option comparison plus a ternary: a utf-8 buffer is "unicode". The
    // result concatenates into the surrounding literal text.
    let map = redraw_after(
        &rpc,
        &mut incoming,
        r#":set statusline=%{&fileencoding=="utf-8"?"unicode":"legacy"}<CR>"#,
    )
    .await;
    assert_eq!(status_text(&map), "unicode");

    feed(&rpc, ":set fileencoding=latin1<CR>");
    let map = redraw_after(
        &rpc,
        &mut incoming,
        r#":set statusline=%{&fileencoding=="utf-8"?"unicode":"legacy"}<CR>"#,
    )
    .await;
    assert_eq!(status_text(&map), "legacy");
}

#[tokio::test]
async fn statusline_ampersand_buftype_distinguishes_chrome_from_documents() {
    let (rpc, mut incoming) = start(None).await;
    // `&buftype` is vim's "is this window a document or editor chrome" signal, and the
    // canonical one here too (`Editor::buffer_buftype`) — a status line keys off it to
    // skip its file-only pieces. An ordinary buffer reports `""`.
    let map = redraw_after(&rpc, &mut incoming, r#":set statusline=[%{&buftype}]<CR>"#).await;
    assert_eq!(status_text(&map), "[]");

    // A built-in listing is a `nofile` scratch surface, and says so.
    let map = redraw_after(&rpc, &mut incoming, ":messages<CR>").await;
    let panel = field(&map, "windows")
        .and_then(Value::as_array)
        .expect("a window array")
        .iter()
        .filter_map(|w| {
            let Value::Map(m) = w else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("status"))
                .and_then(|(_, v)| v.as_array())
        })
        .flatten()
        .filter_map(|seg| {
            let Value::Map(m) = seg else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
        })
        .collect::<String>();
    assert!(
        panel.contains("[nofile]"),
        "the panel window reports its buftype, got {panel:?}"
    );
}

#[tokio::test]
async fn statusline_hand_rolled_noeol_marker_matches_the_builtin() {
    // The two `&option`s together reproduce the default status line's `[noeol]` rule —
    // the expression `examples/endofline/init.lua` hands users, so it has to evaluate.
    // Set through `vim.o` as the example does: the `:set` form would need every space
    // inside `%{…}` backslash-escaped.
    let path = temp_path("stl_noeol_expr");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    exec_lua(
        &rpc,
        r#"vim.o.statusline = '%{&endofline || &buftype != "" ? "" : "[noeol]"}'"#,
    )
    .await;
    assert_eq!(
        status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await),
        "[noeol]",
        "an unterminated file is marked, as the built-in marks it"
    );

    // A terminated file renders nothing — again matching the built-in.
    feed(&rpc, ":set eol<CR>");
    assert_eq!(
        status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await),
        ""
    );
}

#[tokio::test]
async fn statusline_ampersand_fixeol_says_whether_the_write_will_terminate() {
    // `&endofline` alone can't tell a preserved missing terminator from one that is
    // about to be supplied — that is `'fixendofline'`, so a hand-rolled marker wanting
    // to distinguish them needs it to resolve as well as its sibling.
    let path = temp_path("stl_fixeol_expr");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    exec_lua(
        &rpc,
        r#"vim.o.statusline = '%{&endofline ? "" : (&fixeol ? "[+eol]" : "[noeol]")}'"#,
    )
    .await;
    assert_eq!(
        status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await),
        "[+eol]",
        "the default 'fixendofline' means the next write supplies the terminator"
    );

    feed(&rpc, ":set nofixeol<CR>");
    assert_eq!(
        status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await),
        "[noeol]",
        "opted out, the missing terminator is preserved instead"
    );
}

#[tokio::test]
async fn statusline_unknown_option_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    // An `&option` the statusline context doesn't carry is not silently empty — it
    // names the unknown option (E518), per CLAUDE.md's no-silent-stub rule.
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=%{&shiftwidth}<CR>").await;
    let text = status_text(&map);
    assert!(text.contains("E:"), "loud, not empty: {text:?}");
    assert!(text.contains("E518"), "unknown-option error: {text:?}");
    assert!(text.contains("shiftwidth"), "names the option: {text:?}");
}

// ----- `nx.statusline` — the declarative segment registry (lualine shape) -----
//
// A `nx.statusline.setup{}` layout composes named segments into the same `status`
// array the `%`-format path projects. Built-ins resolve natively each frame;
// custom segments publish their cells on (re)render and the server caches them.

#[tokio::test]
async fn nx_statusline_builtins_compose() {
    let (rpc, mut incoming) = start(None).await;
    // Built-in segments resolve from the window's status context: `mode` on the
    // left, `filename`, and `location` pushed to the right past the `%=` fill.
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ left = { "mode", "filename" }, right = { "location" } }"#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let text = status_text(&map);
    assert!(text.contains("NORMAL"), "mode segment: {text:?}");
    assert!(text.contains("[No Name]"), "filename segment: {text:?}");
    assert!(
        text.trim_end().ends_with("1:1"),
        "location pushed right: {text:?}"
    );
}

#[tokio::test]
async fn nx_statusline_layout_overrides_statusline_format() {
    let (rpc, mut incoming) = start(None).await;
    // While a segment layout is active it takes precedence over `'statusline'`.
    feed(&rpc, ":set statusline=ZZZ<CR>");
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ left = { "filename" }, right = {} }"#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let text = status_text(&map);
    assert!(!text.contains("ZZZ"), "the format is overridden: {text:?}");
    assert!(text.contains("[No Name]"), "segments render: {text:?}");
}

#[tokio::test]
async fn nx_statusline_custom_segment_renders_published_cells() {
    let (rpc, mut incoming) = start(None).await;
    // A custom segment's returned cells reach the bar, each carrying its `hl`
    // resolved to a style-palette id.
    exec_lua(
        &rpc,
        r#"
        vim.api.nvim_set_hl(0, 'StatusGit', { fg = '#ff8800' })
        nx.statusline.segment{
          name = 'git',
          render = function() return { { text = ' main', hl = 'StatusGit' } } end,
        }
        nx.statusline.setup{ left = { 'git' }, right = {} }
        "#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let segs = status_segments(&map);
    let git = segs
        .iter()
        .find(|(t, _)| t.contains("main"))
        .unwrap_or_else(|| panic!("git segment present: {segs:?}"));
    assert!(
        git.1.is_some(),
        "the custom cell carries its StatusGit style: {segs:?}"
    );
}

#[tokio::test]
async fn nx_statusline_invalidate_repaints_segment() {
    let (rpc, mut incoming) = start(None).await;
    // The async pattern: a segment reads cached data; `invalidate` re-runs its
    // render so the new value reaches the next paint.
    exec_lua(
        &rpc,
        r#"
        _G.branch = 'one'
        nx.statusline.segment{ name = 'git', render = function() return { { text = _G.branch } } end }
        nx.statusline.setup{ left = { 'git' }, right = {} }
        "#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        status_text(&map).contains("one"),
        "initial render: {:?}",
        status_text(&map)
    );

    exec_lua(
        &rpc,
        r#"_G.branch = 'two'; nx.statusline.invalidate('git')"#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let text = status_text(&map);
    assert!(text.contains("two"), "invalidate re-rendered: {text:?}");
    assert!(!text.contains("one"), "the stale value is gone: {text:?}");
}

#[tokio::test]
async fn nx_statusline_event_rerenders_on_declared_event() {
    // A segment declaring `events = { 'BufEnter' }` is re-rendered whenever that
    // autocmd fires — no explicit invalidate needed. We switch to a second buffer
    // to fire BufEnter (`:enew` reuses the empty one, so its id never changes).
    let dir = temp_dir("nx_stl_evt");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start(Some(a.to_str().unwrap().to_string())).await;
    exec_lua(
        &rpc,
        r#"
        _G.renders = 0
        nx.statusline.segment{
          name = 'counter', events = { 'BufEnter' },
          render = function() _G.renders = _G.renders + 1; return { { text = 'r' .. _G.renders } } end,
        }
        nx.statusline.setup{ left = { 'counter' }, right = {} }
        "#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        status_text(&map).contains("r1"),
        "rendered once at setup: {:?}",
        status_text(&map)
    );

    // Editing b.txt enters a different buffer → BufEnter → the counter re-renders.
    let map = redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    let text = status_text(&map);
    assert!(
        text.contains("r2"),
        "BufEnter re-rendered the segment: {text:?}"
    );
}

#[tokio::test]
async fn nx_statusline_unknown_segment_errors_loudly() {
    let (rpc, _incoming) = start(None).await;
    // An unknown segment name is a hard error at setup (no silent blank) — the
    // same no-stub rule the completion source list enforces.
    let res = exec_lua(
        &rpc,
        r#"
        local ok, err = pcall(function() nx.statusline.setup{ left = { 'nonesuch' } } end)
        return ok and 'no-error' or tostring(err)
        "#,
    )
    .await;
    let s = res.as_str().unwrap_or("");
    assert!(s.contains("unknown segment"), "loud error: {s:?}");
    assert!(s.contains("nonesuch"), "names the segment: {s:?}");
}

/// Every window's status line as a string, in layout order — for asserting that
/// a `nx.statusline` custom segment renders a distinct cell per window.
fn all_window_status_texts(map: &[(Value, Value)]) -> Vec<String> {
    field(map, "windows")
        .and_then(Value::as_array)
        .expect("a windows array")
        .iter()
        .map(|w| {
            let Value::Map(wm) = w else {
                panic!("window is not a map")
            };
            wm.iter()
                .find(|(k, _)| k.as_str() == Some("status"))
                .and_then(|(_, v)| v.as_array())
                .map(|segs| {
                    segs.iter()
                        .filter_map(|seg| {
                            let Value::Map(m) = seg else { return None };
                            m.iter()
                                .find(|(k, _)| k.as_str() == Some("text"))
                                .and_then(|(_, v)| v.as_str())
                                .map(str::to_string)
                        })
                        .collect::<String>()
                })
                .unwrap_or_default()
        })
        .collect()
}

#[tokio::test]
async fn nx_statusline_custom_segment_caches_per_window() {
    let (rpc, mut incoming) = start(None).await;
    // A custom segment whose cell encodes the window's id and whether it is the
    // focused window — proving each window renders against its own `ctx`.
    exec_lua(
        &rpc,
        r#"
        nx.statusline.segment{
          name = 'wi',
          render = function(ctx) return { { text = 'w' .. ctx.win .. (ctx.focused and '*' or '-') } } end,
        }
        nx.statusline.setup{ left = { 'wi' }, right = {} }
        "#,
    )
    .await;
    // Sole window: focused.
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let st = all_window_status_texts(&map);
    assert_eq!(st.len(), 1, "one window: {st:?}");
    assert!(st[0].contains('*'), "the sole window is focused: {st:?}");

    // Split into two windows: distinct per-window cells, exactly one focused.
    let map = redraw_after(&rpc, &mut incoming, ":split<CR>").await;
    let st = all_window_status_texts(&map);
    assert_eq!(st.len(), 2, "two windows after :split: {st:?}");
    assert_ne!(st[0], st[1], "per-window cells differ by window id: {st:?}");
    assert_eq!(
        st.iter().filter(|s| s.contains('*')).count(),
        1,
        "exactly one window is focused: {st:?}"
    );
    assert_eq!(
        st.iter().filter(|s| s.contains('-')).count(),
        1,
        "the other window is unfocused: {st:?}"
    );
}

#[tokio::test]
async fn nx_statusline_custom_segment_follows_focus_change() {
    let (rpc, mut incoming) = start(None).await;
    // The `focused` flag must track focus moving between windows — re-rendered by
    // the server on the focus change, with no declared `WinEnter` event needed.
    exec_lua(
        &rpc,
        r#"
        nx.statusline.segment{
          name = 'wi',
          render = function(ctx) return { { text = 'w' .. ctx.win .. (ctx.focused and '*' or '-') } } end,
        }
        nx.statusline.setup{ left = { 'wi' }, right = {} }
        "#,
    )
    .await;
    let before = all_window_status_texts(&redraw_after(&rpc, &mut incoming, ":split<CR>").await);
    assert_eq!(before.len(), 2, "two windows: {before:?}");

    // Move focus to the other window: the `*`/`-` markers swap windows.
    let after = all_window_status_texts(&redraw_after(&rpc, &mut incoming, "<C-w>w").await);
    assert_eq!(after.len(), 2, "still two windows: {after:?}");
    assert_eq!(
        after.iter().filter(|s| s.contains('*')).count(),
        1,
        "still exactly one focused window: {after:?}"
    );
    assert_ne!(
        before, after,
        "the focused marker followed the focus change: {before:?} -> {after:?}"
    );
}

#[tokio::test]
async fn nx_statusline_custom_segment_tracks_per_window_buffer() {
    // A segment reading its window's buffer (`ctx.buf`) shows a different value in
    // each window when they hold different buffers — re-rendered server-side when a
    // window swaps its buffer, with no declared event on the segment.
    let dir = temp_dir("nx_stl_perwin_buf");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start(Some(a.to_str().unwrap().to_string())).await;
    exec_lua(
        &rpc,
        r#"
        nx.statusline.segment{
          name = 'buftail',
          render = function(ctx)
            local name = nx.buf.name(ctx.buf)
            return { { text = name:match('[^/]+$') or '?' } }
          end,
        }
        nx.statusline.setup{ left = { 'buftail' }, right = {} }
        "#,
    )
    .await;
    // Split, then edit b.txt in the focused window: the two windows now show
    // different buffers, so the segment differs between them.
    feed(&rpc, ":split<CR>");
    let map = redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    let st = all_window_status_texts(&map);
    assert_eq!(st.len(), 2, "two windows: {st:?}");
    assert!(
        st.iter().any(|s| s.contains("a.txt")),
        "one window still shows a.txt: {st:?}"
    );
    assert!(
        st.iter().any(|s| s.contains("b.txt")),
        "the other window shows b.txt: {st:?}"
    );
}

#[tokio::test]
async fn nx_statusline_window_local_layout_overrides_global() {
    let (rpc, mut incoming) = start(None).await;
    // A global layout shows the filename in every window…
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ left = { 'filename' }, right = {} }"#,
    )
    .await;
    let both = all_window_status_texts(&redraw_after(&rpc, &mut incoming, ":split<CR>").await);
    assert_eq!(both.len(), 2, "two windows: {both:?}");
    assert!(
        both.iter().all(|s| s.contains("[No Name]")),
        "both windows inherit the global filename layout: {both:?}"
    );

    // …but a window-local layout (win = 0, the focused window) overrides it for
    // that one window — it shows the mode while the other still shows the filename.
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ win = 0, left = { 'mode' }, right = {} }"#,
    )
    .await;
    let st = all_window_status_texts(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert_eq!(
        st.iter().filter(|s| s.contains("NORMAL")).count(),
        1,
        "exactly one window uses the local mode layout: {st:?}"
    );
    assert_eq!(
        st.iter().filter(|s| s.contains("[No Name]")).count(),
        1,
        "the other window still inherits the global layout: {st:?}"
    );
}

#[tokio::test]
async fn nx_statusline_window_can_opt_back_to_format() {
    let (rpc, mut incoming) = start(None).await;
    // The per-region mix: a global segment layout is active, but one window opts
    // back to the `'statusline'` %-format.
    feed(&rpc, ":set statusline=ZZZ<CR>");
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ left = { 'filename' }, right = {} }"#,
    )
    .await;
    feed(&rpc, ":split<CR>");
    exec_lua(&rpc, r#"nx.statusline.setup{ win = 0, format = true }"#).await;
    let st = all_window_status_texts(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert_eq!(st.len(), 2, "two windows: {st:?}");
    assert!(
        st.iter().any(|s| s.trim() == "ZZZ"),
        "the opted-out window shows the %-format: {st:?}"
    );
    assert!(
        st.iter().any(|s| s.contains("[No Name]")),
        "the other window still shows the segment layout: {st:?}"
    );

    // reset(0) drops the override so the window re-inherits the global layout.
    exec_lua(&rpc, r#"nx.statusline.reset(0)"#).await;
    let st = all_window_status_texts(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert!(
        st.iter().all(|s| s.contains("[No Name]")),
        "reset re-inherits the global layout in both windows: {st:?}"
    );
    assert!(
        !st.iter().any(|s| s.trim() == "ZZZ"),
        "the %-format override is gone: {st:?}"
    );
}

#[tokio::test]
async fn nx_statusline_layout_does_not_leak_into_custom_tabline() {
    let (rpc, mut incoming) = start(None).await;
    // A segment layout drives the status line, but a custom `'tabline'` must still
    // render through the %-format engine — the layout is a status-line surface only.
    exec_lua(
        &rpc,
        r#"nx.statusline.setup{ left = { 'filename' }, right = {} }"#,
    )
    .await;
    feed(&rpc, ":set showtabline=2<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set tabline=TABXYZ<CR>").await;
    let tabline: String = field(&map, "tabline_segments")
        .and_then(Value::as_array)
        .expect("tabline_segments array")
        .iter()
        .filter_map(|seg| {
            let Value::Map(m) = seg else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        tabline.trim(),
        "TABXYZ",
        "the custom tabline renders the %-format, not the segment layout: {tabline:?}"
    );
}

// ----- laststatus (per-window status visibility + global status, Phase 6) -----
//
// `'laststatus'` decides where status lines go: `0` never, `1` only with ≥2
// windows, `2` always (the default), `3` a single global status line at the
// bottom. The per-window `status_visible` flag drives whether the client carves
// a status row off a window; `global_status` (top-level) carries the mode-3 bar.

/// Whether the first window paints its own status row (`windows[0].status_visible`).
fn window0_status_visible(map: &[(Value, Value)]) -> bool {
    field(map, "status_visible")
        .and_then(Value::as_bool)
        .expect("a status_visible flag")
}

/// The top-level `global_status` segments (`laststatus=3`), or `None` for the
/// per-window modes (where it is `Nil`).
fn global_status_segments(map: &[(Value, Value)]) -> Option<Vec<(String, Option<usize>)>> {
    let arr = map_get(map, "global_status")?.as_array()?;
    Some(
        arr.iter()
            .map(|seg| {
                let Value::Map(m) = seg else {
                    panic!("global_status segment is not a map")
                };
                let text = m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("text"))
                    .and_then(|(_, v)| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let style = m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("style"))
                    .and_then(|(_, v)| v.as_u64())
                    .map(|n| n as usize);
                (text, style)
            })
            .collect(),
    )
}

/// The whole global status line as one string.
fn global_status_text(map: &[(Value, Value)]) -> String {
    global_status_segments(map)
        .unwrap_or_default()
        .into_iter()
        .map(|(t, _)| t)
        .collect()
}

#[tokio::test]
async fn laststatus_round_trips_through_set_and_vim_o() {
    let (rpc, mut incoming) = start(None).await;
    // Default is 2 (every window has a status line).
    assert_eq!(
        exec_lua(&rpc, "return vim.o.laststatus").await.as_u64(),
        Some(2)
    );
    // `:set laststatus=0` reaches the core; `:set laststatus?` echoes it back, and
    // `vim.o` (and its `ls` abbreviation) reflect the same value.
    let map = redraw_after(
        &rpc,
        &mut incoming,
        ":set laststatus=0<CR>:set laststatus?<CR>",
    )
    .await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("laststatus=0")
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.laststatus").await.as_u64(),
        Some(0)
    );
    // Writing through vim.o round-trips back into the core (the next redraw honors it).
    exec_lua(&rpc, "vim.o.laststatus = 3").await;
    assert_eq!(exec_lua(&rpc, "return vim.o.ls").await.as_u64(), Some(3));
}

#[tokio::test]
async fn laststatus_two_shows_per_window_status_by_default() {
    let (rpc, mut incoming) = start(None).await;
    // The default: the sole window paints its own status row, so the 25-row
    // windows area leaves 24 text rows and there is no global status bar.
    let map = redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    assert!(
        window0_status_visible(&map),
        "default ls=2 keeps a per-window status"
    );
    assert_eq!(lines_len(&map), 24, "the status row costs one text row");
    assert!(
        global_status_segments(&map).is_none(),
        "no global status bar in mode 2"
    );
}

#[tokio::test]
async fn laststatus_zero_hides_window_status_and_reclaims_the_row() {
    let (rpc, mut incoming) = start(None).await;
    // `laststatus=0` hides the status line entirely; the freed bottom row becomes
    // text, so the single window now shows all 25 rows.
    let map = redraw_after(&rpc, &mut incoming, ":set laststatus=0<CR>").await;
    assert!(!window0_status_visible(&map), "mode 0 hides the status row");
    assert_eq!(lines_len(&map), 25, "the freed row becomes text");
    assert!(
        global_status_segments(&map).is_none(),
        "mode 0 has no global bar"
    );
}

#[tokio::test]
async fn laststatus_one_hides_status_until_a_second_window_opens() {
    let (rpc, mut incoming) = start(None).await;
    // `laststatus=1`: with a single window there is no status line (the row goes
    // to text)…
    let solo = redraw_after(&rpc, &mut incoming, ":set laststatus=1<CR>").await;
    assert!(
        !window0_status_visible(&solo),
        "mode 1 hides status with one window"
    );
    assert_eq!(lines_len(&solo), 25, "the row is text while solo");
    // …but a horizontal split brings the per-window status lines back.
    let split = redraw_after(&rpc, &mut incoming, ":split<CR>").await;
    assert!(
        window0_status_visible(&split),
        "mode 1 shows status once a second window opens"
    );
}

#[tokio::test]
async fn laststatus_three_shows_a_single_global_status_bar() {
    let (rpc, mut incoming) = start(None).await;
    // `laststatus=3`: no per-window status row; a single global bar (the default
    // look: mode, file, ruler) docks at the bottom for the current window.
    let map = redraw_after(&rpc, &mut incoming, ":set laststatus=3<CR>").await;
    assert!(
        !window0_status_visible(&map),
        "mode 3 hides per-window status"
    );
    let text = global_status_text(&map);
    assert!(
        text.contains("NORMAL"),
        "global bar shows the mode: {text:?}"
    );
    assert!(
        text.contains("[No Name]"),
        "global bar shows the file: {text:?}"
    );
    assert!(
        text.trim_end().ends_with("1,1"),
        "global bar ends with the ruler: {text:?}"
    );
}

#[tokio::test]
async fn laststatus_three_global_bar_honors_custom_statusline() {
    let (rpc, mut incoming) = start(None).await;
    // The global bar runs the same `%`-format engine, so a custom 'statusline'
    // (here a `%{v:lua...}` expression) drives it just like a per-window one.
    exec_lua(&rpc, "_G.gtag = function() return 'GLOBAL' end").await;
    feed(&rpc, ":set laststatus=3<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=[%{v:lua.gtag()}]<CR>").await;
    assert_eq!(global_status_text(&map), "[GLOBAL]");
}

#[tokio::test]
async fn laststatus_out_of_range_is_rejected_loudly() {
    let (rpc, mut incoming) = start(None).await;
    // `laststatus` accepts 0..=3; above the range is vim's E474 (loud, not silent),
    // and the option keeps its previous value.
    let map = redraw_after(&rpc, &mut incoming, ":set laststatus=4<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E474"), "out-of-range is loud: {msg:?}");
    assert_eq!(
        exec_lua(&rpc, "return vim.o.laststatus").await.as_u64(),
        Some(2),
        "the value is unchanged after a rejected set"
    );
}

// ----- vim.fn editor-state builtins (statusline / lualine, Phase 5) -----
//
// The `vim.fn.*` surface a real `'statusline'` calls from inside `%{}`/`%!`:
// mode(), line(), col(), winnr(), bufnr(), bufname(), winwidth/height(),
// fnamemodify(), expand(). They read the Rust→Lua mirror the server refreshes
// before evaluating the statusline, so a live redraw reflects the current frame.
// Filename-modifier cases are derived from real neovim's vim.fn.fnamemodify.

#[tokio::test]
async fn statusline_mode_expression_refreshes_live_across_redraws() {
    let (rpc, mut incoming) = start(None).await;
    // A `%{v:lua.vim.fn.mode()}` expression re-evaluates every frame, so the
    // statusline tracks the mode as it changes (the live-refresh guarantee).
    feed(&rpc, ":set statusline=[%{v:lua.vim.fn.mode()}]<CR>");
    let normal = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(status_text(&normal), "[n]", "normal mode short code");
    let insert = redraw_after(&rpc, &mut incoming, "i").await;
    assert_eq!(status_text(&insert), "[i]", "insert mode after entering it");
}

#[tokio::test]
async fn vim_fn_cursor_window_buffer_builtins_read_live_state() {
    let (rpc, _incoming) = start(None).await;
    // Two lines ("abc" / "defgh"); park the cursor on line 1, column 1.
    feed(&rpc, "iabc<CR>defgh<Esc>gg0");
    let out = exec_lua(
        &rpc,
        r#"return table.concat({
            vim.fn.line("."), vim.fn.col("."),   -- cursor row / column (1-based)
            vim.fn.line("$"), vim.fn.col("$"),   -- last line / one past line end
            vim.fn.winnr(), vim.fn.winnr("$"),   -- current window nr / window count
            tostring(vim.fn.bufnr("%") == vim.fn.bufnr("$")), -- the only buffer
          }, "/")"#,
    )
    .await;
    // line 1, col 1; 2 lines; col('$') = #"abc" + 1 = 4; one window; one buffer.
    assert_eq!(out.as_str().unwrap(), "1/1/2/4/1/1/true");
}

#[tokio::test]
async fn vim_fn_fnamemodify_applies_filename_modifiers() {
    let (rpc, _incoming) = start(None).await;
    // Ground truth captured from neovim's vim.fn.fnamemodify (the oracle); each
    // line is one (fname, mods) case.
    let out = exec_lua(
        &rpc,
        r#"local f = vim.fn.fnamemodify
           return table.concat({
             f("/a/b/file.txt", ":h"),       -- /a/b
             f("/a/b/file.txt", ":t"),       -- file.txt
             f("/a/b/file.txt", ":r"),       -- /a/b/file
             f("/a/b/file.txt", ":e"),       -- txt
             f("file.txt", ":h"),            -- .
             f("file.txt", ":t:r"),          -- file
             f("/a/b/c/", ":h"),             -- /a/b/c
             f("dir/file.tar.gz", ":r"),     -- dir/file.tar
             f("dir/file.tar.gz", ":e"),     -- gz
             f("a.b.c.d", ":e:e"),           -- c.d  (consecutive :e widen)
             f("/a/b", ":h:h"),              -- /
           }, "\n")"#,
    )
    .await;
    assert_eq!(
        out.as_str().unwrap(),
        "/a/b\nfile.txt\n/a/b/file\ntxt\n.\nfile\n/a/b/c\ndir/file.tar\ngz\nc.d\n/"
    );
}

#[tokio::test]
async fn vim_fn_fnamemodify_resolves_against_cwd() {
    let (rpc, _incoming) = start(None).await;
    // The cwd-relative modifiers (:p make absolute, :. make relative) are checked
    // against the live getcwd() so the test doesn't bake in a path.
    let out = exec_lua(
        &rpc,
        r#"local cwd = vim.fn.getcwd()
           return table.concat({
             vim.fn.fnamemodify("a/b.txt", ":p") == cwd .. "/a/b.txt" and "p-ok" or "p-bad",
             vim.fn.fnamemodify(cwd .. "/a/b.txt", ":.") == "a/b.txt" and "dot-ok" or "dot-bad",
             vim.fn.fnamemodify("/elsewhere/x", ":.") == "/elsewhere/x" and "abs-ok" or "abs-bad",
           }, "\n")"#,
    )
    .await;
    assert_eq!(out.as_str().unwrap(), "p-ok\ndot-ok\nabs-ok");
}

#[tokio::test]
async fn vim_fn_fnamemodify_p_simplifies_dot_and_dotdot() {
    let (rpc, _incoming) = start(None).await;
    // `:p` lexically simplifies `.`, `..`, and `//` after making the name absolute (vim's
    // `simplify_filename`), so the result is a clean prefix the `:.`/`:~` relativisers can
    // match — a stray `/.` (e.g. from `:p` of `.`) would otherwise survive into every
    // explorer-opened path and defeat the statusline's `:~:.` (the reported bug).
    let out = exec_lua(
        &rpc,
        r#"local cwd = vim.fn.getcwd()
           local f = vim.fn.fnamemodify
           return table.concat({
             f(".", ":p") == cwd and "dot-ok" or ("dot-bad:" .. f(".", ":p")),
             f("a/./b.txt", ":p") == cwd .. "/a/b.txt" and "mid-ok" or ("mid-bad:" .. f("a/./b.txt", ":p")),
             f("a/../b.txt", ":p") == cwd .. "/b.txt" and "up-ok" or ("up-bad:" .. f("a/../b.txt", ":p")),
             f("/x/y/./z", ":p") == "/x/y/z" and "abs-ok" or ("abs-bad:" .. f("/x/y/./z", ":p")),
             -- the round-trip that broke relativisation: `:p` then `:.` is the bare tail.
             f(f(".", ":p") .. "/sample.txt", ":.") == "sample.txt" and "rt-ok" or "rt-bad",
           }, "\n")"#,
    )
    .await;
    assert_eq!(
        out.as_str().unwrap(),
        "dot-ok\nmid-ok\nup-ok\nabs-ok\nrt-ok"
    );
}

#[tokio::test]
async fn vim_fn_fnamemodify_unsupported_modifier_errors_loud() {
    let (rpc, _incoming) = start(None).await;
    // A modifier with no implementation (`:s///`) raises (named), per the
    // no-silent-stub rule, rather than silently returning the name unchanged.
    let err = exec_lua(
        &rpc,
        r"local ok, e = pcall(vim.fn.fnamemodify, 'x', ':s/a/b/')
          return tostring(e)",
    )
    .await;
    let err = err.as_str().expect("a string error from the raise");
    assert!(
        err.contains("fnamemodify") && err.contains(":s"),
        "the error names the unsupported modifier (fail loud): {err:?}"
    );
}

#[tokio::test]
async fn vim_fn_expand_resolves_current_file_modifiers() {
    let path = temp_path("expand_sl");
    std::fs::write(&path, "x\n").unwrap();
    let name = path.to_string_lossy().into_owned();
    let (rpc, _incoming) = start(Some(name.clone())).await;
    // `expand('%')` is the current file; `%:t`/`%:h`/`%:r`/`%:e` route through
    // fnamemodify, so they agree with the path's components.
    let out = exec_lua(
        &rpc,
        r#"return table.concat({
            vim.fn.expand("%"), vim.fn.expand("%:t"),
            vim.fn.expand("%:h"), vim.fn.expand("%:e"),
          }, "\n")"#,
    )
    .await;
    let p = std::path::Path::new(&name);
    let tail = p.file_name().unwrap().to_string_lossy();
    let head = p.parent().unwrap().to_string_lossy();
    assert_eq!(
        out.as_str().unwrap(),
        format!("{name}\n{tail}\n{head}\ntxt")
    );
}
