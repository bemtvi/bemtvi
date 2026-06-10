//! The scroll-animation band on the web open path.
//!
//! The serverless web build animates scrolling in the browser (`web/index.html`),
//! interpolating a *band* of rows the core hands it on the redraw that moved the
//! viewport. The contract this build must uphold is: when a scroll command moves
//! the focused window more than a line, that window's view JSON carries a `scroll`
//! object with the slide's endpoints (`from_top`/`to_top`, `from_cursor`/
//! `to_cursor`), a `duration_ms`, and a self-contained band (`base_line` +
//! `lines`/`numbers`/`selection`) covering every row visible during the slide.
//! Without it the JS has nothing to interpolate and the view snaps. These drive
//! the real `WebEditor` exactly as `web/index.html` does.

use nxvim_web::WebEditor;
use serde_json::Value;

/// Parse the editor's projected view JSON and return its focused window.
fn focused_window(ed: &mut WebEditor) -> Value {
    let view: Value = serde_json::from_str(&ed.view_json()).expect("view_json is valid JSON");
    let windows = view["windows"].as_array().expect("windows array");
    windows
        .iter()
        .find(|w| w["focused"].as_bool() == Some(true))
        .or_else(|| windows.first())
        .cloned()
        .expect("at least one window")
}

/// A buffer with enough lines that a half-page scroll moves the viewport well past
/// a single line (so the core emits a scroll gesture). Each line is uniquely
/// numbered so band slicing is checkable.
fn long_buffer(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The 1-based buffer line currently at the top of the focused window (its first
/// visible line number), so a wheel scroll's *distance* is checkable.
fn top_line(ed: &mut WebEditor) -> u64 {
    let w = focused_window(ed);
    w["numbers"][0]
        .as_u64()
        .expect("top row carries its 1-based buffer line number")
}

#[test]
fn ctrl_d_projects_a_scroll_band_into_the_view() {
    let mut ed = WebEditor::new(80, 24);
    // `WebEditor::new` installs a wasm panic hook that aborts on a host target;
    // drop it so a failing assertion unwinds and reports normally.
    let _ = std::panic::take_hook();

    ed.load_file("big.txt", &long_buffer(500));

    // A fresh view, before any scroll, carries no gesture to animate.
    let w = focused_window(&mut ed);
    assert!(
        w["scroll"].is_null(),
        "an unscrolled window must not carry a scroll band"
    );

    // <C-d> scrolls half a page — more than a line — so the next redraw carries the
    // slide the browser animates.
    ed.input("<C-d>");
    let w = focused_window(&mut ed);
    let scroll = &w["scroll"];
    assert!(
        scroll.is_object(),
        "<C-d> must project a scroll band; got {scroll}"
    );

    // Endpoints: the viewport really moved (from_top != to_top), downward.
    let from_top = scroll["from_top"].as_u64().expect("from_top is a number");
    let to_top = scroll["to_top"].as_u64().expect("to_top is a number");
    assert_eq!(from_top, 0, "the slide starts at the original top line");
    assert!(
        to_top > from_top,
        "<C-d> scrolls down, so to_top ({to_top}) must exceed from_top ({from_top})"
    );

    // A positive duration, or the JS would divide by zero computing progress.
    assert!(
        scroll["duration_ms"].as_u64().unwrap_or(0) > 0,
        "the slide needs a non-zero duration_ms to animate"
    );

    // The band is self-contained and anchored at base_line = min(from_top, to_top).
    let base_line = scroll["base_line"].as_u64().expect("base_line is a number");
    assert_eq!(
        base_line,
        from_top.min(to_top),
        "band anchors at min endpoint"
    );
    let band = scroll["lines"].as_array().expect("band lines array");
    let numbers = scroll["numbers"].as_array().expect("band numbers array");
    let selection = scroll["selection"]
        .as_array()
        .expect("band selection array");
    assert_eq!(
        band.len(),
        numbers.len(),
        "every band row carries a line number slot"
    );
    assert_eq!(
        band.len(),
        selection.len(),
        "every band row carries a selection slot"
    );

    // The band spans the whole slide: it must be at least the travel distance plus
    // the visible height (24 rows minus the command row), so the JS can slice any
    // intermediate frame from it.
    let travel = (to_top - from_top) as usize;
    assert!(
        band.len() > travel,
        "band ({} rows) must cover the travel distance ({travel})",
        band.len()
    );

    // The first band row is buffer line base_line+1 (1-based numbering), confirming
    // the band is anchored where the JS expects to slice from.
    assert_eq!(
        numbers[0].as_u64(),
        Some(base_line + 1),
        "band row 0 is the 1-based buffer line at base_line"
    );
    assert_eq!(
        band[0].as_str(),
        Some(format!("line {}", base_line + 1).as_str()),
        "band row 0 carries the text of the line at base_line"
    );
}

#[test]
fn a_single_line_scroll_emits_no_band() {
    let mut ed = WebEditor::new(80, 24);
    let _ = std::panic::take_hook();

    ed.load_file("big.txt", &long_buffer(500));
    // Move the cursor down within the viewport: no viewport motion, no gesture.
    ed.input("j");
    let w = focused_window(&mut ed);
    assert!(
        w["scroll"].is_null(),
        "a cursor move that doesn't scroll the viewport emits no band"
    );

    // `<C-e>` scrolls exactly one line — vim animates only moves of *more* than a
    // line, so this too carries no band (the view just snaps the single row).
    ed.input("<C-e>");
    let w = focused_window(&mut ed);
    assert!(
        w["scroll"].is_null(),
        "a one-line scroll is below the animation threshold; no band"
    );
}

/// The cell height the JS frontend measures and passes to `wheel` (FONT_PX * LINE,
/// rounded — see `web/index.html`). Pixel deltas below this are sub-line.
const CELL_H: f64 = 21.0;
const CELL_W: f64 = 9.0;

#[test]
fn pixel_wheel_accumulates_sub_line_deltas_before_scrolling() {
    let mut ed = WebEditor::new(80, 24);
    let _ = std::panic::take_hook();
    ed.load_file("big.txt", &long_buffer(500));

    assert_eq!(top_line(&mut ed), 1, "view starts at the first line");

    // A pixel-precise device (trackpad / hi-res wheel) fires many small deltas, each
    // a fraction of a line. The bug: every such event scrolled a full `'mousescroll'`
    // step, so a gentle swipe flew down the buffer. A 7px delta over a 21px cell is a
    // third of a line — two of them must not move the viewport at all.
    ed.wheel(0.0, 7.0, 0, CELL_W, CELL_H, "", 5, 5, 1.0);
    assert_eq!(
        top_line(&mut ed),
        1,
        "a third-of-a-line wheel delta must not scroll yet"
    );
    ed.wheel(0.0, 7.0, 0, CELL_W, CELL_H, "", 5, 5, 1.0);
    assert_eq!(
        top_line(&mut ed),
        1,
        "two thirds of a line is still below one notch"
    );

    // The third delta crosses a whole line → exactly one notch, which scrolls the
    // default `'mousescroll'` (3 lines): top goes line 1 → line 4. The old
    // notch-per-event behavior would already be three notches (line 10) deep here.
    ed.wheel(0.0, 7.0, 0, CELL_W, CELL_H, "", 5, 5, 1.0);
    assert_eq!(
        top_line(&mut ed),
        4,
        "the third sub-line delta completes one notch = one mousescroll step (3 lines)"
    );
}

#[test]
fn line_mode_wheel_scrolls_one_mousescroll_step_per_notch() {
    let mut ed = WebEditor::new(80, 24);
    let _ = std::panic::take_hook();
    ed.load_file("big.txt", &long_buffer(500));

    // A classic wheel reports whole-line deltas (deltaMode 1): one detent = one notch
    // = one `'mousescroll'` step of 3 lines. Down then back up returns to the top.
    ed.wheel(0.0, 1.0, 1, CELL_W, CELL_H, "", 5, 5, 1.0);
    assert_eq!(top_line(&mut ed), 4, "one down notch scrolls 3 lines");
    ed.wheel(0.0, -1.0, 1, CELL_W, CELL_H, "", 5, 5, 1.0);
    assert_eq!(top_line(&mut ed), 1, "one up notch scrolls back");
}

#[test]
fn the_scroll_band_is_one_shot() {
    let mut ed = WebEditor::new(80, 24);
    let _ = std::panic::take_hook();

    ed.load_file("big.txt", &long_buffer(500));
    ed.input("<C-d>");

    // First projection after the scroll carries the band…
    let w = focused_window(&mut ed);
    assert!(
        w["scroll"].is_object(),
        "the slide is present on the first redraw"
    );

    // …and a second projection with no new input does not — the gesture animates
    // exactly once, so a re-render mid-slide (e.g. an async highlight reply) won't
    // re-arm and restart the animation.
    let w = focused_window(&mut ed);
    assert!(
        w["scroll"].is_null(),
        "the scroll band must be consumed after one projection"
    );
}
