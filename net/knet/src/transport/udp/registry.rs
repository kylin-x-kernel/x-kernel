// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP port binding and management.
//!
//! Manages UDP socket bindings, bind-conflict checks, ephemeral-port
//! allocation, binding removal, and receive-side socket lookup.

use alloc::{sync::Arc, vec::Vec};

use ::core::net::{IpAddr, SocketAddr};
use hashbrown::HashMap;
use kerrno::{KResult, k_bail};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use super::{output::ipv4_to_core, pcb::UdpPcb, state::UdpSocketState};
use crate::ip::{IpAddress, IpEndpoint, IpListenEndpoint};

const UDP_REGISTRY_BUCKETS: usize = 256;
const UDP_EPHEMERAL_PORT_START: u16 = 0xc000;
const UDP_EPHEMERAL_PORT_END: u16 = 0xffff;
const UDP_EPHEMERAL_PORT_RANGE: u16 = UDP_EPHEMERAL_PORT_END - UDP_EPHEMERAL_PORT_START + 1;

static UDP_PCB_REGISTRY: LazyInit<UdpPcbRegistry> = LazyInit::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UdpBindKind {
    Auto,
    Explicit,
}

struct UdpBindRecord {
    pcb: Arc<UdpPcb>,
    local_endpoint: IpEndpoint,
    kind: UdpBindKind,
    reuse_address: bool,
}

struct UdpPcbBucket {
    by_port: HashMap<u16, Vec<UdpBindRecord>>,
}

struct UdpPcbRegistry {
    buckets: Vec<SpinNoIrq<UdpPcbBucket>>,
}

