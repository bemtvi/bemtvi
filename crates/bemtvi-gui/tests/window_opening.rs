//! The window opens as a native WinUI 3 window does: with Windows' opening animation
//! (restored, or straight into maximized), and taking the keys as it shows.
//!
//! Windows plays the opening animation on a window's *first* show only, so a window
//! shown early (invisibly) and revealed later appears in one frame. These tests watch
//! the screen: a 64-pixel patch at its centre, sampled as fast as GDI gives it (about
//! one frame per display refresh), against how it was before the GUI started. Animated,
//! the patch changes over several frames (a fade); not, it jumps.
//!
//! `#[ignore]`d: they bring up a real GPU window, on the screen they look at, so run
//! them deliberately, one window at a time, with nothing moving on screen:
//! `cargo test -p bemtvi-gui --test window_opening -- --ignored --test-threads=1`.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, GetDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HDC, SRCCOPY,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXSCREEN, SM_CYSCREEN,
};

/// A throwaway config dir (its `init.lua` is `init`) and state dir, the state saying
/// how the window opens: centred at 900x600, maximized or not.
fn scratch(maximized: bool, init: &str) -> (PathBuf, PathBuf, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("bemtvi_gui_opening_{}_{n}", std::process::id()));
    let (config, state) = (root.join("config"), root.join("state"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(state.join("bemtvi-data")).unwrap();
    std::fs::write(config.join("init.lua"), init).unwrap();
    std::fs::write(
        state.join("bemtvi-data").join("gui-window"),
        format!("maximized={maximized}\nposition=centered\nsize=remember\nwidth=900\nheight=600\n"),
    )
    .unwrap();
    (root, config, state)
}

fn spawn(config: &Path, state: &Path) -> Child {
    spawn_on(config, state, &[])
}

/// The GUI on `args` (a file to open).
fn spawn_on(config: &Path, state: &Path, args: &[&Path]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bemtvi-gui"))
        .args(args)
        .env("BEMTVI_CONFIG", config)
        .env("XDG_STATE_HOME", state)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bemtvi-gui")
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("wait for bemtvi-gui").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    false
}

const PATCH: i32 = 64;

/// A patch of the screen, read back through GDI as composed (DWM's animations
/// included).
struct Screen {
    screen: HDC,
    mem: HDC,
    bits: *mut u8,
    at: (i32, i32),
}

impl Screen {
    /// The patch at the screen's centre.
    fn centre() -> Screen {
        // SAFETY: plain GDI calls on handles made here, for this process's lifetime.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = PATCH;
            info.bmiHeader.biHeight = -PATCH;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                mem,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            SelectObject(mem, bitmap as _);
            let at = (
                GetSystemMetrics(SM_CXSCREEN) / 2 - PATCH / 2,
                GetSystemMetrics(SM_CYSCREEN) / 2 - PATCH / 2,
            );
            Screen {
                screen,
                mem,
                bits: bits.cast(),
                at,
            }
        }
    }

    fn take(&self) -> Vec<u8> {
        // SAFETY: the DIB section is `PATCH`² 32-bit pixels, selected into `mem`.
        unsafe {
            BitBlt(
                self.mem,
                0,
                0,
                PATCH,
                PATCH,
                self.screen,
                self.at.0,
                self.at.1,
                SRCCOPY | CAPTUREBLT,
            );
            std::slice::from_raw_parts(self.bits, (PATCH * PATCH * 4) as usize).to_vec()
        }
    }
}

/// How far a patch is from another: the mean difference of its pixels' channels.
fn difference(a: &[u8], b: &[u8]) -> f32 {
    let sum: u64 = a
        .chunks(4)
        .zip(b.chunks(4))
        .map(|(p, q)| {
            (0..3)
                .map(|c| (p[c] as i32 - q[c] as i32).unsigned_abs() as u64)
                .sum::<u64>()
        })
        .sum();
    sum as f32 / (PATCH * PATCH) as f32
}

/// Starts the GUI on a state that opens it maximized or not, watches the screen's
/// centre until the window has been up a while, and says how it appeared: each frame's
/// difference from before it started, from the first frame that differs.
fn opening(maximized: bool) -> Vec<f32> {
    let (root, config, state) = scratch(
        maximized,
        "btv.timer(function() btv.cmd('qa!') end, 2500)\n",
    );
    let screen = Screen::centre();
    let before = screen.take();
    let mut child = spawn(&config, &state);
    let start = Instant::now();
    let mut frames = Vec::new();
    while start.elapsed() < Duration::from_secs(20) {
        let d = difference(&screen.take(), &before);
        if d > 3.0 || !frames.is_empty() {
            frames.push(d);
        }
        // Up for half a second (at ~30 frames a second): what it opened as is seen.
        if frames.len() >= 15 {
            break;
        }
    }
    assert!(
        wait_for_exit(&mut child, Duration::from_secs(30)),
        "bemtvi-gui didn't quit"
    );
    let _ = std::fs::remove_dir_all(root);
    frames
}

/// Whether the frames show an opening animation: the first frame that differs is well
/// short of where the patch ends up (a fade's first steps), and so is the one after.
/// Shown in one frame, the first that differs is (nearly) where it ends.
fn animated(frames: &[f32]) -> bool {
    let most = frames.iter().copied().fold(0.0, f32::max);
    frames.len() >= 3 && frames[0] < most * 0.5 && frames[1] < most * 0.9
}

