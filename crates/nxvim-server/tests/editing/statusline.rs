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

/// The shipped `examples/laststatus/` config sources cleanly and actually works
/// end-to-end: it starts in `laststatus=3` (a single global bar driven by its
/// custom `'statusline'`), and the `<leader>2` map it defines switches back to
/// per-window status lines live. Proves the example isn't just "loads".
#[tokio::test]
async fn laststatus_example_config_runs() {
    let dir = temp_dir("laststatus-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/laststatus/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    // It opens in mode 3: a global bar (its custom 'statusline', ending ` ls=3 `),
    // and no per-window status row.
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        !window0_status_visible(&map),
        "example starts global (mode 3)"
    );
    let bar = global_status_text(&map);
    assert!(bar.contains("NORMAL"), "global bar shows the mode: {bar:?}");
    assert!(
        bar.trim_end().ends_with("ls=3"),
        "global bar reads ls=3: {bar:?}"
    );

    // The `<Space>2` leader map flips back to per-window status lines.
    let after = redraw_after(&rpc, &mut incoming, " 2").await;
    assert!(
        window0_status_visible(&after),
        "<leader>2 restores per-window status"
    );
    assert!(
        global_status_segments(&after).is_none(),
        "no global bar in mode 2"
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

/// The shipped `examples/statusline/` config sources cleanly and actually drives
/// the status line (not just "loads"): its `%!v:lua.statusline()` builder runs
/// through the engine and assembles the line from the Phase 5 `vim.fn` surface —
/// the mode block, the file label, and a live cursor ruler all render.
#[tokio::test]
async fn statusline_example_config_runs() {
    let dir = temp_dir("statusline-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/statusline/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    // On the startup No Name buffer: NORMAL mode block (vim.fn.mode), the file
    // label (vim.fn.expand), and the cursor ruler "1:1" (vim.fn.line/col).
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let text = status_text(&map);
    assert!(
        text.contains("NORMAL"),
        "mode block from vim.fn.mode(): {text:?}"
    );
    assert!(
        text.contains("[No Name]"),
        "file label from expand(): {text:?}"
    );
    assert!(
        text.contains("1:1"),
        "live ruler from vim.fn.line/col: {text:?}"
    );
    // The encoding block uses the pure `%{&fileencoding}` expression (no Lua): a
    // fresh buffer is utf-8, with no `[bom]` tag since 'bomb' is off.
    assert!(
        text.contains("utf-8"),
        "encoding block from %{{&fileencoding}}: {text:?}"
    );
    assert!(
        !text.contains("[bom]"),
        "no bom tag when 'bomb' is off: {text:?}"
    );
}
