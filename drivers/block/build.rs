// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Build script for the `block` crate.
//!
//! When `XKERNEL_RAMDISK_IMG` is set (an absolute path to a filesystem image),
//! it is forwarded to the crate via `cargo:rustc-env` so that the
//! `ramdisk-static` feature can embed it with `include_bytes!(env!(...))`.
//!
//! The variable is optional: when the `ramdisk-static` feature is off the
//! gated module that consumes it is not compiled, so leaving the variable
//! unset is perfectly fine.
//!
//! Because `include_bytes!(env!(...))` is opaque to Cargo's dependency
//! tracking, a content fingerprint is also emitted so the crate is rebuilt
//! (and the image re-embedded) whenever the image file changes -- either by
//! regeneration at the same path or by pointing `XKERNEL_RAMDISK_IMG` at a
//! different file.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=XKERNEL_RAMDISK_IMG");

    let Ok(img) = std::env::var("XKERNEL_RAMDISK_IMG") else {
        // Feature not in use; nothing to embed.
        return;
    };

    let path = Path::new(&img);
    // `canonicalize` requires the file to exist; fall back to the raw path so
    // that a missing image produces a clear `include_bytes!` error at compile
    // time rather than a build-script failure.
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG={}", abs.display());

    let Ok(meta) = std::fs::metadata(&abs) else {
        // The variable points at a path that does not exist (typically because
        // the `ramdisk-static` feature is not in use). Emitting
        // `rerun-if-changed` for a missing file would make Cargo flag this
        // build script as stale on every invocation, so we deliberately omit
        // it here. If the feature is later enabled and the image is generated,
        // the resulting `include_bytes!` error at compile time makes the
        // misconfiguration obvious.
        return;
    };

    // The image exists: track it so the crate is rebuilt (and the image
    // re-embedded) whenever it changes. The byte length is also exposed so the
    // source can size its static array without a second `include_bytes!` call.
    println!("cargo:rerun-if-changed={}", abs.display());
    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG_LEN={}", meta.len());
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("{}:{}", d.as_secs(), d.subsec_nanos()))
        .unwrap_or_else(|| "0".into());
    println!(
        "cargo:rustc-env=XKERNEL_RAMDISK_IMG_FINGERPRINT={}_{}",
        meta.len(),
        mtime
    );
}
