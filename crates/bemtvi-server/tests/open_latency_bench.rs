//! Open-latency benchmark: how long from `:edit`ing a file to (a) its content
//! first painting, and (b) its syntax highlighting arriving.
//!
//! This is a **measurement harness, not an assertion suite** — it's `#[ignore]`d so
//! `cargo test` never runs it, and it prints a table rather than asserting on wall
//! times (which are machine- and load-dependent). Keep it for spot-checking the
//! open path after any change to the treesitter / redraw / grammar-load code.
//!
//! It exists because the first highlight of a buffer pays a large one-time cost —
//! dlopen'ing the language grammar and compiling every `.scm` query (tens of ms for
//! a big grammar like Python), plus the initial full-buffer parse. That work is
//! deferred off the first-paint frame (see `first_highlight_deferred` in the
//! server) so a freshly-opened file appears instantly and colours in a beat later;
//! this harness is how you confirm that split still holds. The tell of a regression
//! is the **cold** row's `first-paint` climbing back up toward its `highlight`
//! column — that means grammar-load has moved back onto the first-paint path.
//!
//! Needs the Python grammar installed (skips cleanly otherwise, like the LSP
//! tests). Run with:
//!   BEMTVI_DATA_DIR=$HOME/.local/share/bemtvi \
//!     cargo test -p bemtvi-server --test open_latency_bench -- --ignored --nocapture
use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{feed, start_attached, temp_dir, window0_field};
use rmpv::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// A Python source of roughly `nlines` lines with enough structure (classes,
/// comments, string/number literals, operators) to give the highlighter real work.
fn python_source(nlines: usize) -> String {
    let mut out = vec![
        "import os".to_string(),
        "import sys".to_string(),
        "from dataclasses import dataclass".to_string(),
        String::new(),
    ];
    let mut i = 0;
    while out.len() < nlines {
        out.extend([
            "@dataclass".to_string(),
            format!("class Widget{i}:"),
            format!("    name: str = 'w{i}'"),
            format!("    count: int = {i}"),
            "    def process(self, items):".to_string(),
            "        total = 0".to_string(),
            format!("        for x in items:  # comment {i}"),
            format!("            if x > {i} and x < {}:", i * 2),
            "                total += x * 2  # inline".to_string(),
            "            else:".to_string(),
            "                total -= x".to_string(),
            "        return total".to_string(),
            String::new(),
        ]);
        i += 1;
    }
    out.truncate(nlines);
    out.join("\n") + "\n"
}

/// A per-window redraw carries `lines` (content) and `highlights` (syntax spans).
/// The freshly-opened file's content frame is the first one whose serialized lines
/// mention `token`; a highlighted frame is the first with any non-empty span row.
fn lines_contain(map: &[(Value, Value)], token: &str) -> bool {
    window0_field(map, "lines").is_some_and(|v| format!("{v:?}").contains(token))
}
fn has_highlights(map: &[(Value, Value)]) -> bool {
    window0_field(map, "highlights")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|r| r.as_array().is_some_and(|s| !s.is_empty()))
        })
}

/// `:edit path`, then watch redraws for the first content frame and the first
/// highlighted frame; returns `(time_to_first_paint, time_to_highlight)`.
async fn open_and_time(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    path: &str,
    token: &str,
) -> (Option<Duration>, Option<Duration>) {
    let t0 = Instant::now();
    feed(rpc, &format!(":edit {path}\r"));

    let (mut painted, mut highlighted) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while (painted.is_none() || highlighted.is_none()) && Instant::now() < deadline {
        let Ok(Some(Incoming::Notification { method, params })) =
            tokio::time::timeout(Duration::from_millis(500), incoming.recv()).await
        else {
            break;
        };
        if method != "redraw" {
            continue;
        }
        let Some(Value::Map(map)) = params.into_iter().next() else {
            continue;
        };
        if painted.is_none() && lines_contain(&map, token) {
            painted = Some(t0.elapsed());
        }
        if painted.is_some() && highlighted.is_none() && has_highlights(&map) {
            highlighted = Some(t0.elapsed());
        }
    }
    (painted, highlighted)
}

#[tokio::test]
#[ignore = "benchmark: run explicitly with --ignored, needs the python grammar"]
async fn open_latency() {
    if !bemtvi_ts::installed_parsers()
        .iter()
        .any(|p| p.lang == "python")
    {
        eprintln!(
            "skip: python grammar not installed under {} \
             (set BEMTVI_DATA_DIR or install it)",
            bemtvi_ts::data_dir().display()
        );
        return;
    }

    let dir = temp_dir("open_latency");
    let sizes = [50usize, 500, 2000, 10000];
    let mut py = Vec::new();
    for n in sizes {
        let p = dir.join(format!("sample_{n}.py"));
        std::fs::write(&p, python_source(n)).unwrap();
        py.push(p.to_string_lossy().into_owned());
    }
    let txt = dir.join("sample.txt");
    std::fs::write(&txt, python_source(2000)).unwrap();

    let (rpc, mut incoming) = start_attached(ServerInit::default(), 120, 40).await;

    let fmt = |d: Option<Duration>| d.map(|d| format!("{d:.2?}")).unwrap_or_else(|| "—".into());
    let row = |label: &str, paint: Option<Duration>, hl: Option<Duration>| {
        eprintln!(
            "  {label:28}  first-paint {:>10}   highlight {:>10}",
            fmt(paint),
            fmt(hl)
        );
    };

    eprintln!("\nopen latency (wall clock; client-side receipt, so sub-ms gaps are noisy):");
    // The first python open is the cold one — it pays the grammar dlopen + query
    // compile. That cost must land after first paint, i.e. first-paint ≪ highlight.
    let (p, h) = open_and_time(&rpc, &mut incoming, &py[1], "dataclass").await;
    row("python 500 lines (COLD)", p, h);
    for (n, path) in sizes.iter().zip(&py) {
        if std::ptr::eq(path, &py[1]) {
            continue; // already shown as the cold row
        }
        let (p, h) = open_and_time(&rpc, &mut incoming, path, "dataclass").await;
        row(&format!("python {n} lines (warm)"), p, h);
    }
    let (p, h) = open_and_time(&rpc, &mut incoming, &txt.to_string_lossy(), "dataclass").await;
    row("plain .txt 2000 lines", p, h);

    std::fs::remove_dir_all(&dir).ok();
}
