//! Build script for the GUI client. On Windows it embeds the app icon (and the
//! version/product info winresource derives from Cargo) into `bemtvi-gui.exe`, so
//! Explorer, the taskbar, and Alt-Tab show the bemtvi icon instead of the generic
//! executable icon. On every other platform this is a no-op.
//!
//! It also embeds `assets/bemtvi-file.ico`, the icon for documents opened with
//! bemtvi, as the exe's second icon (resource ID 2). A file-association
//! registration points its `DefaultIcon` at `bemtvi-gui.exe,1` (by index) or
//! `bemtvi-gui.exe,-2` (by ID). The app icon must keep the lowest ID, because
//! Explorer shows the first icon group as the exe's own icon.
//!
//! `assets/gen-icons.py` generates both `.ico` files from the hand-drawn
//! pixel art: `bemtvi.ico` from the same sources as the Linux AppImage and macOS
//! `.icns` icons (`assets/icons/`), `bemtvi-file.ico` from `fileicon32.png`.
//! Rerun it after the art changes (see `assets/README.md`).

fn main() {
    // `#[cfg(windows)]` here is the *host* (build scripts compile for the host).
    // The `winresource` build-dependency is gated to a Windows host too, so on
    // any other host the crate isn't present and this block must not reference
    // it. The inner CARGO_CFG_TARGET_OS check guards the rare Windows-host →
    // non-Windows-target cross build.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/bemtvi.ico");
        println!("cargo:rerun-if-changed=assets/bemtvi-file.ico");
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/bemtvi.ico")
                .set_icon_with_id("assets/bemtvi-file.ico", "2");
            // Fail loud: a broken resource compile must not silently ship an
            // unbranded .exe (project convention — no silent stubs).
            if let Err(e) = res.compile() {
                panic!("failed to embed Windows resources (icon/version): {e}");
            }
        }
    }
}