impl UdpPcbRegistry {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(UDP_REGISTRY_BUCKETS);
        for _ in 0..UDP_REGISTRY_BUCKETS {
            buckets.push(SpinNoIrq::new(UdpPcbBucket::new()));
        }
        Self { buckets }
    }

    #[cfg(unittest)]
    fn register(&self, pcb: Arc<UdpPcb>) {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .register(pcb, local_endpoint);
    }

    fn unregister(&self, pcb: &Arc<UdpPcb>) {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .unregister(pcb, local_endpoint);
    }

    fn lookup_map<T>(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
        map_pcb_fn: impl Fn(&Arc<UdpPcb>) -> T,
    ) -> Option<T> {
        self.bucket(local_addr.port()).lock().lookup_in_bucket(
            local_addr,
            remote_addr,
            ifindex,
            map_pcb_fn,
        )
    }

    fn lookup(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
    ) -> Option<Arc<UdpPcb>> {
        self.lookup_map(local_addr, remote_addr, ifindex, |pcb| pcb.clone())
    }

    fn lookup_state(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
    ) -> Option<Arc<UdpSocketState>> {
        self.lookup_map(local_addr, remote_addr, ifindex, |pcb| pcb.state.clone())
    }

    fn bind(
        &self,
        pcb: Arc<UdpPcb>,
        endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult {
        self.bucket(endpoint.port)
            .lock()
            .bind(pcb.clone(), endpoint, kind, reuse_address)?;
        // `set_local_endpoint` takes a sleepable lock; keep it outside the
        // IRQ-safe bucket used by `NetRx` lookup.
        pcb.state.set_local_endpoint(Some(endpoint));
        Ok(())
    }

    fn bind_ephemeral(
        &self,
        pcb: Arc<UdpPcb>,
        local_addr: IpAddress,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult<IpEndpoint> {
        for port in self.ephemeral_port_iter()? {
            let endpoint = IpEndpoint {
                addr: local_addr,
                port,
            };
            let mut bucket = self.bucket(endpoint.port).lock();
            if !bucket.contains_bind_conflict(listen_endpoint(endpoint), reuse_address) {
                bucket.bind(pcb.clone(), endpoint, kind, reuse_address)?;
                drop(bucket);
                pcb.state.set_local_endpoint(Some(endpoint));
                return Ok(endpoint);
            }
        }

        k_bail!(AddrInUse, "no available ports");
    }

    #[cfg(unittest)]
    fn contains_bind_conflict(&self, endpoint: IpListenEndpoint, reuse_address: bool) -> bool {
        self.bucket(endpoint.port)
            .lock()
            .contains_bind_conflict(endpoint, reuse_address)
    }

    #[cfg(unittest)]
    fn clear(&self) {
        for bucket in &self.buckets {
            bucket.lock().clear();
        }
    }

    fn is_explicitly_bound(&self, pcb: &Arc<UdpPcb>) -> bool {
        let Some(local_endpoint) = pcb.state.local_endpoint() else {
            return false;
        };
        self.bucket(local_endpoint.port)
            .lock()
            .is_explicitly_bound(pcb, local_endpoint.port)
    }

    fn bucket(&self, port: u16) -> &SpinNoIrq<UdpPcbBucket> {
        &self.buckets[bucket_index_for_port(port)]
    }

    /// Requires sleepable task context; entropy sources may perform I/O.
    /// Returns `EAGAIN` before publishing a bind if quality entropy is unavailable.
    fn ephemeral_port_iter(&self) -> KResult<UdpEphemeralPortIter> {
        // Bootstrap output can be derived from ticks alone. Only use output
        // after the pool has credited a trusted entropy source.
        entropy::try_seed_from_hardware();
        if !entropy::is_ready() {
            return Err(kerrno::KError::WouldBlock);
        }
        let mut bytes = [0u8; 4];
        entropy::fill_random(&mut bytes);
        Ok(UdpEphemeralPortIter::new(u32::from_le_bytes(bytes)))
    }
}

impl UdpPcbBucket {
    fn new() -> Self {
        Self {
            by_port: HashMap::new(),
        }
    }

    #[cfg(unittest)]
    fn register(&mut self, pcb: Arc<UdpPcb>, local_endpoint: IpEndpoint) {
        if self.contains(&pcb, local_endpoint.port) {
            return;
        };
        self.insert(pcb, local_endpoint, UdpBindKind::Explicit, false);
    }

    fn unregister(&mut self, pcb: &Arc<UdpPcb>, local_endpoint: IpEndpoint) {
        let port = local_endpoint.port;
        let should_remove_port = if let Some(records) = self.by_port.get_mut(&port) {
            records.retain(|record| !Arc::ptr_eq(&record.pcb, pcb));
            records.is_empty()
        } else {
            false
        };
        if should_remove_port {
            self.by_port.remove(&port);
        }
    }

    fn bind(
        &mut self,
        pcb: Arc<UdpPcb>,
        endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) -> KResult {
        if self.contains(&pcb, endpoint.port) {
            return Ok(());
        }

        let listen_endpoint = listen_endpoint(endpoint);
        if self.contains_bind_conflict(listen_endpoint, reuse_address) {
            return Err(kerrno::KError::AddrInUse);
        }

        self.insert(pcb, endpoint, kind, reuse_address);
        Ok(())
    }

    fn insert(
        &mut self,
        pcb: Arc<UdpPcb>,
        local_endpoint: IpEndpoint,
        kind: UdpBindKind,
        reuse_address: bool,
    ) {
        self.by_port
            .entry(local_endpoint.port)
            .or_default()
            .push(UdpBindRecord {
                pcb,
                local_endpoint,
                kind,
                reuse_address,
            });
    }

    fn contains(&self, pcb: &Arc<UdpPcb>, port: u16) -> bool {
        self.by_port
            .get(&port)
            .is_some_and(|records| records.iter().any(|record| Arc::ptr_eq(&record.pcb, pcb)))
    }

    fn is_explicitly_bound(&self, pcb: &Arc<UdpPcb>, port: u16) -> bool {
        self.by_port.get(&port).is_some_and(|records| {
            records
                .iter()
                .any(|record| Arc::ptr_eq(&record.pcb, pcb) && record.kind == UdpBindKind::Explicit)
        })
    }

    fn lookup_in_bucket<T>(
        &self,
        local_addr: SocketAddr,
        remote_addr: SocketAddr,
        ifindex: i32,
        map_pcb_fn: impl Fn(&Arc<UdpPcb>) -> T,
    ) -> Option<T> {
        let records = self.by_port.get(&local_addr.port())?;

        let mut selected = None;
        let mut selected_score = 0;
        for record in records {
            let local_endpoint = record.local_endpoint;
            if !local_endpoint_matches(local_endpoint, local_addr) {
                continue;
            }
            if !bound_device_matches(record.pcb.state.bound_dev_if(), ifindex) {
                continue;
            }

            let Some(score) = delivery_score(&record.pcb.state, local_endpoint, remote_addr) else {
                continue;
            };
            if score > selected_score {
                selected_score = score;
                selected = Some(map_pcb_fn(&record.pcb));
            }
        }
        selected
    }

    fn contains_bind_conflict(&self, endpoint: IpListenEndpoint, reuse_address: bool) -> bool {
        let Some(records) = self.by_port.get(&endpoint.port) else {
            return false;
        };

        records.iter().any(|record| {
            let existing_endpoint = listen_endpoint(record.local_endpoint);
            udp_bind_entries_conflict(
                existing_endpoint,
                record.reuse_address,
                endpoint,
                reuse_address,
            )
        })
    }

    #[cfg(unittest)]
    fn clear(&mut self) {
        self.by_port.clear();
    }
}

struct UdpEphemeralPortIter {
    start: u16,
    step: u16,
    tries: u16,
}

impl UdpEphemeralPortIter {
    fn new(rand: u32) -> Self {
        Self {
            start: (rand as u16) & (UDP_EPHEMERAL_PORT_RANGE - 1),
            step: (((rand >> 16) as u16) & (UDP_EPHEMERAL_PORT_RANGE - 1)) | 1,
            tries: 0,
        }
    }
}

impl Iterator for UdpEphemeralPortIter {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        if self.tries >= UDP_EPHEMERAL_PORT_RANGE {
            return None;
        }

        let offset = self.start.wrapping_add(self.tries.wrapping_mul(self.step))
            & (UDP_EPHEMERAL_PORT_RANGE - 1);
        self.tries += 1;

        Some(UDP_EPHEMERAL_PORT_START + offset)
    }
}

