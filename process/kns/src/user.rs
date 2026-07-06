// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User namespace (placeholder).

use alloc::sync::Arc;

use crate::types::NamespaceId;

/// User namespace.
///
/// Placeholder for future user namespace isolation.
pub struct UserNamespace {
    id: NamespaceId,
    _parent: Option<Arc<UserNamespace>>,
}

impl UserNamespace {
    /// Creates the initial (root) user namespace.
    pub fn new_root() -> Self {
        Self {
            id: NamespaceId::new(),
            _parent: None,
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }
}
