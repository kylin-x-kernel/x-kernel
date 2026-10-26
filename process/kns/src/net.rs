// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace identity.
//!
//! [`NetNamespace`] carries the stable identity of one network namespace.
//! `knet` still resolves sockets through its global protocol stack, so
//! creating additional network namespaces is rejected until the data plane
//! has per-namespace instances; per-namespace stacks will key on
//! [`NetNamespace::id`] when they land.

use kcred::NamespaceId;

/// Network namespace.
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
    ///
    /// This is the `/proc/<pid>/ns/net` inode identity; when the network
    /// stack grows per-namespace data-plane instances, it will key those too.
    pub fn id(&self) -> NamespaceId {
        self.id
    }
}
