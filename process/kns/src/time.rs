// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time namespace (placeholder).

use crate::types::NamespaceId;

/// Time namespace.
///
/// Placeholder for future time namespace isolation.
pub struct TimeNamespace {
    id: NamespaceId,
}

impl Default for TimeNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeNamespace {
    /// Creates a new time namespace.
    pub fn new() -> Self {
        Self {
            id: NamespaceId::new(),
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }
}
