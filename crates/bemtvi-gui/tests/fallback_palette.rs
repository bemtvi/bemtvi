//! The GUI's built-in colours are the `bemtvi` colorscheme's.
//!
//! The GUI paints with its own colours for any group the active colorscheme leaves
//! unset. That fallback is not a second theme: it is a copy of
//! `runtime/colors/bemtvi.lua`, the editor's default and the one place it comes from.
//! These tests start a real server, load that scheme, and resolve the group each
//! constant (and each `group_fallback` arm) stands in for, so a colour changed in the
//! scheme and not here fails instead of quietly drifting.

use bemtvi_gui::{
    group_fallback, DEFAULT_BG, DEFAULT_ERROR_MSG, DEFAULT_FG, DEFAULT_INCSEARCH_BG, DEFAULT_INLAY,
    DEFAULT_LINE_NR, DEFAULT_SEARCH_BG, DEFAULT_STATUS_BG, DEFAULT_VISUAL_BG,
    DEFAULT_WIN_SEPARATOR, DIAG_ERROR, DIAG_HINT, DIAG_INFO, DIAG_WARN, MULTICURSOR_ACCENT,
};
use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A server with `:colorscheme bemtvi` loaded by the config.
async fn start_bemtvi(tag: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir(tag);
    std::fs::write(dir.join("init.lua"), "vim.cmd.colorscheme('bemtvi')\n")
        .expect("write init.lua");
    let (rpc, incoming) = spawn(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// A group's `attr` (`fg` / `bg` / `sp`) under the loaded scheme, links followed.
async fn group_color(rpc: &Rpc, group: &str, attr: &str) -> Option<u32> {
    let code =
        format!("return vim.api.nvim_get_hl(0, {{ name = '{group}', link = false }}).{attr} or -1");
    exec_lua(rpc, &code)
        .await
        .as_i64()
        .filter(|v| *v >= 0)
        .map(|v| v as u32)
}

#[tokio::test]
async fn the_gui_chrome_fallbacks_are_the_bemtvi_scheme() {
    let (rpc, _incoming) = start_bemtvi("gui_fallback_chrome").await;
    let constants: &[(&str, u32, &str, &str)] = &[
        ("DEFAULT_FG", DEFAULT_FG, "Normal", "fg"),
        ("DEFAULT_BG", DEFAULT_BG, "Normal", "bg"),
        ("DEFAULT_LINE_NR", DEFAULT_LINE_NR, "LineNr", "fg"),
        ("DIAG_ERROR", DIAG_ERROR, "DiagnosticUnderlineError", "sp"),
        ("DIAG_WARN", DIAG_WARN, "DiagnosticUnderlineWarn", "sp"),
        ("DIAG_INFO", DIAG_INFO, "DiagnosticUnderlineInfo", "sp"),
        ("DIAG_HINT", DIAG_HINT, "DiagnosticUnderlineHint", "sp"),
        ("DEFAULT_INLAY", DEFAULT_INLAY, "LspInlayHint", "fg"),
        (
            "DEFAULT_WIN_SEPARATOR",
            DEFAULT_WIN_SEPARATOR,
            "WinSeparator",
            "fg",
        ),
        ("DEFAULT_VISUAL_BG", DEFAULT_VISUAL_BG, "Visual", "bg"),
        ("DEFAULT_SEARCH_BG", DEFAULT_SEARCH_BG, "Search", "bg"),
        (
            "DEFAULT_INCSEARCH_BG",
            DEFAULT_INCSEARCH_BG,
            "IncSearch",
            "bg",
        ),
        ("DEFAULT_ERROR_MSG", DEFAULT_ERROR_MSG, "ErrorMsg", "fg"),
        ("DEFAULT_STATUS_BG", DEFAULT_STATUS_BG, "StatusLine", "bg"),
        ("MULTICURSOR_ACCENT", MULTICURSOR_ACCENT, "CurSearch", "bg"),
    ];
    let mut wrong = Vec::new();
    for (name, value, group, attr) in constants {
        let want = group_color(&rpc, group, attr).await;
        if want != Some(*value) {
            wrong.push(format!(
                "{name} = {value:06x}, bemtvi {group}.{attr} = {want:06x?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "GUI fallbacks differ from the bemtvi scheme:\n{}",
        wrong.join("\n")
    );
}

#[tokio::test]
async fn the_gui_syntax_fallback_is_the_bemtvi_scheme() {
    // Each capture family `group_fallback` colours, against what the scheme resolves
    // that capture to: the colour and the italic / bold it carries.
    let (rpc, _incoming) = start_bemtvi("gui_fallback_syntax").await;
    let captures = [
        "keyword",
        "conditional",
        "repeat",
        "include",
        "exception",
        "function",
        "method",
        "constructor",
        "type",
        "namespace",
        "module",
        "string",
        "character",
        "number",
        "boolean",
        "float",
        "constant",
        "attribute",
        "label",
        "property",
        "field",
        "comment",
        "tag",
        "operator",
        "punctuation",
    ];
    let mut wrong = Vec::new();
    for capture in captures {
        let resolved = rpc
            .request("bemtvi_resolve_capture", vec![Value::from(capture)])
            .await
            .expect("resolve_capture");
        let Value::Map(map) = resolved else {
            wrong.push(format!("{capture}: bemtvi resolves nothing"));
            continue;
        };
        let field = |key: &str| {
            map.iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v)
        };
        let fg = field("fg").and_then(Value::as_u64).map(|v| v as u32);
        let italic = field("italic").and_then(Value::as_bool).unwrap_or(false);
        let bold = field("bold").and_then(Value::as_bool).unwrap_or(false);
        let got = group_fallback(capture);
        if (got.fg, got.italic, got.bold) != (fg, italic, bold) {
            wrong.push(format!(
                "{capture}: gui {:06x?} italic={} bold={}, bemtvi {fg:06x?} italic={italic} bold={bold}",
                got.fg, got.italic, got.bold
            ));
        }
    }
    // `SpecialKey` is a group, not a capture.
    let special = group_fallback("SpecialKey");
    let bold = exec_lua(
        &rpc,
        "return vim.api.nvim_get_hl(0, { name = 'SpecialKey' }).bold == true",
    )
    .await
    .as_bool()
    .unwrap_or(false);
    if special.fg != group_color(&rpc, "SpecialKey", "fg").await || special.bold != bold {
        wrong.push(format!(
            "SpecialKey: gui {:06x?} bold={}",
            special.fg, special.bold
        ));
    }
    assert!(
        wrong.is_empty(),
        "group_fallback differs from the bemtvi scheme:\n{}",
        wrong.join("\n")
    );
}
