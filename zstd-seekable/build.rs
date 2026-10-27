//! Compiles only the seekable-format layer (`seekable/`) and the `xxh64`
//! helper. zstd itself comes from `zstd-sys`, whichever way it was built
//! (bundled sources or `ZSTD_SYS_USE_PKG_CONFIG`), so a binary that also
//! links the `zstd` crate carries one zstd library, not two.

use std::env;
use std::path::PathBuf;

fn main() {
    for file in [
        "build.rs",
        "xxh64.c",
        "seekable/zstd_seekable.h",
        "seekable/zstdseek_compress.c",
        "seekable/zstdseek_decompress.c",
    ] {
        println!("cargo:rerun-if-changed={file}");
    }
    println!("cargo:rerun-if-changed=seekable/common");

    let mut build = cc::Build::new();
    // zstd-sys's headers first, so zstd.h and zstd_errors.h always match the
    // library that is linked. In pkg-config mode the list may be empty and
    // the system include path supplies them.
    let zstd_includes = env::var("DEP_ZSTD_INCLUDE").unwrap_or_default();
    for dir in zstd_includes.split(';').filter(|dir| !dir.is_empty()) {
        build.include(dir);
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    // Internal helpers only (mem.h, xxhash.h and their includes); this
    // directory holds no zstd.h.
    build.include(root.join("seekable/common"));
    build.include(root.join("seekable"));
    // Inline xxHash into each object instead of exporting XXH symbols.
    build.define("XXH_PRIVATE_API", None);
    build
        .file("seekable/zstdseek_compress.c")
        .file("seekable/zstdseek_decompress.c")
        .file("xxh64.c")
        .warnings(false)
        .opt_level(3)
        .compile("zstdseek");
}
