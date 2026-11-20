// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace identity.
//!
//! [`NetNamespace`] carries the stable identity of one network namespace.
//! The namespace's protocol-stack instance lives in `knet` and is keyed by
//! [`NetNamespace::id`]; [`Drop`] notifies that owner through
//! [`set_net_namespace_drop_hook`] so a destroyed namespace releases its
//! stack. Socket routing still resolves through the global stack at this
//! layer; per-namespace data-plane routing is the follow-up layer.

use alloc::sync::Arc;

use kcred::{NamespaceId, UserNamespace};
use ksync::{Mutex, static_lock};

static_lock! {
/// Drop notification hook, installed by the network-stack owner (`knet`) so a
/// destroyed namespace can release its protocol-stack resources.
static NET_NAMESPACE_DROP_HOOK: Mutex<Option<fn(u64)>> = Mutex::new(None);
}

static_lock! {
/// The id of the initial (boot) network namespace, recorded by
/// [`NetNamespace::new_initial`] when the boot `NsProxy` is constructed.
///
/// [`NamespaceId`] is a kernel-wide counter shared by every namespace kind, so
/// the initial network namespace's id is whatever the counter holds at
/// boot-time construction — not a fixed constant.
static INITIAL_NET_ID: Mutex<Option<u64>> = Mutex::new(None);
}

/// Installs the drop notification hook for network namespaces.
pub fn set_net_namespace_drop_hook(hook: fn(u64)) {
    *NET_NAMESPACE_DROP_HOOK.lock() = Some(hook);
}

/// Network namespace.
pub struct NetNamespace {
    id: NamespaceId,
    owner: Arc<UserNamespace>,
}

impl Default for NetNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NetNamespace {
    fn drop(&mut self) {
        let hook = NET_NAMESPACE_DROP_HOOK.lock().as_ref().copied();
        if let Some(hook) = hook {
            hook(self.id.as_u64());
        }
    }
}

impl NetNamespace {
    /// Creates a new network namespace owned by the initial user namespace.
    pub fn new() -> Self {
        Self::new_in(kcred::initial_user_namespace())
    }

    /// Creates a new network namespace owned by `owner`.
    pub fn new_in(owner: Arc<UserNamespace>) -> Self {
        Self {
            id: NamespaceId::new(),
            owner,
        }
    }

    /// Creates the initial (boot) network namespace and records its id.
    ///
    /// Called exactly once from `NsProxy::new_initial*`.
    pub fn new_initial() -> Self {
        let initial = Self::new_in(kcred::initial_user_namespace());
        *INITIAL_NET_ID.lock() = Some(initial.id.as_u64());
        initial
    }

    /// Returns the initial (boot) network namespace's id, if it has been
    /// constructed yet.
    pub fn initial_id() -> Option<u64> {
        *INITIAL_NET_ID.lock()
    }

    /// Returns the namespace ID.
    ///
    /// This is both the `/proc/<pid>/ns/net` inode identity and the key the
    /// network stack registry (`knet::netns`) uses to locate the namespace's
    /// protocol-stack instance.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the user namespace that owns this network namespace.
    pub fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }
}
