use crate::support::*;

// ----- highlight registry (Phase 3): nvim_set_hl, links, captures, colorscheme

/// Resolve a highlight group via `nvim_get_hl(0, { name = group })`, returning
/// its concrete-style map (empty when the group is unstyled/absent).
async fn get_hl(rpc: &Rpc, group: &str) -> Vec<(Value, Value)> {
    get_hl_ns(rpc, 0, group).await
}

/// Resolve a highlight group in a specific namespace via
/// `nvim_get_hl(ns, { name = group })` (empty when absent in that namespace).
async fn get_hl_ns(rpc: &Rpc, ns: u64, group: &str) -> Vec<(Value, Value)> {
    let opts = Value::Map(vec![(Value::from("name"), Value::from(group))]);
    let result = rpc
        .request("nvim_get_hl", vec![Value::from(ns), opts])
        .await
        .expect("get_hl");
    match result {
        Value::Map(map) => map,
        _ => Vec::new(),
    }
}

/// Resolve a treesitter capture name through the `@`-group fallback chain;
/// `None` when nothing in the registry matches.
async fn resolve_capture(rpc: &Rpc, capture: &str) -> Option<Vec<(Value, Value)>> {
    let result = rpc
        .request("nxvim_resolve_capture", vec![Value::from(capture)])
        .await
        .expect("resolve_capture");
    match result {
        Value::Map(map) => Some(map),
        _ => None,
    }
}

/// Whether a boolean attribute (`bold`, `italic`, …) is set in a style map.
fn hl_flag(map: &[(Value, Value)], key: &str) -> bool {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_bool())
        .unwrap_or(false)
}

/// WCAG relative luminance (0.0–1.0) of an `0xRRGGBB` color.
fn luminance(rgb: u64) -> f64 {
    let chan = |c: u64| {
        let s = (c & 0xff) as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * chan(rgb >> 16) + 0.7152 * chan(rgb >> 8) + 0.0722 * chan(rgb)
}

/// WCAG contrast ratio (1.0–21.0) between two `0xRRGGBB` colors.
fn contrast(a: u64, b: u64) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = (la.max(lb), la.min(lb));
    (hi + 0.05) / (lo + 0.05)
}

#[tokio::test]
async fn nxvim_scheme_completion_match_is_visible_on_the_selected_row() {
    // Regression: in the built-in `nxvim` scheme the fuzzy-match accent must stay
    // readable on a *selected* completion/picker row. The clients paint the
    // selected row with `PmenuSel`'s bg and overdraw the matched characters with
    // the match accent (the `CmpItemAbbrMatch` → `Special` chain, resolving to
    // `Special` here). If those two colors have near-identical luminance the
    // matched letters vanish into the selection background.
    let dir = temp_dir("nxvim_scheme_match");
    let (rpc, _incoming) = start_with_config(&dir, "vim.cmd.colorscheme('nxvim')\n").await;

    let sel_bg = hl_color(&get_hl(&rpc, "PmenuSel").await, "bg").expect("PmenuSel bg");
    let match_fg = hl_color(&get_hl(&rpc, "Special").await, "fg").expect("match (Special) fg");

    let ratio = contrast(match_fg, sel_bg);
    assert!(
        ratio >= 2.0,
        "matched chars (fg {match_fg:06x}) must stand out from the selected-row bg \
         ({sel_bg:06x}); WCAG contrast was only {ratio:.2}"
    );
}

#[tokio::test]
async fn nvim_set_hl_stores_resolved_colors_and_attrs() {
    // catppuccin-mocha-ish: Normal carries fg+bg, Comment fg+italic. The
    // registry stores them and nvim_get_hl reads them back as RGB ints + flags.
    let dir = temp_dir("hlset");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
         vim.api.nvim_set_hl(0, 'Comment', { fg = '#6c7086', italic = true })\n",
    )
    .await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
    let comment = get_hl(&rpc, "Comment").await;
    assert_eq!(hl_color(&comment, "fg"), Some(hex("6c7086")));
    assert!(hl_flag(&comment, "italic"), "Comment should be italic");
}

