// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned pathname object used by VFS pathname lookup.

use crate::path::{PathBuf, Pathname};

/// A pathname object owned by VFS namei callers.
///
/// This corresponds to Linux `struct filename`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Filename {
    name: PathBuf,
}

impl Filename {
    /// Creates an owned pathname object.
    pub fn new(name: impl Into<PathBuf>) -> Self {
        Self { name: name.into() }
    }

    /// Borrows this filename as a pathname view.
    pub fn as_pathname(&self) -> Pathname<'_> {
        self.name.as_pathname()
    }
}
