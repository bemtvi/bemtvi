//! Build script for the GUI client. On Windows it embeds the app icon (and the
//! version/product info winresource derives from Cargo) into `nxvim-gui.exe`, so
//! Explorer, the taskbar, and Alt-Tab show the NX mark instead of the generic
//! executable icon. On every other platform this is a no-op.
//!
//! The icon is the same `assets/nxvim.ico` brand mark as the Linux AppImage
//! (`assets/nxvim.svg` → `nxvim.png`) and the macOS `.icns`; regenerate the
//! `.ico` from the SVG if the brand changes (see `assets/README.md`).

fn main() {
    // `#[cfg(windows)]` here is the *host* (build scripts compile for the host).
    // The `winresource` build-dependency is gated to a Windows host too, so on
    // any other host the crate isn't present and this block must not reference
    // it. The inner CARGO_CFG_TARGET_OS check guards the rare Windows-host →
    // non-Windows-target cross build.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/nxvim.ico");
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/nxvim.ico");
            // Fail loud: a broken resource compile must not silently ship an
            // unbranded .exe (project convention — no silent stubs).
            if let Err(e) = res.compile() {
                panic!("failed to embed Windows resources (icon/version): {e}");
            }
        }
    }
}
