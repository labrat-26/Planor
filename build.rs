fn main() {
    println!("cargo:rerun-if-changed=planor.manifest");
    println!("cargo:rustc-link-arg-bin=planor=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=planor=/MANIFESTINPUT:{}",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("planor.manifest")
            .display()
    );
}
