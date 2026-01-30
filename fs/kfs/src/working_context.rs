// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Working directory context
//!
//! Lightweight state management for root and current working directory.

use fs_ng_vfs::{Location, VfsResult};

/// Working context - manages root and current working directory
///
/// This is a lightweight structure that only holds directory state.
/// It does not contain any path resolution or file operation logic.
#[derive(Debug, Clone)]
pub struct WorkingContext {
    root_dir: Location,
    current_dir: Location,
}

impl WorkingContext {
    /// Creates a new working context with the given root directory
    ///
    /// Initially, both root and current directory point to the same location.
    #[inline]
    pub fn new(root: Location) -> Self {
        Self {
            root_dir: root.clone(),
            current_dir: root,
        }
    }

    /// Returns a reference to the root directory
    #[inline]
    pub fn root(&self) -> &Location {
        &self.root_dir
    }

    /// Returns a reference to the current working directory
    #[inline]
    pub fn cwd(&self) -> &Location {
        &self.current_dir
    }

    /// Changes the current working directory
    ///
    /// # Errors
    /// Returns `NotADirectory` if the target is not a directory
    pub fn chdir(&mut self, dir: Location) -> VfsResult<()> {
        dir.check_is_dir()?;
        self.current_dir = dir;
        Ok(())
    }

    /// Creates a new context with a different current working directory
    ///
    /// This is an immutable version of `chdir` that returns a new context
    /// instead of modifying the existing one.
    ///
    /// # Errors
    /// Returns `NotADirectory` if the target is not a directory
    pub fn with_cwd(&self, dir: Location) -> VfsResult<Self> {
        dir.check_is_dir()?;
        Ok(Self {
            root_dir: self.root_dir.clone(),
            current_dir: dir,
        })
    }
}

#[cfg(test)]
mod tests {

    // Note: Full tests require actual filesystem, see tests/working_context_tests.rs

    #[test]
    fn test_working_context_clone() {
        // Basic clone test without filesystem
        // Real tests are in integration tests
    }
}
