//! The window's state survives the GUI closing: where it is, how large, and whether
//! maximized — what `'guiposition'`, `'guisize'` and `'guimaximize'` decide, and what
//! they remember.
//!
//! Proven by spawning the real `bemtvi-gui` binary with a throwaway config (whose
//! `init.lua` sets the option, then quits once the first frame has had time to land)
//! and a throwaway state directory, then reading the file the GUI saved on the way out.
//! `#[ignore]`d: it brings up a real GPU window, so run it deliberately with
//! `cargo test -p bemtvi-gui --test window_state -- --ignored --test-threads=1` (one
//! window at a time: two starting at once can each take longer than the config's
//! two seconds to get their first frame).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A scratch dir holding a config whose `init.lua` sets `'guimaximize'` to `mode` and
/// quits a second later, and the state dir (`$XDG_STATE_HOME`) the GUI saves into.
fn scratch(mode: &str) -> (PathBuf, PathBuf, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("bemtvi_gui_window_{}_{n}", std::process::id()));
    let config = root.join("config");
    let state = root.join("state");
    std::fs::create_dir_all(&config).expect("create scratch config dir");
    std::fs::create_dir_all(&state).expect("create scratch state dir");
    std::fs::write(
        config.join("init.lua"),
        format!("btv.o.guimaximize = {mode:?}\nbtv.timer(function() btv.cmd('qa!') end, 2000)\n"),
    )
    .expect("write init.lua");
    (root, config, state)
}

/// The file the GUI keeps the state in, under the scratch state dir.
fn state_file(state: &Path) -> PathBuf {
    // `stdpath("state")`: `bemtvi-data` on Windows, where it shares `%LOCALAPPDATA%`
    // with the config dir.
    let dir = if cfg!(windows) {
        "bemtvi-data"
    } else {
        "bemtvi"
    };
    state.join(dir).join("gui-window")
}