#[tokio::test]
async fn nonzero_namespace_set_hl_does_not_clobber_global() {
    // `nvim_set_hl(ns, name, opts)` with `ns != 0` defines the group *in that
    // namespace* — it must not fold into / overwrite the global (ns 0) table.
    // A plugin that sets `Normal` in its own namespace must leave the
    // colorscheme's global `Normal` intact, and the namespaced definition must
    // be readable back via `nvim_get_hl(ns, ...)`.
    let dir = temp_dir("hlns");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
         vim.api.nvim_set_hl(0, 'Comment', { fg = '#6c7086' })\n\
         vim.api.nvim_set_hl(5, 'Normal', { fg = '#f38ba8', bg = '#11111b' })\n",
    )
    .await;
    // The global Normal is untouched by the ns-5 write.
    let global = get_hl_ns(&rpc, 0, "Normal").await;
    assert_eq!(hl_color(&global, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&global, "bg"), Some(hex("1e1e2e")));
    // Namespace 5 carries its own Normal.
    let ns5 = get_hl_ns(&rpc, 5, "Normal").await;
    assert_eq!(hl_color(&ns5, "fg"), Some(hex("f38ba8")));
    assert_eq!(hl_color(&ns5, "bg"), Some(hex("11111b")));
    // A group defined only in the global table is *not* visible from ns 5: a
    // namespace read returns that namespace's own table (neovim's render-time
    // fallback to the global table is a separate mechanism).
    assert!(
        get_hl_ns(&rpc, 5, "Comment").await.is_empty(),
        "an undefined group in ns 5 reads empty, not the global def"
    );
}

#[tokio::test]
async fn nonzero_namespace_visible_to_lua_get_hl() {
    // The Lua-side `vim.api.nvim_get_hl(ns, ...)` (backed by the Rust→Lua
    // mirror) must also be namespace-aware: reading ns 0 sees the global
    // `Normal`, reading ns 5 sees the namespaced one, with no cross-contamination.
    let dir = temp_dir("hlnslua");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4' })\n\
         vim.api.nvim_set_hl(5, 'Normal', { fg = '#f38ba8' })\n",
    )
    .await;
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'Normal' }).fg"
        )
        .await
        .as_u64(),
        Some(hex("cdd6f4")),
        "Lua nvim_get_hl(0, ...) reads the global Normal"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(5, { name = 'Normal' }).fg"
        )
        .await
        .as_u64(),
        Some(hex("f38ba8")),
        "Lua nvim_get_hl(5, ...) reads the namespaced Normal"
    );
}

#[tokio::test]
async fn nvim_get_hl_follows_links_to_the_target_color() {
    // `@keyword` is a pure link to `Keyword`; resolving it must yield Keyword's
    // concrete color and attributes, not an empty alias.
    let dir = temp_dir("hllink");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Keyword', { fg = '#cba6f7', bold = true })\n\
         vim.api.nvim_set_hl(0, '@keyword', { link = 'Keyword' })\n",
    )
    .await;
    let kw = get_hl(&rpc, "@keyword").await;
    assert_eq!(hl_color(&kw, "fg"), Some(hex("cba6f7")));
    assert!(
        hl_flag(&kw, "bold"),
        "linked group inherits the target's bold"
    );
}

#[tokio::test]
async fn capture_resolves_through_the_group_fallback_chain() {
    // Only the broad groups are themed; specific captures must fall through to
    // them. `string` -> String (green); `function.call` -> @function.call ->
    // @function -> Function (blue); an unknown capture resolves to nothing.
    let dir = temp_dir("capfb");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'String', { fg = '#a6e3a1' })\n\
         vim.api.nvim_set_hl(0, 'Function', { fg = '#89b4fa' })\n",
    )
    .await;
    let string = resolve_capture(&rpc, "string")
        .await
        .expect("string resolves");
    assert_eq!(hl_color(&string, "fg"), Some(hex("a6e3a1")));
    let call = resolve_capture(&rpc, "function.call")
        .await
        .expect("function.call resolves via fallback");
    assert_eq!(hl_color(&call, "fg"), Some(hex("89b4fa")));
    assert!(
        resolve_capture(&rpc, "frobnicate").await.is_none(),
        "an unknown capture has no resolved style"
    );
}

