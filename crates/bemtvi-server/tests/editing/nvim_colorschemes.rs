use crate::support::*;

// ----- neovim's bundled colorschemes, ported (scripts/gen-nvim-colorschemes.py)

/// Every scheme neovim bundles in `runtime/colors/`, `vim.lua` included, and
/// neovim's own `default.vim` under the name `neovim`.
const PORTED: &[&str] = &[
    "blue",
    "catppuccin",
    "darkblue",
    "delek",
    "desert",
    "elflord",
    "evening",
    "habamax",
    "industry",
    "koehler",
    "lunaperche",
    "morning",
    "murphy",
    "neovim",
    "pablo",
    "peachpuff",
    "quiet",
    "retrobox",
    "ron",
    "shine",
    "slate",
    "sorbet",
    "torte",
    "unokai",
    "vim",
    "wildcharm",
    "zaibatsu",
    "zellner",
];

/// `nvim_get_hl(0, { name = group })`: the group's resolved style, links followed.
async fn get_hl(rpc: &Rpc, group: &str) -> Vec<(Value, Value)> {
    let opts = Value::Map(vec![(Value::from("name"), Value::from(group))]);
    match rpc
        .request("nvim_get_hl", vec![Value::from(0u64), opts])
        .await
        .expect("get_hl")
    {
        Value::Map(map) => map,
        _ => Vec::new(),
    }
}

async fn lua_str(rpc: &Rpc, chunk: &str) -> String {
    exec_lua(rpc, chunk)
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn every_ported_neovim_scheme_loads_with_no_runtime_file() {
    let dir = temp_dir("ported_schemes");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    for scheme in PORTED {
        let map = redraw_after(&rpc, &mut incoming, &format!(":colorscheme {scheme}<CR>")).await;
        let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
        assert!(
            !msg.starts_with('E'),
            ":colorscheme {scheme} must load from the binary, got: {msg}"
        );
        assert_eq!(
            lua_str(&rpc, "return btv.g.colors_name").await,
            *scheme,
            "the scheme names itself"
        );
        // Each one is a full table — the scheme, vim.lua's base and neovim's
        // defaults — not a handful of groups.
        let count = exec_lua(
            &rpc,
            "local n = 0 for _ in pairs(btv.hl.get(0, {})) do n = n + 1 end return n",
        )
        .await
        .as_u64()
        .unwrap_or(0);
        assert!(count >= 200, "{scheme} defines only {count} groups");
    }
}

#[tokio::test]
async fn a_ported_scheme_paints_its_own_palette() {
    let dir = temp_dir("ported_habamax");
    let (rpc, _incoming) = start_with_config(&dir, "vim.cmd.colorscheme('habamax')\n").await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("c7c7c7")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1c1c1c")));
    assert_eq!(
        hl_color(&get_hl(&rpc, "Comment").await, "fg"),
        Some(hex("767676"))
    );
    // A link of the scheme's own (`Function` → `Identifier`) resolves.
    assert_eq!(
        hl_color(&get_hl(&rpc, "Function").await, "fg"),
        Some(hex("87afaf"))
    );
}

#[tokio::test]
async fn a_scheme_that_sets_background_sets_it() {
    // morning is a light scheme and says so, even from a dark start.
    let dir = temp_dir("ported_morning");
    let (rpc, _incoming) = start_with_config(&dir, "vim.cmd.colorscheme('morning')\n").await;
    assert_eq!(lua_str(&rpc, "return btv.o.background").await, "light");
    assert_eq!(
        hl_color(&get_hl(&rpc, "Normal").await, "bg"),
        Some(hex("e4e4e4"))
    );
}

#[tokio::test]
async fn a_scheme_that_follows_background_reloads_into_its_light_variant() {
    // neovim's own theme (its `default`, bemtvi's `neovim`) has both variants;
    // `:set background=light` reloads it, as in vim, and the name stays `neovim`.
    let dir = temp_dir("ported_neovim_bg");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('neovim')\n").await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("e0e2ea")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("14161b")));

    let _ = redraw_after(&rpc, &mut incoming, ":set background=light<CR>").await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("14161b")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("e0e2ea")));
    assert_eq!(lua_str(&rpc, "return btv.g.colors_name").await, "neovim");
}

#[tokio::test]
async fn a_link_spelled_in_another_case_reaches_its_target() {
    // retrobox writes `hi! link MessageWindow PMenu`; neovim's group names are
    // case-insensitive and bemtvi's are not, so the port points it at `Pmenu`.
    let dir = temp_dir("ported_link_case");
    let (rpc, _incoming) = start_with_config(&dir, "vim.cmd.colorscheme('retrobox')\n").await;
    let pmenu = hl_color(&get_hl(&rpc, "Pmenu").await, "bg").expect("Pmenu bg");
    assert_eq!(pmenu, hex("3c3836"));
    assert_eq!(
        hl_color(&get_hl(&rpc, "MessageWindow").await, "bg"),
        Some(pmenu)
    );
}

#[tokio::test]
async fn default_is_an_alias_for_bemtvi() {
    // `:colorscheme default` is bemtvi's own scheme, the one every client starts
    // on, and it loads under that scheme's name.
    let dir = temp_dir("default_alias");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('neovim')\n").await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme default<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(!msg.starts_with('E'), ":colorscheme default failed: {msg}");
    assert_eq!(lua_str(&rpc, "return btv.g.colors_name").await, "bemtvi");
    assert_eq!(
        hl_color(&get_hl(&rpc, "Normal").await, "bg"),
        Some(hex("1e1e24"))
    );
    // `:colorscheme <Tab>` offers the alias beside the schemes.
    assert_eq!(
        exec_lua(
            &rpc,
            "for _, n in ipairs(btv._builtin_colorschemes) do \
               if n == 'default' then return true end \
             end \
             return false",
        )
        .await
        .as_bool(),
        Some(true)
    );
}

#[tokio::test]
async fn a_user_colors_default_file_shadows_the_alias() {
    let dir = temp_dir("default_alias_shadow");
    std::fs::create_dir_all(dir.join("colors")).unwrap();
    std::fs::write(
        dir.join("colors").join("default.lua"),
        "btv.hl.define(0, 'Normal', { bg = '#123456' })\n",
    )
    .unwrap();
    let (rpc, _incoming) = start_with_config(&dir, "vim.cmd.colorscheme('default')\n").await;
    assert_eq!(lua_str(&rpc, "return btv.g.colors_name").await, "default");
    assert_eq!(
        hl_color(&get_hl(&rpc, "Normal").await, "bg"),
        Some(hex("123456"))
    );
}
