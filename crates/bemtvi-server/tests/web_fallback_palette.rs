//! The browser client's built-in colours are the `bemtvi` colorscheme's.
//!
//! The page paints with its own palette until the first frame brings the server's
//! resolved scheme, and keeps it for any group or capture the active scheme leaves
//! undefined. That fallback is not a second theme: it is a copy of
//! `runtime/colors/bemtvi.lua`, the one place the default comes from. These tests
//! read the page's sources and resolve every copied entry through the real scheme,
//! so a colour changed in the scheme and not in the page fails here instead of
//! quietly drifting.

use std::collections::HashSet;
use std::path::PathBuf;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::*;
use rmpv::Value;

fn web_file(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bemtvi-edithost/web")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A server with `:colorscheme bemtvi` loaded by the config.
async fn start_bemtvi(tag: &str) -> (Rpc, tokio::sync::mpsc::UnboundedReceiver<Incoming>) {
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

/// Every `#rrggbb` in `text`, lowercased.
fn hex_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for (i, _) in text.match_indices('#') {
        let hex = &bytes[i + 1..(i + 7).min(bytes.len())];
        let followed = bytes.get(i + 7).copied().unwrap_or(b' ');
        if hex.len() == 6
            && hex.iter().all(u8::is_ascii_hexdigit)
            && !followed.is_ascii_alphanumeric()
        {
            out.push(format!("#{}", String::from_utf8_lossy(hex).to_lowercase()));
        }
    }
    out
}

/// The text between `open` and the next `close` after it.
fn section<'a>(text: &'a str, open: &str, close: &str) -> &'a str {
    let start = text.find(open).unwrap_or_else(|| panic!("no `{open}`")) + open.len();
    let len = text[start..]
        .find(close)
        .unwrap_or_else(|| panic!("no `{close}` after `{open}`"));
    &text[start..start + len]
}

/// `name: '#rrggbb'` / `'dotted.name': '#rrggbb'` entries of a JS object literal body.
fn js_color_entries(body: &str) -> Vec<(String, String)> {
    // Comments go first: one may hold a comma, which would split it into an "entry".
    let code: String = body
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    for entry in code.split(',') {
        let Some((key, value)) = entry.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('\'');
        let value = value.trim().trim_matches('\'');
        if !key.is_empty() && value.starts_with('#') {
            out.push((key.to_string(), value.to_lowercase()));
        }
    }
    out
}

/// The resolved `fg` of a capture under the loaded scheme, as `#rrggbb`.
async fn capture_fg(rpc: &Rpc, capture: &str) -> Option<String> {
    let resolved = rpc
        .request("bemtvi_resolve_capture", vec![Value::from(capture)])
        .await
        .expect("resolve_capture");
    let Value::Map(map) = resolved else {
        return None;
    };
    map.iter()
        .find(|(k, _)| k.as_str() == Some("fg"))
        .and_then(|(_, v)| v.as_u64())
        .map(|fg| format!("#{fg:06x}"))
}

/// A group's `attr` (`fg` / `bg` / `sp`) under the loaded scheme, links followed.
async fn group_color(rpc: &Rpc, group: &str, attr: &str) -> Option<String> {
    let code = format!(
        "local v = vim.api.nvim_get_hl(0, {{ name = '{group}', link = false }}).{attr}\n\
         return v and string.format('#%06x', v) or ''"
    );
    let value = exec_lua(rpc, &code).await;
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// The colours the `bemtvi` scheme is built from: its palette table.
fn bemtvi_palette() -> HashSet<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/colors/bemtvi.lua");
    let scheme = std::fs::read_to_string(path).expect("read bemtvi.lua");
    hex_literals(section(&scheme, "local p = {", "\n}"))
        .into_iter()
        .collect()
}

#[tokio::test]
async fn web_syntax_fallback_is_the_bemtvi_scheme() {
    // `highlight.js`'s `FG` table colours a capture the active scheme doesn't. Every
    // entry must be what `bemtvi` resolves that capture to, and a capture `bemtvi`
    // leaves plain must not be coloured by the page either.
    let (rpc, _incoming) = start_bemtvi("web_fallback_syntax").await;
    let source = web_file("highlight.js");
    let entries = js_color_entries(section(&source, "const FG = {", "\n};"));
    assert!(
        entries.len() > 40,
        "parsed only {} FG entries",
        entries.len()
    );
    let mut wrong = Vec::new();
    for (capture, color) in &entries {
        let want = capture_fg(&rpc, capture).await;
        if want.as_deref() != Some(color.as_str()) {
            wrong.push(format!("{capture}: page {color}, bemtvi {want:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "highlight.js FG differs from the bemtvi scheme:\n{}",
        wrong.join("\n")
    );
}

#[tokio::test]
async fn web_chrome_fallback_is_the_bemtvi_scheme() {
    // The page's `:root` colour variables are the chrome `applyChrome` themes from the
    // scheme: before the first frame they must already be `bemtvi`'s.
    let (rpc, _incoming) = start_bemtvi("web_fallback_chrome").await;
    let page = web_file("index.html");
    let root = section(&page, ":root {", "}");
    // `--var` → the group and attribute it stands for (the `applyChrome` mapping, plus
    // the multi-cursor accent, which is the current-match colour).
    let map: &[(&str, &str, &str)] = &[
        ("--bg", "Normal", "bg"),
        ("--fg", "Normal", "fg"),
        ("--gutter", "LineNr", "fg"),
        ("--gutter-cur", "CursorLineNr", "fg"),
        ("--sel", "Visual", "bg"),
        ("--search", "Search", "bg"),
        ("--status-bg", "StatusLine", "bg"),
        ("--status-fg", "StatusLine", "fg"),
        ("--cursor", "Cursor", "bg"),
        ("--eob", "EndOfBuffer", "fg"),
        ("--accent", "CurSearch", "bg"),
        ("--cursorline", "CursorLine", "bg"),
        ("--colorcolumn", "ColorColumn", "bg"),
    ];
    let mut wrong = Vec::new();
    for decl in root.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim().to_lowercase());
        let Some((_, group, attr)) = map.iter().find(|(var, _, _)| *var == name) else {
            wrong.push(format!("{name}: no group mapped for this variable"));
            continue;
        };
        let want = group_color(&rpc, group, attr).await;
        if want.as_deref() != Some(value.as_str()) {
            wrong.push(format!(
                "{name} ({group}.{attr}): page {value}, bemtvi {want:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "index.html :root differs from the bemtvi scheme:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn every_web_page_color_comes_from_the_bemtvi_palette() {
    // The rest of the page's colours (popup boxes, the boot indicator, status flashes)
    // aren't one group each, but they are still drawn from the scheme's palette — no
    // stray hex from some other theme.
    let palette = bemtvi_palette();
    assert!(
        palette.len() > 20,
        "parsed only {} palette colours",
        palette.len()
    );
    let mut stray = Vec::new();
    for file in ["index.html", "highlight.js"] {
        for hex in hex_literals(&web_file(file)) {
            if !palette.contains(&hex) {
                stray.push(format!("{file}: {hex}"));
            }
        }
    }
    stray.sort();
    stray.dedup();
    assert!(
        stray.is_empty(),
        "colours not in the bemtvi palette:\n{}",
        stray.join("\n")
    );
}