#[tokio::test]
async fn colorscheme_sources_the_file_and_fires_the_autocmd() {
    // `:colorscheme cat` must source colors/cat.lua (populating the registry)
    // and fire the ColorScheme autocmd registered in init.lua.
    let dir = temp_dir("colo");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('ColorScheme', \
           { pattern = 'cat', callback = function(o) print('themed:' .. o.match) end })\n",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme cat<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("themed:cat"),
        "the ColorScheme autocmd should fire with the scheme name"
    );
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
}

#[tokio::test]
async fn init_lua_colorscheme_themes_the_first_frame() {
    // A colorscheme loaded from init.lua must be in effect before the first
    // frame is served — so the startup redraw already carries resolved chrome,
    // not bare defaults. (The real-plugin version of this is the Tier-3 PTY
    // test `catppuccin_repaints_the_editor_in_truecolor`.)
    let dir = temp_dir("startup_theme");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    // The startup frame's `chrome.normal` indexes a `styles` entry carrying
    // catppuccin's base background — i.e. the theme painted the very first frame.
    let map = redraw_after(&rpc, &mut incoming, "").await;
    let normal_id = field(&map, "chrome")
        .and_then(|c| chrome_id(c, "normal"))
        .expect("Normal resolved in the startup frame's chrome");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let normal = match &styles[normal_id] {
        Value::Map(m) => m.as_slice(),
        _ => panic!("style entry is not a map"),
    };
    assert_eq!(hl_color(normal, "bg"), Some(hex("1e1e2e")));
    assert_eq!(hl_color(normal, "fg"), Some(hex("cdd6f4")));
}

