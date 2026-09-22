use std::path::PathBuf;
use std::{env, fs};

fn main() {
    // Cargo のビルド中にはダウンロードせず、Nix が固定した libvpx を使用する。
    println!("cargo:rerun-if-changed=build.rs");
    let library = pkg_config::Config::new()
        .atleast_version("1.15")
        .probe("vpx")
        .expect("libvpx development package is required");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let mut builder = bindgen::Builder::default()
        .header_contents(
            "rho-vpx.h",
            "#include <vpx/vp8cx.h>\n#include <vpx/vp8dx.h>\n#include <vpx/vpx_decoder.h>\n",
        )
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for include in library.include_paths {
        builder = builder.clang_arg(format!("-I{}", include.display()));
    }
    builder
        .generate()
        .expect("generate libvpx bindings")
        .write_to_file(out.join("bindings.rs"))
        .expect("write bindings");
    fs::write(out.join("metadata.rs"), format!(
        "pub const BUILD_METADATA_REPOSITORY: &str = \"https://github.com/webmproject/libvpx\";\npub const BUILD_METADATA_VERSION: &str = {:?};\n",
        library.version
    )).expect("write metadata");
}
