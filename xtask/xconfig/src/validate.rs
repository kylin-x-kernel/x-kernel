// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input validation helpers for paths that cross the xconfig trust boundary
//! (CLI arguments, environment variables, configuration file references).
//!
//! These checks harden the tool against malformed or traversal-bearing path
//! inputs (CWE-22). They intentionally allow absolute paths and `.` (current
//! directory) since those are legitimate for a host-side build tool, but reject
//! `..` (parent-directory) components so a configuration path cannot be made to
//! escape its intended location.

use std::path::{Component, Path};

use crate::error::{KconfigError, Result};

/// Validate a host-side input path supplied via the CLI, an environment
/// variable, or a configuration file reference.
///
/// Rejects empty paths, NUL bytes, non-UTF-8 paths, and any `..` (parent)
/// component. Absolute paths and `.` are allowed.
pub fn validate_input_path(path: &Path) -> Result<()> {
    let Some(text) = path.to_str() else {
        return Err(KconfigError::Config(format!(
            "path must be valid UTF-8: {}",
            path.display()
        )));
    };
    if text.is_empty() {
        return Err(KconfigError::Config("path must not be empty".to_string()));
    }
    if text.contains('\0') {
        return Err(KconfigError::Config(format!(
            "path must not contain NUL bytes: {}",
            path.display()
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(KconfigError::Config(format!(
            "path must not contain parent-directory (..) components: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::validate_input_path;

    #[test]
    fn accepts_clean_relative_and_absolute_paths() {
        assert!(validate_input_path(Path::new("Kconfig")).is_ok());
        assert!(validate_input_path(Path::new(".config")).is_ok());
        assert!(validate_input_path(Path::new(".")).is_ok());
        assert!(validate_input_path(Path::new("/etc/kbuild/Kconfig")).is_ok());
        assert!(validate_input_path(Path::new("sub/dir/config")).is_ok());
    }

    #[test]
    fn rejects_parent_directory_components() {
        assert!(validate_input_path(Path::new("../etc/passwd")).is_err());
        assert!(validate_input_path(Path::new("a/../../b")).is_err());
        assert!(validate_input_path(Path::new("foo/..")).is_err());
    }

    #[test]
    fn rejects_empty_paths() {
        assert!(validate_input_path(Path::new("")).is_err());
    }
}
