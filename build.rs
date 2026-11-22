fn main() {
    println!("cargo:rerun-if-changed=planor.manifest");
    println!("cargo:rerun-if-changed=planor.rc");
    println!("cargo:rerun-if-changed=assets/planor.ico");
    println!("cargo:rustc-link-arg-bin=planor=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=planor=/MANIFESTINPUT:{}",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("planor.manifest")
            .display()
    );
    // App icon (resource id 1) for the tray, the settings window and Explorer.
    embed_resource::compile_for("planor.rc", ["planor"], embed_resource::NONE)
        .manifest_required()
        .expect("compile planor.rc (needs the Windows SDK resource compiler)");
}
