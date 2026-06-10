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
        .file("csrc/shim/nxre_shim.c")
        .file("vendor/utf8proc/utf8proc.c")
        .define("UTF8PROC_STATIC", None)
        .std("c11")
        // The vendored sources are upstream code compiled outside their home
        // build system; their warnings are not ours to fix.
        .warnings(false);
    build.compile("nxvim_regex_c");

    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/utf8proc");
}
