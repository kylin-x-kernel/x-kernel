// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Copies the generated kernel symbol table blob into the build output so
//! `symtab.rs` can embed it via `include_bytes!`.
//!
//! `xkmake` stores the blob at `$TARGET_DIR/kbuild/ksymtab.bin`. Direct
//! builds without `xkmake` (e.g. plain `cargo check`) fall back to an empty
//! blob, which disables symbol annotations without breaking compilation.

use std::{env, fs, path::Path};

fn main() {
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR must be set by Cargo");
    let destination = Path::new(&out_dir).join("ksymtab.bin");

    let Some(target_dir) = env::var_os("TARGET_DIR") else {
        write_empty(&destination);
        return;
    };
    let source = Path::new(&target_dir).join("kbuild").join("ksymtab.bin");
    // The blob appears only after the first kernel build; re-run this build
    // script when it shows up or changes.
    println!("cargo:rerun-if-changed={}", source.display());

    match fs::read(&source) {
        Ok(blob) => write_if_changed(&destination, &blob),
        Err(_) => write_empty(&destination),
    }
}

fn write_empty(path: &Path) {
    fs::write(path, []).expect("failed to write empty ksymtab blob");
}

fn write_if_changed(path: &Path, contents: &[u8]) {
    match fs::read(path) {
        Ok(existing) if existing == contents => {}
        _ => {
            fs::write(path, contents).expect("failed to write ksymtab blob");
        }
    }
}
