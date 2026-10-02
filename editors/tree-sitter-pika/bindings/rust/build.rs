//! Compiles the generated parser and the external scanner.

fn main() {
    let src = std::path::Path::new("src");
    let mut build = cc::Build::new();
    build
        .std("c11")
        .include(src)
        .file(src.join("parser.c"))
        .file(src.join("scanner.c"))
        .warnings(false);
    #[cfg(target_env = "msvc")]
    build.flag("-utf-8");
    build.compile("tree-sitter-pika");
    for file in ["parser.c", "scanner.c", "tree_sitter/parser.h"] {
        println!("cargo:rerun-if-changed={}", src.join(file).display());
    }
}