pub(crate) fn init_udp_registry() {
    UDP_PCB_REGISTRY.call_once(UdpPcbRegistry::new);
}

#[cfg(unittest)]
pub(super) fn register_udp_pcb(pcb: Arc<UdpPcb>) {
    init_udp_registry();
    UDP_PCB_REGISTRY.register(pcb);
}

pub(super) fn bind_udp_pcb(pcb: Arc<UdpPcb>, endpoint: IpEndpoint, reuse_address: bool) -> KResult {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind(pcb, endpoint, UdpBindKind::Explicit, reuse_address)
}

#[cfg(unittest)]
pub(super) fn bind_udp_auto_pcb_for_test(
    pcb: Arc<UdpPcb>,
    endpoint: IpEndpoint,
    reuse_address: bool,
) -> KResult {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind(pcb, endpoint, UdpBindKind::Auto, reuse_address)
}

pub(super) fn bind_udp_auto_ephemeral_pcb(
    pcb: Arc<UdpPcb>,
    local_addr: IpAddress,
    reuse_address: bool,
) -> KResult<IpEndpoint> {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind_ephemeral(pcb, local_addr, UdpBindKind::Auto, reuse_address)
}

pub(super) fn bind_udp_explicit_ephemeral_pcb(
    pcb: Arc<UdpPcb>,
    local_addr: IpAddress,
    reuse_address: bool,
) -> KResult<IpEndpoint> {
    init_udp_registry();
    UDP_PCB_REGISTRY.bind_ephemeral(pcb, local_addr, UdpBindKind::Explicit, reuse_address)
}

pub(super) fn unregister_udp_pcb(pcb: &Arc<UdpPcb>) {
    if !UDP_PCB_REGISTRY.is_inited() {
        return;
    }
    UDP_PCB_REGISTRY.unregister(pcb);
}

