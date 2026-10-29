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

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

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
pub fn ensure_stack(id: u64) {
    kns::set_net_namespace_drop_hook(on_namespace_drop);
    if kns::NetNamespace::initial_id() == Some(id) {
        // The boot namespace's stack is the global singleton created by
        // `init_network`.
        return;
    }
    let mut stacks = NAMESPACE_STACKS.lock();
    if stacks.contains_key(&id) {
        return;
    }

    let mut router = Router::new();
    let lo_dev = router
        .add_device(Box::new(LoopbackDevice::new()))
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
            },
        )
        .expect("loopback IPv4 address must fit the interface");

    stacks.insert(id, Arc::new(Service::new(router)));
}
