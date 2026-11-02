// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Source paths as written into facts records.
//!
//! Files in the repository are repository-relative. Files elsewhere get a
//! host-independent prefix so that facts from different machines compare
//! equal:
//!
//! | Source | Written as |
//! | --- | --- |
//! | crates.io (`$CARGO_HOME/registry/src/<index>/`) | `crates/<name>-<version>/<path>` |
//! | git dependency (`$CARGO_HOME/git/checkouts/`) | `git/<checkout>/<rev>/<path>` |
//! | standard library (`/rustc/<commit>/`) | `rust/library/<crate>/<path>` |
//!
//! Any other path is written as rustc reports it.

use std::path::{Component, Path, PathBuf};

const REGISTRY_SRC: &str = "/registry/src/";
const GIT_CHECKOUTS: &str = "/git/checkouts/";
const RUSTC_REMAP: &str = "/rustc/";

/// Lexically resolves `.` and `..` (`#[path = "../x.rs"]` modules) without
/// touching the file system.
pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Returns the facts form of `path`; `root` is the repository root.
pub(super) fn display(path: &Path, root: &Path) -> String {
    let path = normalize(path);
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_string_lossy().into_owned();
    }
    display_str(&path.to_string_lossy())
}

/// Maps a path string outside the repository (also used for file names that
/// rustc remapped, which have no local path).
pub(super) fn display_str(path: &str) -> String {
    if let Some((_, rest)) = path.split_once(REGISTRY_SRC) {
        // Skip the registry index directory (`index.crates.io-<hash>`).
        if let Some((_, krate)) = rest.split_once('/') {
            return format!("crates/{krate}");
        }
    }
    if let Some((_, rest)) = path.split_once(GIT_CHECKOUTS) {
        return format!("git/{rest}");
    }
    if let Some(rest) = path.strip_prefix(RUSTC_REMAP) {
        // Skip the compiler commit hash.
        if let Some((_, library)) = rest.split_once('/') {
            return format!("rust/{library}");
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{display, display_str, normalize};

    #[test]
    fn resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("util/unittest/src/../ut-tests/framework.rs")),
            Path::new("util/unittest/ut-tests/framework.rs")
        );
        assert_eq!(normalize(Path::new("./a/./b")), Path::new("a/b"));
    }

    #[test]
    fn keeps_repository_paths_relative() {
        let root = Path::new("/w/x-kernel");
        assert_eq!(
            display(Path::new("task/kspin/src/lib.rs"), root),
            "task/kspin/src/lib.rs"
        );
        assert_eq!(
            display(Path::new("/w/x-kernel/fs/kvfs/src/../src/a.rs"), root),
            "fs/kvfs/src/a.rs"
        );
    }

    #[test]
    fn prefixes_paths_outside_the_repository() {
        assert_eq!(
            display_str(
                "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/log-0.4.28/src/lib.\
                 rs"
            ),
            "crates/log-0.4.28/src/lib.rs"
        );
        assert_eq!(
            display_str("/home/u/.cargo/git/checkouts/smoltcp-0123456789abcdef/a1b2c3d/src/lib.rs"),
            "git/smoltcp-0123456789abcdef/a1b2c3d/src/lib.rs"
        );
        assert_eq!(
            display_str("/rustc/e370b60cf2b0d3e4b55923ec1558c5b5f8970cfb/library/core/src/cell.rs"),
            "rust/library/core/src/cell.rs"
        );
        assert_eq!(display_str("<built-in>"), "<built-in>");
    }
}