#[tokio::test]
async fn float_chrome_groups_resolve_into_the_frame_chrome() {
    // The float highlight groups (FloatBorder / NormalFloat / FloatTitle) a
    // colorscheme defines must reach the client as resolved chrome styles, the
    // same way Normal does — otherwise clients fall back to a bare default and
    // float borders stay uncolored regardless of the theme.
    let dir = temp_dir("float_chrome");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'FloatBorder', { fg = '#89b4fa', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'NormalFloat', { fg = '#cdd6f4', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'FloatTitle',  { fg = '#cba6f7', bg = '#181825' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    let map = redraw_after(&rpc, &mut incoming, "").await;
    let chrome = field(&map, "chrome").expect("chrome map");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let style_of = |key: &str| -> &[(Value, Value)] {
        let id = chrome_id(chrome, key).unwrap_or_else(|| panic!("{key} resolved in chrome"));
        match &styles[id] {
            Value::Map(m) => m.as_slice(),
            _ => panic!("style entry is not a map"),
        }
    };
    assert_eq!(
        hl_color(style_of("float_border"), "fg"),
        Some(hex("89b4fa"))
    );
    assert_eq!(
        hl_color(style_of("normal_float"), "bg"),
        Some(hex("181825"))
    );
    assert_eq!(hl_color(style_of("float_title"), "fg"), Some(hex("cba6f7")));
}

#[tokio::test]
async fn tabline_chrome_groups_resolve_into_the_frame_chrome() {
    // The tabline highlight groups (TabLine / TabLineSel / TabLineFill) a
    // colorscheme defines must reach the client as resolved chrome styles, the
    // same way StatusLine does — otherwise the built-in tabline falls back to the
    // status-line colors (or a bare default) and stays unthemed regardless of the
    // theme.
    let dir = temp_dir("tabline_chrome");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'TabLine',     { fg = '#a6adc8', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'TabLineSel',  { fg = '#1e1e2e', bg = '#cba6f7' })\n\
         vim.api.nvim_set_hl(0, 'TabLineFill', { fg = '#6c7086', bg = '#11111b' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    let map = redraw_after(&rpc, &mut incoming, "").await;
    let chrome = field(&map, "chrome").expect("chrome map");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let style_of = |key: &str| -> &[(Value, Value)] {
        let id = chrome_id(chrome, key).unwrap_or_else(|| panic!("{key} resolved in chrome"));
        match &styles[id] {
            Value::Map(m) => m.as_slice(),
            _ => panic!("style entry is not a map"),
        }
    };
    assert_eq!(hl_color(style_of("tabline"), "bg"), Some(hex("181825")));
    assert_eq!(hl_color(style_of("tabline_sel"), "bg"), Some(hex("cba6f7")));
    assert_eq!(
        hl_color(style_of("tabline_fill"), "bg"),
        Some(hex("11111b"))
    );
}

#[tokio::test]
async fn msg_area_chrome_group_resolves_into_the_frame_chrome() {
    // The command-line / message row used to carry no dedicated chrome group, so a
    // colorscheme (e.g. catppuccin) could not theme it — the row stayed the
    // terminal default. `MsgArea` now bridges to the client as `chrome.msg_area`
    // so the cmdline picks up the theme (issue: "the cmd line is not being themed").
    let dir = temp_dir("msgarea_chrome");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal',  { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
         vim.api.nvim_set_hl(0, 'MsgArea', { fg = '#cdd6f4', bg = '#181825' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    let map = redraw_after(&rpc, &mut incoming, "").await;
    let chrome = field(&map, "chrome").expect("chrome map");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let id = chrome_id(chrome, "msg_area").expect("MsgArea resolved as chrome.msg_area");
    let style = match &styles[id] {
        Value::Map(m) => m.as_slice(),
        _ => panic!("style entry is not a map"),
    };
    assert_eq!(hl_color(style, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(style, "bg"), Some(hex("181825")));
}

#[tokio::test]
async fn win_separator_chrome_group_resolves_into_the_frame_chrome() {
    // Split / dock separators used to reuse the (often bright) StatusLine look.
    // `WinSeparator` now bridges to the client as `chrome.win_separator`, so a
    // theme's dim separator colour (e.g. catppuccin's near-background `crust`)
    // reaches the renderer (issue: "the tui split/dock separators are too bright").
    let dir = temp_dir("winsep_chrome");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'StatusLine',   { fg = '#cdd6f4', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'WinSeparator', { fg = '#11111b' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    let map = redraw_after(&rpc, &mut incoming, "").await;
    let chrome = field(&map, "chrome").expect("chrome map");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let id =
        chrome_id(chrome, "win_separator").expect("WinSeparator resolved as chrome.win_separator");
    let style = match &styles[id] {
        Value::Map(m) => m.as_slice(),
        _ => panic!("style entry is not a map"),
    };
    assert_eq!(
        hl_color(style, "fg"),
        Some(hex("11111b")),
        "the separator carries WinSeparator's dim foreground, not the status-line bg"
    );
}

/// Resolve `chrome[key]`'s `fg` through the redraw's `styles` palette (`None` when
/// the group is undefined for this frame).
fn chrome_fg(map: &[(Value, Value)], key: &str) -> Option<u64> {
    let chrome = field(map, "chrome")?;
    let id = chrome_id(chrome, key)?;
    let styles = field(map, "styles").and_then(Value::as_array)?;
    match styles.get(id)? {
        Value::Map(style) => hl_color(style, "fg"),
        _ => None,
    }
}

#[tokio::test]
async fn colorscheme_switch_repaints_win_separator_after_a_split() {
    // Repro: open under a dark theme, split so a separator exists, then switch to a
    // light theme. The redraw after the switch must carry the NEW WinSeparator, not
    // the stale dark one — else the split/dock separators keep the old theme's
    // colour (a dark line on a light background).
    let dir = temp_dir("winsep_switch");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("dark.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
         vim.api.nvim_set_hl(0, 'WinSeparator', { fg = '#11111b' })\n",
    )
    .expect("write dark");
    std::fs::write(
        dir.join("colors").join("light.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#4c4f69', bg = '#eff1f5' })\n\
         vim.api.nvim_set_hl(0, 'WinSeparator', { fg = '#dce0e8' })\n",
    )
    .expect("write light");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('dark')\n").await;

    // A vertical split so the layout carries a separator.
    let map = redraw_after(&rpc, &mut incoming, ":vsplit<CR>").await;
    assert!(
        field(&map, "separators")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the vsplit produced at least one separator"
    );
    assert_eq!(
        chrome_fg(&map, "win_separator"),
        Some(hex("11111b")),
        "under the dark theme the separator is dark crust"
    );

    // Switch to the light theme; the next frame's win_separator must update.
    let map2 = redraw_after(&rpc, &mut incoming, ":colorscheme light<CR>").await;
    assert_eq!(
        chrome_fg(&map2, "win_separator"),
        Some(hex("dce0e8")),
        "after switching themes the separator repaints to the light theme's WinSeparator"
    );
    // …and the frame must STILL carry the separators — a redraw that drops them
    // makes a client paint none, leaving the stale (dark) cells from the prior frame.
    assert!(
        field(&map2, "separators")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the colorscheme-switch frame still carries the split separators"
    );
}

/// The `style_id` a redraw's `chrome` map assigns to region `key`, if resolved.
fn chrome_id(chrome: &Value, key: &str) -> Option<usize> {
    match chrome {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_u64())
            .map(|n| n as usize),
        _ => None,
    }
}

#[tokio::test]
async fn colorscheme_missing_file_reports_e185() {
    let dir = temp_dir("colomiss");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme nope<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E185: Cannot find color scheme 'nope'"),
        "a colorscheme with no file on the runtimepath is an error"
    );
}

#[tokio::test]
async fn builtin_nxvim_colorscheme_loads_with_no_runtime_file() {
    // `:colorscheme nxvim` must work with an empty config dir — the scheme is
    // bundled in the binary, not sourced off the runtimepath. It populates the
    // registry (One Dark palette) and fires the ColorScheme autocmd like any
    // file-backed scheme.
    let dir = temp_dir("builtin_nxvim");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('ColorScheme', \
           { pattern = 'nxvim', callback = function(o) print('themed:' .. o.match) end })\n",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme nxvim<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("themed:nxvim"),
        "the bundled scheme should fire the ColorScheme autocmd"
    );
    // Editor chrome (One Dark base) and a couple of syntax groups resolve.
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("abb2bf")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("282c34")));
    let comment = get_hl(&rpc, "Comment").await;
    assert_eq!(hl_color(&comment, "fg"), Some(hex("5c6370")));
    assert!(hl_flag(&comment, "italic"), "Comment is italic in One Dark");
    assert_eq!(
        hl_color(&get_hl(&rpc, "Keyword").await, "fg"),
        Some(hex("c678dd"))
    );
    assert_eq!(
        hl_color(&get_hl(&rpc, "String").await, "fg"),
        Some(hex("98c379"))
    );
    // The treesitter capture chain resolves through the bundled legacy groups.
    let func = resolve_capture(&rpc, "function.call")
        .await
        .expect("@function.call resolves under the bundled scheme");
    assert_eq!(hl_color(&func, "fg"), Some(hex("61afef")));
}

#[tokio::test]
async fn builtin_nxvim_colorscheme_shows_eob_tildes() {
    // The `~` end-of-buffer fillers must stay visible under the bundled scheme:
    // `EndOfBuffer` is highlighted like `NonText` (vim's default), so its fg is
    // the gutter colour, *not* the Normal background (which would hide them).
    let dir = temp_dir("eob_nxvim");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let _ = redraw_after(&rpc, &mut incoming, ":colorscheme nxvim<CR>").await;
    let eob = get_hl(&rpc, "EndOfBuffer").await;
    assert_eq!(
        hl_color(&eob, "fg"),
        Some(hex("4b5263")),
        "EndOfBuffer should be the gutter colour (like NonText), so `~` fillers are visible"
    );
    assert_ne!(
        hl_color(&eob, "fg"),
        Some(hex("282c34")),
        "EndOfBuffer fg must not equal the Normal background, or the `~` fillers vanish"
    );
}

#[tokio::test]
async fn builtin_nxvim_colorscheme_themes_the_tabline() {
    // The bundled scheme themed `StatusLine` but not the tabline groups, so the
    // tabline row fell back to the terminal default with a reverse-video active
    // cell — visibly unthemed against the rest of the frame in the TUI. The bar
    // now carries its own One Dark look: the active tab reads as the editor
    // background (a real "front" tab), inactive tabs and the fill sit on the
    // darker chrome background the status line uses.
    let dir = temp_dir("tabline_nxvim");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let _ = redraw_after(&rpc, &mut incoming, ":colorscheme nxvim<CR>").await;

    let tabline = get_hl(&rpc, "TabLine").await;
    assert_eq!(hl_color(&tabline, "fg"), Some(hex("5c6370")));
    assert_eq!(hl_color(&tabline, "bg"), Some(hex("21252b")));
    let sel = get_hl(&rpc, "TabLineSel").await;
    assert_eq!(hl_color(&sel, "fg"), Some(hex("abb2bf")));
    assert_eq!(
        hl_color(&sel, "bg"),
        Some(hex("282c34")),
        "the active tab sits on the Normal background, so it reads as the front tab"
    );
    let fill = get_hl(&rpc, "TabLineFill").await;
    assert_eq!(hl_color(&fill, "bg"), Some(hex("21252b")));
    assert_ne!(
        hl_color(&sel, "bg"),
        hl_color(&tabline, "bg"),
        "the active cell must be distinguishable from the inactive ones"
    );
}

#[tokio::test]
async fn truecolor_attach_defaults_in_the_nxvim_colorscheme() {
    // A client that declares truecolor support (`truecolor = true` in the attach
    // capabilities) with no config-chosen scheme lands on the bundled `nxvim` One
    // Dark palette automatically — a rich terminal reaches real colors with zero
    // config. Nothing was typed; the default is applied at attach.
    let dir = temp_dir("truecolor_default");
    std::fs::write(dir.join("init.lua"), "").expect("write init.lua");
    let (rpc, _incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    });
    attach_truecolor(&rpc, 80, 25).await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(
        hl_color(&normal, "fg"),
        Some(hex("abb2bf")),
        "a truecolor attach defaults in the nxvim scheme's Normal fg"
    );
    assert_eq!(hl_color(&normal, "bg"), Some(hex("282c34")));
    // `g:colors_name` records the auto-loaded scheme, so a re-attach won't reapply
    // and tooling (statusline, etc.) sees the active theme.
    assert_eq!(
        exec_lua(&rpc, "return vim.g.colors_name").await.as_str(),
        Some("nxvim")
    );
}

#[tokio::test]
async fn non_truecolor_attach_leaves_the_registry_empty() {
    // A legacy / 256-color terminal (no `truecolor` capability) keeps its own tuned
    // palette — the registry stays empty and no scheme is defaulted in. This is the
    // plain `attach` the default harness setup uses.
    let dir = temp_dir("no_truecolor_default");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    assert!(
        get_hl(&rpc, "Normal").await.is_empty(),
        "no colorscheme is defaulted in on a non-truecolor attach"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.g.colors_name").await,
        Value::Nil,
        "g:colors_name stays unset with no scheme loaded"
    );
}

#[tokio::test]
async fn truecolor_attach_respects_a_config_chosen_colorscheme() {
    // If the user's config picked a scheme, the truecolor default must not clobber
    // it: `init.lua` runs before attach, so `g:colors_name` is already set and the
    // auto-default is skipped. The user's `cat` wins, not the bundled `nxvim`.
    let dir = temp_dir("truecolor_respects_config");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n",
    )
    .expect("write colorscheme");
    std::fs::write(dir.join("init.lua"), "vim.cmd.colorscheme('cat')\n").expect("write init.lua");
    let (rpc, _incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    });
    attach_truecolor(&rpc, 80, 25).await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(
        hl_color(&normal, "bg"),
        Some(hex("1e1e2e")),
        "the config-chosen scheme wins over the truecolor default"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.g.colors_name").await.as_str(),
        Some("cat")
    );
}

#[tokio::test]
async fn user_colors_file_overrides_the_builtin_scheme() {
    // A `colors/nxvim.lua` on the runtimepath shadows the bundled scheme — the
    // runtimepath is searched first, the built-in is only the fallback.
    let dir = temp_dir("override_nxvim");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("nxvim.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#ffffff', bg = '#000000' })\n",
    )
    .expect("write override colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let _ = redraw_after(&rpc, &mut incoming, ":colorscheme nxvim<CR>").await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(
        hl_color(&normal, "bg"),
        Some(hex("000000")),
        "a user colors/nxvim.lua must win over the bundled scheme"
    );
}

#[tokio::test]
async fn hi_clear_empties_the_registry() {
    let dir = temp_dir("hiclear");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4' })\n",
    )
    .await;
    assert_eq!(
        hl_color(&get_hl(&rpc, "Normal").await, "fg"),
        Some(hex("cdd6f4"))
    );
    let _ = redraw_after(&rpc, &mut incoming, ":hi clear<CR>").await;
    assert!(
        get_hl(&rpc, "Normal").await.is_empty(),
        ":hi clear should empty the registry back to defaults"
    );
}

// ----- compile step (Phase 4): bytecode round-trip + on-disk cache -----------

/// Install a colorscheme fixture that exercises catppuccin's real compile
/// mechanics under `dir`: its `load()` serializes a highlight table to Lua
/// source, `loadstring`s it, `string.dump(fn, true)`s the result to bytecode,
/// writes that to `<compile_path>/<flavour>` via `io.open(..., "wb")`, then on
/// load `loadfile`s the cached bytecode and runs it (firing `nvim_set_hl`). A
/// `vim.g._compiles` counter makes cache reuse observable. This mirrors the real
/// plugin's `lib/compiler.lua` + `init.lua` load path; the actual catppuccin
/// checkout is wired up in Phase 6. `compile_path` is a subdir of `dir` so the
/// test can assert the cache file without touching `~/.cache`.
fn write_compiler_fixture(dir: &std::path::Path) {
    let module = dir.join("lua").join("compilescheme");
    std::fs::create_dir_all(&module).expect("create module dir");
    let compile_path = dir.join("cache");
    // The compile step writes bytecode into this dir with `io.open(..., "wb")`,
    // which won't create parents; the real plugin would `nx.fs.mkdir` it (async,
    // off-tick) but the synchronous compile-on-load path can't await, so the
    // harness creates the cache dir up front (it isn't what this test exercises).
    std::fs::create_dir_all(&compile_path).expect("create cache dir");
    std::fs::write(
        module.join("init.lua"),
        format!(
            "local M = {{ options = {{ compile_path = {path:?}, flavour = 'mocha' }} }}\n\
             local sep = package.config:sub(1, 1)\n\
             local function inspect(t)\n\
               local list = {{}}\n\
               for k, v in pairs(t) do\n\
                 if type(v) == 'string' then\n\
                   list[#list + 1] = string.format('%s = \"%s\"', k, v)\n\
                 else\n\
                   list[#list + 1] = string.format('%s = %s', k, tostring(v))\n\
                 end\n\
               end\n\
               return '{{ ' .. table.concat(list, ', ') .. ' }}'\n\
             end\n\
             local function compile(flavour)\n\
               vim.g._compiles = (vim.g._compiles or 0) + 1\n\
               local theme = {{\n\
                 Normal = {{ fg = '#cdd6f4', bg = '#1e1e2e' }},\n\
                 Comment = {{ fg = '#6c7086', italic = true }},\n\
                 Keyword = {{ fg = '#cba6f7' }},\n\
                 ['@keyword'] = {{ link = 'Keyword' }},\n\
               }}\n\
               local lines = {{\n\
                 'return string.dump(function(flavour)\\n'\n\
                 .. 'vim.o.termguicolors = true\\n'\n\
                 .. 'vim.g.colors_name = \"compilescheme-' .. flavour .. '\"\\n'\n\
                 .. 'local h = vim.api.nvim_set_hl',\n\
               }}\n\
               for group, color in pairs(theme) do\n\
                 lines[#lines + 1] = string.format('h(0, \"%s\", %s)', group, inspect(color))\n\
               end\n\
               lines[#lines + 1] = 'end, true)'\n\
               local f = assert(loadstring(table.concat(lines, '\\n')), 'compile failed')\n\
               local file = assert(io.open(M.options.compile_path .. sep .. flavour, 'wb'))\n\
               file:write(f())\n\
               file:close()\n\
             end\n\
             function M.setup(conf) M.options = vim.tbl_deep_extend('force', M.options, conf or {{}}) end\n\
             function M.load(flavour)\n\
               flavour = flavour or M.options.flavour\n\
               local compiled = M.options.compile_path .. sep .. flavour\n\
               local f = loadfile(compiled)\n\
               if not f then\n\
                 compile(flavour)\n\
                 f = assert(loadfile(compiled), 'could not load cache')\n\
               end\n\
               f(flavour)\n\
               print('compiles=' .. tostring(vim.g._compiles or 0))\n\
             end\n\
             return M\n",
            path = compile_path.to_string_lossy(),
        ),
    )
    .expect("write module");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("compilescheme.lua"),
        "require('compilescheme').load()\n",
    )
    .expect("write colors file");
}

#[tokio::test]
async fn colorscheme_compiles_to_bytecode_then_reuses_the_cache() {
    // Strategy A end-to-end: the first `:colorscheme` compiles (serialize ->
    // loadstring -> string.dump -> io.write), loads the cached bytecode via
    // loadfile, and runs it to populate the registry. The second reuses the
    // on-disk cache without recompiling (the compile counter stays at 1).
    let dir = temp_dir("compile");
    write_compiler_fixture(&dir);
    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    })
    .await;

    // First load: no cache yet, so it compiles exactly once.
    let first = redraw_after(&rpc, &mut incoming, ":colorscheme compilescheme<CR>").await;
    assert_eq!(
        field(&first, "message").and_then(Value::as_str),
        Some("compiles=1"),
        "first colorscheme load should compile once"
    );

    // The bytecode cache file was written to disk.
    assert!(
        dir.join("cache").join("mocha").is_file(),
        "the compiled flavour should be cached on disk"
    );

    // The registry is populated through the real bytecode load path.
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
    assert!(hl_flag(&get_hl(&rpc, "Comment").await, "italic"));
    assert_eq!(
        hl_color(&get_hl(&rpc, "@keyword").await, "fg"),
        Some(hex("cba6f7")),
        "the linked @keyword resolves through the compiled table"
    );

    // Second load: the cache exists, so loadfile succeeds and no recompile
    // happens — the counter is still 1.
    let second = redraw_after(&rpc, &mut incoming, ":colorscheme compilescheme<CR>").await;
    assert_eq!(
        field(&second, "message").and_then(Value::as_str),
        Some("compiles=1"),
        "second load should reuse the cached bytecode, not recompile"
    );
}
