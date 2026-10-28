// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPC namespace.

use alloc::sync::Arc;

use kcred::NamespaceId;
use ksync::{Mutex, static_lock};

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

static_lock! {
/// Drop observers keyed by namespace id: at most one per namespace, taken by
/// the owner of a namespace-scoped resource that must die with the namespace
/// (the mqueue superblock plays Linux's `ns->mq_mnt` role this way).
static DROP_HOOKS: Mutex<alloc::collections::BTreeMap<u64, Arc<dyn Fn() + Send + Sync>>> =
    Mutex::new(alloc::collections::BTreeMap::new());
}

/// Registers `callback` as the drop observer of the namespace with `id`.
///
/// The callback runs once, when the last strong reference to that namespace
/// is released; registration fails while a previous callback is still
/// registered. The callback runs outside the registry lock and must not
/// re-enter [`register_ipc_ns_drop_hook`].
///
/// # Errors
///
/// An `ArgumentListTooLong`-shaped conflict is reported as
/// [`KError::AlreadyExists`] when a hook is already registered for `id`.
pub fn register_ipc_ns_drop_hook(
    id: NamespaceId,
    callback: Arc<dyn Fn() + Send + Sync>,
) -> kerrno::KResult<()> {
    let mut hooks = DROP_HOOKS.lock();
    if hooks.contains_key(&id.as_u64()) {
        return Err(kerrno::KError::AlreadyExists);
    }
    hooks.insert(id.as_u64(), callback);
    Ok(())
}

impl Drop for IpcNamespace {
    fn drop(&mut self) {
        // Take the observer out first so it cannot re-register, then run it
        // outside the registry lock.
        let callback = DROP_HOOKS.lock().remove(&self.id.as_u64());
        if let Some(callback) = callback {
            callback();
        }
    }
}

unittest::path_mod_test!(tests_ipc, "../ut-tests/ipc.rs");
