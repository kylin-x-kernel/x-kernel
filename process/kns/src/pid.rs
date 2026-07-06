// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PID namespace (placeholder).

use alloc::sync::Arc;

use crate::types::NamespaceId;

/// PID namespace.
///
/// Placeholder for future PID namespace isolation.
pub struct PidNamespace {
    id: NamespaceId,
    _parent: Option<Arc<PidNamespace>>,
}

impl PidNamespace {
    /// Creates the initial (root) PID namespace.
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