/// Run the GUI on the scratch config until it quits itself.
fn run(config: &Path, state: &Path) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bemtvi-gui"))
        .env("BEMTVI_CONFIG", config)
        .env("XDG_STATE_HOME", state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bemtvi-gui");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().expect("wait for bemtvi-gui") {
            assert!(status.success(), "bemtvi-gui exited with {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("bemtvi-gui did not quit itself");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The saved state's `key=value` lines.
fn saved(state: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(state_file(state))
        .unwrap_or_else(|e| panic!("no saved state at {}: {e}", state_file(state).display()))
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// A saved number.
fn num(state: &Path, key: &str) -> f32 {
    let saved = saved(state);
    saved
        .get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {key} in {saved:?}"))
}

/// An `init.lua` setting `opts` (Lua statements), then quitting once the window is up.
fn init(config: &Path, opts: &str) {
    std::fs::write(
        config.join("init.lua"),
        format!(
            "{opts}
btv.timer(function() btv.cmd('qa!') end, 2000)
"
        ),
    )
    .expect("write init.lua");
}

/// A saved window's top edge, 120 points or so below the top of the screen: Windows
/// measures `y` down from the top of the primary display, AppKit up from its bottom
/// (so there, a top at 620 leaves a 500-point window room on a 640-point screen).
const TOP: f32 = if cfg!(target_os = "macos") {
    620.0
} else {
    120.0
};

fn has_display() -> bool {
    cfg!(not(all(unix, not(target_os = "macos"))))
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
}

#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn guimaximize_decides_how_the_window_opens_next_time() {
    if !has_display() {
        eprintln!("skip: no WAYLAND_DISPLAY / DISPLAY to open a window on");
        return;
    }

    let maximized = |state: &Path| saved(state)["maximized"].clone();
    // `always` maximizes it, and saves that.
    let (root, config, state) = scratch("always");
    run(&config, &state);
    assert_eq!(maximized(&state), "true");

    // `remember` keeps it as it opened: maximized, as it was last time.
    init(&config, "");
    run(&config, &state);
    assert_eq!(maximized(&state), "true", "remember kept the last state");

    // `never` restores it, and saves that.
    init(&config, "btv.o.guimaximize = 'never'");
    run(&config, &state);
    assert_eq!(maximized(&state), "false");

    // And `remember` from restored stays restored.
    init(&config, "");
    run(&config, &state);
    assert_eq!(maximized(&state), "false");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn guiposition_and_guisize_decide_where_and_how_large_it_opens() {
    if !has_display() {
        eprintln!("skip: no WAYLAND_DISPLAY / DISPLAY to open a window on");
        return;
    }
    let (root, config, state) = scratch("remember");
    let dir = state_file(&state);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();

    // `remember`: it opens where, and as large as, it was left, and says so again.
    std::fs::write(
        &dir,
        format!("maximized=false\nposition=remember\nsize=remember\nx=100\ny={TOP}\nwidth=700\nheight=500\n"),
    )
    .unwrap();
    init(&config, "");
    run(&config, &state);
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    assert!(
        near(num(&state, "width"), 700.0) && near(num(&state, "height"), 500.0),
        "{:?}",
        saved(&state)
    );
    let placed = cfg!(not(target_os = "linux"));
    if placed {
        assert!(
            near(num(&state, "x"), 100.0) && near(num(&state, "y"), TOP),
            "{:?}",
            saved(&state)
        );
    }

    // `centered` centres it as the config sets it, and saves the mode.
    init(&config, "btv.o.guiposition = 'centered'");
    run(&config, &state);
    assert_eq!(saved(&state)["position"], "centered");
    if placed {
        assert!(
            !near(num(&state, "x"), 100.0) || !near(num(&state, "y"), TOP),
            "it moved to the centre: {:?}",
            saved(&state)
        );
    }

    // A share of the screen sizes it as the config sets it, and saves the mode.
    init(
        &config,
        "btv.o.guiposition = 'centered'
btv.o.guisize = '50%x40%'",
    );
    run(&config, &state);
    assert_eq!(saved(&state)["size"], "50%x40%");
    let (width, height) = (num(&state, "width"), num(&state, "height"));
    assert!(
        !near(width, 700.0) && width / height > 1.0,
        "{:?}",
        saved(&state)
    );

    // And it opens at that share next time: about as large as it was sized live (a
    // window not made yet takes the share of the primary display's work area, less a
    // title bar's estimate, where the live one measures its own).
    run(&config, &state);
    let about = |a: f32, b: f32| (a - b).abs() <= b * 0.03;
    assert!(
        about(num(&state, "width"), width) && about(num(&state, "height"), height),
        "{:?}",
        saved(&state)
    );

    let _ = std::fs::remove_dir_all(root);
}

/// The primary display's work area in physical pixels (left, top, right, bottom), and
/// its scale, as a per-monitor DPI-aware process sees them.
#[cfg(windows)]
fn primary_work_area() -> ((i32, i32, i32, i32), f32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: plain Win32 queries into a local.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let scale = GetDpiForSystem() as f32 / 96.0;
        ((work.left, work.top, work.right, work.bottom), scale)
    }
}

/// A window left on the right half of the screen, with room to spare, opens there
/// again. Windows makes a new window large (three quarters of the screen) before the
/// saved frame sizes it: placing it by that size pulled it back left.
#[cfg(windows)]
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn a_window_left_on_the_right_opens_there() {
    let ((_, top, right, _), scale) = primary_work_area();
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let (width, height) = (600.0_f32, 400.0_f32);
    // Its right edge a fifth of the window's width short of the screen's.
    let x = right - (width * scale * 1.2).round() as i32;
    let y = top + (100.0 * scale) as i32;
    std::fs::write(
        &file,
        format!(
            "maximized=false
position=remember
size=remember
x={x}
y={y}
width={width}
height={height}
"
        ),
    )
    .unwrap();
    init(&config, "");
    run(&config, &state);
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    assert!(
        near(num(&state, "x"), x as f32) && near(num(&state, "y"), y as f32),
        "it opened at {x},{y}: {:?}",
        saved(&state)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Centred and sized to a share of the screen, as a config sets both: it ends centred
/// at its new size (the share resized it from its top left corner, off centre).
#[cfg(windows)]
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn a_centred_window_sized_to_a_share_stays_centred() {
    let ((left, _, right, _), scale) = primary_work_area();
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "maximized=false\nposition=remember\nsize=remember\nx=100\ny=120\nwidth=700\nheight=500\n",
    )
    .unwrap();
    init(
        &config,
        "btv.o.guiposition = 'centered'\nbtv.o.guisize = '50%x40%'",
    );
    run(&config, &state);
    let (x, width) = (num(&state, "x"), num(&state, "width"));
    let centre = x + width * scale / 2.0;
    let screen_centre = (left + right) as f32 / 2.0;
    assert!(
        (centre - screen_centre).abs() <= 16.0 * scale,
        "centred at {centre} on a screen centred at {screen_centre}: {:?}",
        saved(&state)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Maximized, `centered` doesn't move it: its restored place is where it goes back to,
/// and it's centred when it opens restored next time.
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn centering_a_maximized_window_keeps_its_restored_place() {
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "maximized=true\nposition=remember\nsize=remember\nx=100\ny=120\nwidth=700\nheight=500\n",
    )
    .unwrap();
    init(&config, "btv.o.guiposition = 'centered'");
    run(&config, &state);
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    assert_eq!(saved(&state)["position"], "centered");
    if cfg!(not(target_os = "linux")) {
        assert!(
            near(num(&state, "x"), 100.0) && near(num(&state, "y"), 120.0),
            "the restored place moved: {:?}",
            saved(&state)
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Quit before the window says where it is (a config quitting at once): what was saved
/// stays, not forgotten (centred, the window opens with no frame of its own).
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn quitting_before_the_window_shows_keeps_its_size() {
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "maximized=false\nposition=centered\nsize=remember\nwidth=900\nheight=600\n",
    )
    .unwrap();
    std::fs::write(config.join("init.lua"), "btv.cmd('qa!')\n").unwrap();
    run(&config, &state);
    assert_eq!(
        (num(&state, "width"), num(&state, "height")),
        (900.0, 600.0),
        "{:?}",
        saved(&state)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Sized live to a share of the screen while it's near the screen's bottom right
/// corner, it stays on the screen: grown from its top left corner, it went past the
/// screen's edges.
#[cfg(windows)]
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn a_window_sized_live_to_a_share_stays_on_the_screen() {
    let ((_, top, right, bottom), scale) = primary_work_area();
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let (width, height) = (500.0_f32, 300.0_f32);
    let x = right - ((width + 40.0) * scale) as i32;
    let y = bottom - ((height + 80.0) * scale) as i32;
    std::fs::write(
        &file,
        format!("maximized=false\nposition=remember\nsize=remember\nx={x}\ny={y}\nwidth={width}\nheight={height}\n"),
    )
    .unwrap();
    std::fs::write(
        config.join("init.lua"),
        "btv.timer(function() btv.o.guisize = '90%x90%' end, 1000)\n\
         btv.timer(function() btv.cmd('qa!') end, 2500)\n",
    )
    .unwrap();
    run(&config, &state);
    let (x, y) = (num(&state, "x"), num(&state, "y"));
    let (w, h) = (num(&state, "width") * scale, num(&state, "height") * scale);
    // Its content inside the work area (its frame's borders may reach past it).
    assert!(
        x + w <= right as f32 + 16.0 * scale && y + h <= bottom as f32 && y >= top as f32 - 1.0,
        "{:?} on a work area to {right},{bottom}",
        saved(&state)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A window left flush against the right edge of the screen opens there again: its
/// invisible resize border past the edge (where Windows puts it, snapped or dragged
/// there) isn't taken for the window being off the screen and pulled back in.
#[cfg(windows)]
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn a_window_left_flush_against_the_screens_edge_opens_there() {
    let ((_, top, right, _), _) = primary_work_area();
    let scale_of_test = || primary_work_area().1;
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    // First where it opens unplaced near the right edge, to learn its outer width.
    std::fs::write(
        &file,
        format!("maximized=false\nposition=remember\nsize=remember\nx={}\ny={}\nwidth=600\nheight=400\n", right - 2000, top + 200),
    )
    .unwrap();
    init(&config, "");
    run(&config, &state);
    let x0 = num(&state, "x");
    // The outer rect reaches past the content by the frame; place it so its outer
    // right edge is a few pixels past the screen's (its invisible border there).
    let probe = right as f32 - 1.0;
    std::fs::write(
        &file,
        format!("maximized=false\nposition=remember\nsize=remember\nx={probe}\ny={}\nwidth=600\nheight=400\n", top + 200),
    )
    .unwrap();
    run(&config, &state);
    // Clamped all the way in: its outer right edge at the screen's. That x tells the
    // outer width: x_clamped = right - outer_width.
    let clamped = num(&state, "x");
    let outer = right as f32 - clamped;
    // Now flush, the way Windows leaves it: the visible edge on the screen's, the
    // invisible border (outer less visible, at most the frame) past it.
    // Windows' invisible resize border at this scale (11 px at 200%, 7 at 100%).
    let flush = clamped + (outer - 1200.0 * scale_of_test()) / 2.0 - (scale_of_test()).round();
    std::fs::write(
        &file,
        format!("maximized=false\nposition=remember\nsize=remember\nx={flush}\ny={}\nwidth=600\nheight=400\n", top + 200),
    )
    .unwrap();
    run(&config, &state);
    assert_eq!(
        num(&state, "x"),
        flush,
        "flush at {flush} (outer {outer} wide, first opened at {x0}): {:?}",
        saved(&state)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Left maximized, it opens maximized, and its restored frame is still where and as
/// large as it was: the size the window is made at, applied after it was maximized
/// (still hidden), un-maximized it on macOS, at the screen's top left corner, and that
/// was saved.
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn a_window_left_maximized_opens_maximized_keeping_its_restored_frame() {
    if !has_display() {
        eprintln!("skip: no WAYLAND_DISPLAY / DISPLAY to open a window on");
        return;
    }
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!("maximized=true\nposition=remember\nsize=remember\nx=100\ny={TOP}\nwidth=700\nheight=500\n"),
    )
    .unwrap();
    init(&config, "");
    run(&config, &state);
    assert_eq!(saved(&state)["maximized"], "true", "{:?}", saved(&state));
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    assert!(
        near(num(&state, "width"), 700.0) && near(num(&state, "height"), 500.0),
        "{:?}",
        saved(&state)
    );
    if cfg!(not(target_os = "linux")) {
        assert!(
            near(num(&state, "x"), 100.0) && near(num(&state, "y"), TOP),
            "{:?}",
            saved(&state)
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Maximized once it's up, its restored frame is the one it had. On macOS the zoom is
/// animated when the app is frontmost (as it is when the user zooms it), and each step
/// was taken for the restored frame: the last one short of the screen, nearly as large
/// as it, was saved, and reopened with a gap to the screen's right and bottom edges.
/// (A test run whose window isn't frontmost zooms it in one step, and passes either
/// way.)
#[test]
#[ignore = "spawns a real GPU window; needs a display. Run with --ignored."]
fn maximizing_a_shown_window_keeps_its_restored_frame() {
    if !has_display() {
        eprintln!("skip: no WAYLAND_DISPLAY / DISPLAY to open a window on");
        return;
    }
    let (root, config, state) = scratch("remember");
    let file = state_file(&state);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!("maximized=false\nposition=remember\nsize=remember\nx=100\ny={TOP}\nwidth=700\nheight=500\n"),
    )
    .unwrap();
    std::fs::write(
        config.join("init.lua"),
        "btv.timer(function() btv.o.guimaximize = 'always' end, 1000)\n\
         btv.timer(function() btv.cmd('qa!') end, 2500)\n",
    )
    .unwrap();
    run(&config, &state);
    assert_eq!(saved(&state)["maximized"], "true", "{:?}", saved(&state));
    let near = |a: f32, b: f32| (a - b).abs() <= 1.0;
    assert!(
        near(num(&state, "width"), 700.0) && near(num(&state, "height"), 500.0),
        "{:?}",
        saved(&state)
    );
    if cfg!(not(target_os = "linux")) {
        assert!(
            near(num(&state, "x"), 100.0) && near(num(&state, "y"), TOP),
            "{:?}",
            saved(&state)
        );
    }
    let _ = std::fs::remove_dir_all(root);
}