pub(super) fn is_udp_pcb_explicitly_bound(pcb: &Arc<UdpPcb>) -> bool {
    if !UDP_PCB_REGISTRY.is_inited() {
        return false;
    }
    UDP_PCB_REGISTRY.is_explicitly_bound(pcb)
}

#[cfg(unittest)]
pub(crate) fn clear_udp_registry_for_test() {
    init_udp_registry();
    UDP_PCB_REGISTRY.clear();
}

#[cfg(unittest)]
pub(crate) fn register_udp_state_for_test(state: Arc<UdpSocketState>) {
    register_udp_pcb(UdpPcb::new(state));
}

#[cfg(unittest)]
pub(super) fn lookup_udp_pcb(
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
) -> Option<Arc<UdpPcb>> {
    lookup_udp_pcb_on_device(local_addr, remote_addr, 0)
}

pub(super) fn lookup_udp_pcb_on_device(
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    ifindex: i32,
) -> Option<Arc<UdpPcb>> {
    init_udp_registry();
    UDP_PCB_REGISTRY.lookup(local_addr, remote_addr, ifindex)
}

pub(crate) fn lookup_udp_error_state(
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    ifindex: i32,
) -> Option<Arc<UdpSocketState>> {
    init_udp_registry();
    UDP_PCB_REGISTRY.lookup_state(local_addr, remote_addr, ifindex)
}

fn bound_device_matches(bound_dev_if: i32, packet_ifindex: i32) -> bool {
    bound_dev_if == 0 || bound_dev_if == packet_ifindex
}

fn bucket_index_for_port(port: u16) -> usize {
    let port = port as usize;
    (port ^ (port >> 8)) & (UDP_REGISTRY_BUCKETS - 1)
}

fn delivery_score(
    state: &UdpSocketState,
    local_endpoint: IpEndpoint,
    remote_addr: SocketAddr,
) -> Option<u8> {
    let is_specific_local = !local_endpoint.addr.is_unspecified();
    match state.peer_endpoint() {
        Some((peer, _)) if endpoint_matches_addr(peer, remote_addr) => {
            Some(if is_specific_local { 4 } else { 3 })
        }
        Some(_) => None,
        None => Some(if is_specific_local { 2 } else { 1 }),
    }
}

fn local_endpoint_matches(endpoint: IpEndpoint, addr: SocketAddr) -> bool {
    endpoint.port == addr.port()
        && (endpoint.addr.is_unspecified() || endpoint_addr_matches(endpoint.addr, addr.ip()))
}

pub(super) fn endpoint_matches_addr(endpoint: IpEndpoint, addr: SocketAddr) -> bool {
    endpoint.port == addr.port() && endpoint_addr_matches(endpoint.addr, addr.ip())
}

fn endpoint_addr_matches(endpoint_addr: IpAddress, addr: IpAddr) -> bool {
    match (endpoint_addr, addr) {
        (IpAddress::Ipv4(endpoint), IpAddr::V4(addr)) => ipv4_to_core(endpoint) == addr,
        (IpAddress::Ipv6(endpoint), IpAddr::V6(addr)) => endpoint.octets() == addr.octets(),
        _ => false,
    }
}

pub(super) fn listen_endpoint(endpoint: IpEndpoint) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: (!endpoint.addr.is_unspecified()).then_some(endpoint.addr),
        port: endpoint.port,
    }
}

#[cfg(unittest)]
pub(super) fn udp_port_available(endpoint: IpListenEndpoint) -> bool {
    init_udp_registry();
    !UDP_PCB_REGISTRY.contains_bind_conflict(endpoint, false)
}

fn udp_bind_entries_conflict(
    existing_endpoint: IpListenEndpoint,
    existing_reuse_address: bool,
    new_endpoint: IpListenEndpoint,
    new_reuse_address: bool,
) -> bool {
    let same_bind_scope = listen_addrs_overlap(existing_endpoint.addr, new_endpoint.addr);
    let both_allow_reuse = existing_reuse_address && new_reuse_address;

    same_bind_scope && !both_allow_reuse
}

fn listen_addrs_overlap(a: Option<IpAddress>, b: Option<IpAddress>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/transport/udp/registry/tests.rs");
