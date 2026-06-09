//! Release builds embed `../nxvim-web/web` into the binary via `rust-embed`. The
//! `web/pkg/` (the wasm-bindgen output) and `web/vendor/` (tree-sitter + socket.io
//! assets) are build outputs, gitignored and absent until `crates/nxvim-web/build.sh`
//! runs. A *release* build that skipped it would silently embed only the committed
//! `index.html`/`highlight.js` and serve a broken UI (404 on the wasm). Fail loud
//! instead — in line with the project's no-silent-stubs rule. Debug builds read the
//! tree from disk at runtime, so a missing `pkg/` there is fine (it 404s until built).

use std::path::Path;

fn main() {
    let web = Path::new("../nxvim-web/web");
    // Re-run if the frontend bundle appears/disappears or its entrypoint changes.
    println!("cargo:rerun-if-changed=../nxvim-web/web/pkg/nxvim_web.js");

    // PROFILE is "release" for `--release` (and any profile with debug-assertions
    // off, which is what flips rust-embed from disk-read to compile-time embed).
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        let entry = web.join("pkg/nxvim_web.js");
        if !entry.exists() {
            panic!(
                "nxvim-web-bridge release build embeds the web frontend, but {} is \
                 missing — run `crates/nxvim-web/build.sh` before building the bridge \
                 in release.",
                entry.display()
            );
        }
    }
}
