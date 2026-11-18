//! How the window was when the GUI last closed — where, how large, whether maximized —
//! and the `'guiposition'` / `'guisize'` modes then: what the next window opens by. The
//! window opens before a config runs, so this can't wait for the options: the GUI saves
//! what it resolved at close, and reads it back before the window is built.
//!
//! `key=value` lines in `stdpath("state")/gui-window`:
//!
//! ```text
//! maximized=false
//! position=remember
//! size=60%x70%
//! x=120
//! y=80
//! width=960
//! height=640
//! ```
//!
//! `x`/`y` are the window's top-left corner in the platform's own desktop coordinates
//! (mitsuami's `WindowFrame`), absent where the platform can't tell (GTK, Wayland);
//! `width`/`height` its content size in points, when it isn't maximized. A client-side
//! file: the window is this machine's, whichever server (local or a daemon's) it shows.

use std::path::PathBuf;

use bemtvi_gpu::{WindowMaximize, WindowPosition, WindowSizing};
use mitsuami::prelude::{Point, Size, WindowFrame};

/// What the window opens by.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Saved {
    pub maximized: bool,
    pub position: WindowPosition,
    pub sizing: WindowSizing,
    /// Its restored frame (where, and as large, when not maximized), if one was saved.
    pub frame: Option<WindowFrame>,
}

/// The file the state lives in, or `None` with no state directory to keep it in.
fn path() -> Option<PathBuf> {
    bemtvi_core::stdpath::state_dir().map(|dir| dir.join("gui-window"))
}

/// How the window was when the GUI last closed; the defaults when nothing was saved
/// yet. A file that can't be read, or a line in it that makes no sense, is reported (on
/// the terminal it was started from, if any) and left at its default.
pub fn load() -> Saved {
    let Some(path) = path() else {
        return Saved::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
        Err(e) => {
            eprintln!("bemtvi-gui: can't read {}: {e}", path.display());
            Saved::default()
        }
    }
}

fn parse(text: &str, name: &str) -> Saved {
    let mut saved = Saved::default();
    let (mut x, mut y, mut width, mut height) = (None, None, None, None);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parsed = match line.split_once('=') {
            Some(("maximized", v)) => v.parse().ok().map(|v| saved.maximized = v),
            Some(("position", v)) => WindowPosition::parse(v).map(|v| saved.position = v),
            Some(("size", v)) => WindowSizing::parse(v).map(|v| saved.sizing = v),
            Some(("x", v)) => v.parse::<f32>().ok().map(|v| x = Some(v)),
            Some(("y", v)) => v.parse::<f32>().ok().map(|v| y = Some(v)),
            Some(("width", v)) => v.parse::<f32>().ok().map(|v| width = Some(v)),
            Some(("height", v)) => v.parse::<f32>().ok().map(|v| height = Some(v)),
            _ => None,
        };
        if parsed.is_none() {
            eprintln!("bemtvi-gui: ignoring {line:?} in {name}");
        }
    }
    if let (Some(width), Some(height)) = (width, height) {
        let position = x.zip(y).map(|(x, y)| Point::new(x, y));
        saved.frame = Some(WindowFrame {
            position,
            size: Size::new(width, height),
        });
    }
    saved
}

/// Save how the window opens next time: maximized as `maximize` resolves it (as it is
/// now, `saved.maximized`, under `remember`), the restored frame it has, and the
/// position and size modes.
pub fn save(maximize: WindowMaximize, saved: Saved) {
    let maximized = match maximize {
        WindowMaximize::Remember => saved.maximized,
        WindowMaximize::Always => true,
        WindowMaximize::Never => false,
    };
    let Some(path) = path() else {
        eprintln!("bemtvi-gui: not saving the window's state: no state directory");
        return;
    };
    let mut text = format!(
        "maximized={maximized}\nposition={}\nsize={}\n",
        saved.position.spec(),
        saved.sizing.spec()
    );
    if let Some(frame) = saved.frame {
        if let Some(at) = frame.position {
            text += &format!("x={}\ny={}\n", at.x, at.y);
        }
        text += &format!("width={}\nheight={}\n", frame.size.width, frame.size.height);
    }
    // Atomically (a temp file renamed over it): another bemtvi-gui reading it as this one
    // quits, or a crash mid-write, sees the old state or the new, never half of one.
    use bemtvi_core::host::HostFs as _;
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| bemtvi_core::host::StdHostFs.write_atomic(&path, text.as_bytes()));
    if let Err(e) = written {
        eprintln!("bemtvi-gui: can't save {}: {e}", path.display());
    }
}
