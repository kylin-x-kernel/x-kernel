// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount namespace.

use alloc::sync::Arc;

use kfs::FsContext;
use ksync::Mutex;

use crate::types::NamespaceId;

/// Mount namespace.
///
/// Wraps an `FsContext` that holds root/current directory and mount tree state.
/// In the first phase, only `FsContext` ownership is isolated; mount tree
/// modifications are still shared between cloned namespaces.
pub struct MntNamespace {
    id: NamespaceId,
    fs_context: Arc<Mutex<FsContext>>,
}

impl MntNamespace {
    /// Creates a new mount namespace wrapping the given `FsContext`.
    pub fn new(fs_context: Arc<Mutex<FsContext>>) -> Self {
        Self {
            id: NamespaceId::new(),
            fs_context,
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns a reference to the underlying `FsContext`.
    pub fn fs_context(&self) -> &Arc<Mutex<FsContext>> {
        &self.fs_context
    }
}
