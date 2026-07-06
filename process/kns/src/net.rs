// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace (placeholder).

use crate::types::NamespaceId;

/// Network namespace.
///
/// Placeholder for future network namespace isolation.
pub struct NetNamespace {
    id: NamespaceId,
}

impl Default for NetNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl NetNamespace {
    /// Creates a new network namespace.
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
