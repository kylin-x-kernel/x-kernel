// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPC namespace.

use kcred::NamespaceId;

/// IPC namespace.
///
/// In the first phase this is a placeholder that only carries an ID.
/// Full migration of message queue and shared memory managers will follow.
pub struct IpcNamespace {
    id: NamespaceId,
}

impl Default for IpcNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl IpcNamespace {
    /// Creates a new IPC namespace.
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
