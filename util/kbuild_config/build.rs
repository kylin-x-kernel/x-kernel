use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // Get the path to the generated config.rs
    let out_dir = env::var("OUT_DIR").unwrap();
    let config_rs_path = Path::new(&out_dir).join("config.rs");

    // If cargo-kbuild has generated the config.rs, copy it to OUT_DIR
    // Otherwise, generate an empty one
    let workspace_root = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&workspace_root)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let target_config_path = workspace_root.join("target/kbuild/config.rs");

    if target_config_path.exists() {
        // Copy the generated config
        let config_content =
            fs::read_to_string(&target_config_path).expect("Failed to read generated config.rs");
        fs::write(&config_rs_path, config_content).expect("Failed to write config.rs to OUT_DIR");
    } else {
        // Generate empty config if not available
        fs::write(&config_rs_path, "// No config.rs generated yet\n")
            .expect("Failed to write empty config.rs");
    }

    // Set environment variable for inclusion
    println!(
        "cargo:rustc-env=CONFIG_RS_PATH={}",
        config_rs_path.display()
    );
    println!("cargo:rerun-if-changed={}", target_config_path.display());
}
