// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Recognizes third-party crates kept under the workspace `vendor/` directory.
//!
//! Vendored code is imported as-is and CG rules do not apply to it (see
//! `docs/code-guidelines/README.md`, "Vendored code"). Cargo still treats these
//! path dependencies as local packages: it does not cap their lints, and the
//! dependency facts scan and `--fix` both send them through the Dylint driver.
//! [`crate::register_lints`] therefore registers no checking pass for them.

use std::{path::Path, sync::OnceLock};

/// Returns whether the crate being compiled lives directly under a `vendor/`
/// directory, judged from the `CARGO_MANIFEST_DIR` Cargo sets for rustc.
pub(crate) fn is_vendored_crate() -> bool {
    static IS_VENDORED: OnceLock<bool> = OnceLock::new();
    *IS_VENDORED.get_or_init(|| {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .is_some_and(|dir| is_vendored_manifest_dir(Path::new(&dir)))
    })
}

/// Returns whether `dir` is a crate manifest directory whose parent is named
/// `vendor`.
fn is_vendored_manifest_dir(dir: &Path) -> bool {
    dir.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "vendor")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_vendored_manifest_dir;

    #[test]
    fn only_direct_children_of_vendor_are_vendored() {
        assert!(is_vendored_manifest_dir(Path::new("/src/x-kernel/vendor/acpi")));
        assert!(is_vendored_manifest_dir(Path::new("vendor/acpi")));
        assert!(!is_vendored_manifest_dir(Path::new("/src/x-kernel/vendor")));
        assert!(!is_vendored_manifest_dir(Path::new("/src/x-kernel/vendor/acpi/sub")));
        assert!(!is_vendored_manifest_dir(Path::new("/src/x-kernel/tools/lints")));
        assert!(!is_vendored_manifest_dir(Path::new("/src/x-kernel/vendored/acpi")));
    }

    /// A crate whose manifest sits under `vendor/` gets no diagnostics. The
    /// static is the first item `unregistered_static_spinlock` reports in its
    /// own UI test, so only the central guard keeps this output empty.
    const VENDORED_SOURCE: &str = r#"// rustc-env:CARGO_MANIFEST_DIR=/src/x-kernel/vendor/fixture
#![allow(dead_code, unused)]

use kspin::SpinNoIrq;

static A: SpinNoIrq<u32> = SpinNoIrq::new(0);

fn main() {}
"#;

    #[test]
    fn vendored_crate_gets_no_diagnostics() {
        crate::testing::run_ui(
            "vendored_crate",
            VENDORED_SOURCE,
            "",
            &[crate::testing::MOCK_KSPIN],
        );
    }
}
