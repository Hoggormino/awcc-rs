use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let material_path = manifest_dir.join("../../ui-libraries/material-1.1.0/material.slint");

    println!("cargo:rerun-if-changed=ui/main.slint");
    println!("cargo:rerun-if-changed=ui/fan-curve.slint");
    println!("cargo:rerun-if-changed={}", material_path.display());

    let library_paths = HashMap::from([("material".to_string(), material_path)]);
    let config = slint_build::CompilerConfiguration::new().with_library_paths(library_paths);

    if let Err(err) = slint_build::compile_with_config("ui/main.slint", config) {
        panic!("Failed to compile ui/main.slint: {err}");
    }
}
