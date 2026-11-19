fn main() {
    let bytes = std::fs::read("native.c").expect("native.c");
    println!("cargo:rustc-env=FRUST_NATIVE_BYTES={}", bytes.len());
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rerun-if-changed=build.rs");
}
