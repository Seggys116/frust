use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("native.o");
    let archive = out.join("libfrust_native.a");
    assert!(Command::new("cc")
        .args(["-c", "native.c", "-o"])
        .arg(&object)
        .status()
        .expect("run cc")
        .success());
    assert!(Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .expect("run ar")
        .success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=frust_native");
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rerun-if-changed=build.rs");
}
