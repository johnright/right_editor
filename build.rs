//! Windows only: link assets/icon.res (built by tools/icon) into the exe.
//! link.exe takes a .res directly, no rc step needed.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.res");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins={dir}/assets/icon.res");
    }
}