fn check_opening(maximized: bool) {
    let frames = opening(maximized);
    let most = frames.iter().copied().fold(0.0, f32::max);
    if most < 15.0 {
        eprintln!("skip: the window looks too much like the desktop behind it to see ({frames:?})");
        return;
    }
    assert!(
        animated(&frames),
        "it appeared without the opening animation: {frames:?}"
    );
}

#[test]
#[ignore = "spawns a real GPU window and watches the screen. Run with --ignored."]
fn a_restored_window_opens_with_the_opening_animation() {
    check_opening(false);
}

#[test]
#[ignore = "spawns a real GPU window and watches the screen. Run with --ignored."]
fn a_maximized_window_opens_with_the_opening_animation() {
    check_opening(true);
}

/// Whether the window in the foreground (the one keys go to) is `pid`'s.
fn in_front(pid: u32) -> bool {
    let mut owner = 0;
    // SAFETY: plain queries of the foreground window.
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut owner) };
    owner == pid
}

/// Opening maximized on a file, the editor first shows at the window's final size: the
/// only grid it ever has (`'columns'` x `'lines'`, sampled every millisecond from the
/// start, past the editor's own 80x24 before any client attaches) is the one it has
/// once the window is up, never the restored window's, which the server would lay a
/// frame out for that the editor then draws, small in the window's corner, as the
/// window shows. (The file matters: loading it, that frame came after the show.)
#[test]
#[ignore = "spawns a real GPU window. Run with --ignored."]
fn a_maximized_window_shows_the_editor_at_its_final_size_first() {
    let (root, config, state) = scratch(true, "");
    let grids = root.join("grids.txt");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "local seen = {{}}
             local function sample()
             local grid = btv.o.columns .. 'x' .. btv.o.lines
             if seen[#seen] ~= grid then seen[#seen + 1] = grid end
             btv.timer(sample, 1)
             end
             sample()
             btv.timer(function()
             local f = io.open([[{}]], 'w')
             f:write(table.concat(seen, ' '))
             f:close()
             btv.cmd('qa!')
             end, 2500)
",
            grids.display()
        ),
    )
    .unwrap();
    let file = root.join("lines.txt");
    std::fs::write(
        &file,
        "The quick brown fox jumps over the lazy dog.
"
        .repeat(2000),
    )
    .unwrap();
    let mut child = spawn_on(&config, &state, &[&file]);
    assert!(
        wait_for_exit(&mut child, Duration::from_secs(30)),
        "bemtvi-gui didn't quit"
    );
    let text = std::fs::read_to_string(&grids).unwrap_or_default();
    let _ = std::fs::remove_dir_all(root);
    let attached: Vec<&str> = text.split(' ').skip_while(|g| *g == "80x24").collect();
    assert_eq!(attached.len(), 1, "the grids it had, in order: {text}");
}

/// Types `text` as the keyboard would, to `pid`'s window: never to another, should
/// it lose the foreground (the keys would land in whatever window the user has there).
fn type_text(pid: u32, text: &str) {
    for unit in text.encode_utf16() {
        assert!(
            in_front(pid),
            "bemtvi-gui lost the foreground: not typing into another window"
        );
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [
            key(KEYEVENTF_UNICODE),
            key(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        ];
        // SAFETY: two well-formed keyboard inputs.
        unsafe { SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Presses a virtual key (Escape, Return), to `pid`'s window only.
fn press(pid: u32, vk: u16) {
    assert!(
        in_front(pid),
        "bemtvi-gui lost the foreground: not typing into another window"
    );
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(0), key(KEYEVENTF_KEYUP)];
    // SAFETY: two well-formed keyboard inputs.
    unsafe { SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    std::thread::sleep(Duration::from_millis(15));
}

/// The editor takes the keys as the window shows, opening maximized or not: what's
/// typed then lands in the buffer, and `:wq!` writes and quits.
fn types_as_it_opens(maximized: bool) {
    // VimEnter writes a marker, so the test knows the editor is up.
    let (root, config, state) = scratch(maximized, "");
    let up = root.join("up");
    let written = root.join("typed.txt");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "btv.autocmd.create('VimEnter', {{ callback = function() \
             btv.timer(function() io.open([[{}]], 'w'):close() end, 300) end }})\n",
            up.display()
        ),
    )
    .unwrap();
    let mut child = spawn(&config, &state);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !up.exists() {
        assert!(Instant::now() < deadline, "bemtvi-gui never came up");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Shown by now (the reveal comes with the first frame); in front, with the keys:
    // never type before it is (they'd land in another window).
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !in_front(pid) {
        if Instant::now() > deadline {
            // Windows gives the foreground only to a process the foreground app
            // started (or the user): from a background shell, it can't be had.
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&root);
            eprintln!("skip: bemtvi-gui couldn't take the foreground from this session");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    type_text(pid, "ihello");
    press(pid, 0x1B); // Escape
    type_text(pid, &format!(":wq! {}", written.display()));
    press(pid, 0x0D); // Return
    let quit = wait_for_exit(&mut child, Duration::from_secs(15));
    let text = std::fs::read_to_string(&written).unwrap_or_default();
    let _ = std::fs::remove_dir_all(root);
    assert!(quit, "bemtvi-gui didn't quit: the keys didn't reach it");
    assert_eq!(text.trim(), "hello", "what was typed as it opened");
}

#[test]
#[ignore = "spawns a real GPU window and types into it. Run with --ignored."]
fn it_takes_the_keys_as_it_opens() {
    types_as_it_opens(false);
}

#[test]
#[ignore = "spawns a real GPU window and types into it. Run with --ignored."]
fn it_takes_the_keys_as_it_opens_maximized() {
    types_as_it_opens(true);
}
