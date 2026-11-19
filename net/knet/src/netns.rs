// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-network-namespace protocol-stack instances.
//!
//! Every network namespace id maps to one [`Service`] protocol stack. The
//! initial namespace owns the boot-time global stack — physical NICs, the
//! sampling timer and the socket registry all keep their existing global
//! wiring. A namespace created through `unshare(CLONE_NEWNET)` gets its own
//! stack holding only a loopback device with `127.0.0.1/8`, matching the
//! Linux initial namespace state; physical NIC ingress never reaches it.
//!
//! Scope note (layered bring-up): socket creation, netlink dumps and the
//! listen table still resolve through the global singletons regardless of the
//! caller's namespace. The lifecycle boundary — namespace creation, entering
//! through `setns(2)`, and `/proc/<pid>/ns/net` identity — is fully
//! namespace-aware, which is what libnetwork's default sandbox and
//! `runc`'s `setns` path require. Per-namespace data-plane routing is the
//! follow-up layer.

use alloc::{collections::BTreeMap, sync::Arc};

use kalloc_types::KBox;
use kerrno::{KError, KResult};
use ksync::{Mutex, static_lock};

use crate::{
    device::LoopbackDevice,
    ip::{Ipv4Address, Ipv4Cidr},
    network::{interface_address::Ipv4AddrEntry, router::ROUTE_SCOPE_HOST},
    router::Router,
    service::Service,
};

static_lock! {
/// Stacks of the non-initial network namespaces, keyed by namespace id.
///
/// The initial namespace's stack is the global `SERVICE` singleton and never
/// enters this map.
static NAMESPACE_STACKS: Mutex<BTreeMap<u64, Arc<Service>>> = Mutex::new(BTreeMap::new());
}

/// Reads the IPv4 forwarding switch of an installed namespace stack.
/// Requires sleepable task context without stack or namespace-registry locks.
/// Returns `NotFound` when the namespace has no installed stack.
pub fn ipv4_forwarding(namespace: &kns::NetNamespace) -> KResult<i32> {
    with_stack(namespace, Service::ipv4_forwarding)
}

/// Sets the IPv4 forwarding control; zero disables it, other values enable it.
///
/// The caller checks administrative permission. Requires sleepable task context
/// without stack or namespace-registry locks. Disabling discards queued forwards.
/// Returns `NotFound` when the namespace has no installed stack.
pub fn set_ipv4_forwarding(namespace: &kns::NetNamespace, value: i32) -> KResult<()> {
    with_stack(namespace, |service| service.set_ipv4_forwarding(value))
}

fn with_stack<R>(
    namespace: &kns::NetNamespace,
    operation: impl FnOnce(&Service) -> R,
) -> KResult<R> {
    let id = namespace.id().as_u64();
    if kns::NetNamespace::initial_id() == Some(id) {
        return Ok(operation(&crate::SERVICE));
    }
    let service = NAMESPACE_STACKS
        .lock()
        .get(&id)
        .cloned()
        .ok_or(KError::NotFound)?;
    Ok(operation(&service))
}

/// Drops the stack of a destroyed network namespace.
///
/// Registered as the `kns` drop hook so a namespace whose last reference is
/// released does not keep its whole protocol stack alive in the registry.
fn on_namespace_drop(id: u64) {
    let _ = NAMESPACE_STACKS.lock().remove(&id);
}

/// Materializes the protocol stack backing the namespace `id`.
///
/// The initial namespace's stack is the boot-time global service; a
/// non-initial namespace gets a private stack with a loopback device assigned
/// `127.0.0.1/8`. The private stack runs without the global sampling timer:
/// it carries no physical devices, and no data plane is routed into it at
/// this layer.
///
/// Idempotent: entering the same namespace again reuses its stack.
///
/// # Execution context
/// Requires reclaim-safe sleepable task context with no registry lock held.
///
/// # Errors
/// Owner allocation failure leaves the registry unchanged. The borrowed
/// namespace keeps its identity alive until publication completes.
pub fn ensure_stack(namespace: &kns::NetNamespace) -> KResult<()> {
    let id = namespace.id().as_u64();
    kns::set_net_namespace_drop_hook(on_namespace_drop);
    if kns::NetNamespace::initial_id() == Some(id) {
        // The boot namespace's stack is the global singleton created by
        // `init_network`.
        return Ok(());
    }
    ensure_stack_with(&NAMESPACE_STACKS, id, KBox::<LoopbackDevice>::new_uninit)
}

pub(crate) fn ensure_stack_with(
    stacks: &Mutex<BTreeMap<u64, Arc<Service>>>,
    id: u64,
    reserve: impl FnOnce() -> KResult<KBox<core::mem::MaybeUninit<LoopbackDevice>>>,
) -> KResult<()> {
    if stacks.lock().contains_key(&id) {
        return Ok(());
    }

    PreparedStack::new_with(reserve)?.install_into(stacks, id);
    Ok(())
}

/// Unpublished loopback-only stack prepared before a namespace transaction.
/// Dropping it abandons preparation without installing a registry entry.
pub struct PreparedStack {
    service: Arc<Service>,
}

impl PreparedStack {
    /// Prepares the device owner with KERNEL permissions outside registry locks.
    ///
    /// # Execution context
    /// Requires reclaim-safe sleepable task context without reclaim-dependent locks.
    ///
    /// # Errors
    /// Returns NoMemory before constructing the loopback device; nested
    /// allocations remain infallible.
    pub fn new() -> KResult<Self> {
        Self::new_with(KBox::<LoopbackDevice>::new_uninit)
    }

    fn new_with(
        reserve: impl FnOnce() -> KResult<KBox<core::mem::MaybeUninit<LoopbackDevice>>>,
    ) -> KResult<Self> {
        let storage = reserve()?;
        let mut router = Router::new();
        let loopback = KBox::write(storage, LoopbackDevice::new());
        let lo_dev = router
            .add_device(loopback)
            .expect("loopback interface ID must be available");

        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: the fixed loopback prefix length 8 is \
                      within the IPv4 range 0 through 32"
        )]
        let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8).unwrap();
        router
            .add_ipv4_addr(
                lo_dev,
                Ipv4AddrEntry {
                    addr: lo_ip,
                    scope: ROUTE_SCOPE_HOST,
                    broadcast: None,
                    no_prefix_route: false,
                    label: None,
                },
            )
            .expect("loopback IPv4 address must fit the interface");

        Ok(Self {
            service: Arc::new(Service::new(router)),
        })
    }

    /// Installs without another fallible owner allocation. An existing stack
    /// wins; the unused candidate is destroyed outside the registry lock.
    ///
    /// # Execution context
    /// Requires task context without registry locks. Registry growth retains
    /// its existing infallible allocation policy. The namespace borrow pins
    /// the identity throughout publication.
    pub fn install(self, namespace: &kns::NetNamespace) {
        kns::set_net_namespace_drop_hook(on_namespace_drop);
        let id = namespace.id().as_u64();
        if kns::NetNamespace::initial_id() != Some(id) {
            self.install_into(&NAMESPACE_STACKS, id);
        }
    }

    fn install_into(self, stacks: &Mutex<BTreeMap<u64, Arc<Service>>>, id: u64) {
        let candidate = self.service;
        {
            let mut stacks = stacks.lock();
            if let alloc::collections::btree_map::Entry::Vacant(entry) = stacks.entry(id) {
                entry.insert(candidate);
                return;
            }
        }
        drop(candidate);
    }
}
