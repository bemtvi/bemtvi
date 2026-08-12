fn main() {
    let mut build = cc::Build::new();
    build
        .include("csrc")
        .include("vendor/utf8proc")
        .file("csrc/nvim/regexp.c")
        .file("csrc/nvim/mbyte.c")
        .file("csrc/nvim/charset.c")
        .file("csrc/nvim/strings.c")
        .file("csrc/nvim/garray.c")
        .file("csrc/shim/btvre_shim.c")
        .file("vendor/utf8proc/utf8proc.c")
        .define("UTF8PROC_STATIC", None)
        .std("c11")
        // The vendored sources are upstream code compiled outside their home
        // build system; their warnings are not ours to fix.
        .warnings(false);

    // MSVC lacks the POSIX `ssize_t` the vendored sources expect. Force-include
    // a portability header (via /FI) rather than patching the vendored tree, so
    // csrc/nvim stays byte-for-byte upstream across re-vendoring.
    if build.get_compiler().is_like_msvc() {
        build.flag("/FIbtvre_compat.h");
    }

    build.compile("bemtvi_regex_c");

    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/utf8proc");
}
