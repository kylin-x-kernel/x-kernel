// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use ::core::net::{IpAddr, SocketAddr};
use hashbrown::HashMap;
use kerrno::{KResult, k_bail};
use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ksync::Mutex;
use lazyinit::LazyInit;

use super::{output::ipv4_to_core, pcb::UdpPcb, state::UdpSocketState};
use crate::ip::{IpAddress, IpEndpoint, IpListenEndpoint};

const UDP_REGISTRY_BUCKETS: usize = 256;
const UDP_EPHEMERAL_PORT_START: u16 = 0xc000;
const UDP_EPHEMERAL_PORT_END: u16 = 0xffff;
const UDP_EPHEMERAL_PORT_RANGE: u16 = UDP_EPHEMERAL_PORT_END - UDP_EPHEMERAL_PORT_START + 1;

static UDP_PCB_REGISTRY: LazyInit<UdpPcbRegistry> = LazyInit::new();

struct UdpPcbRegistry {
    buckets: Vec<SpinNoIrq<UdpPcbBucket>>,
    port_rand: Mutex<UdpPortRand>,
}

struct UdpPcbBucket {
    by_port: HashMap<u16, Vec<UdpBindRecord>>,
}

struct UdpBindRecord {
    pcb: Arc<UdpPcb>,
    local_endpoint: IpEndpoint,
    kind: UdpBindKind,
    reuse_address: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UdpBindKind {
    Auto,
    Explicit,
}

struct UdpPortRand {
    state: u64,
}

struct UdpEphemeralPortIter {
    start: u16,
    step: u16,
    tries: u16,
}

impl UdpPcbRegistry {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(UDP_REGISTRY_BUCKETS);
        for _ in 0..UDP_REGISTRY_BUCKETS {
            buckets.push(SpinNoIrq::new(UdpPcbBucket::new()));
        }
        Self {
            buckets,
            port_rand: Mutex::new(UdpPortRand::new(monotonic_time().as_nanos_u64_saturating())),
        }
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
        for port in self.ephemeral_port_iter() {
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

    fn ephemeral_port_iter(&self) -> UdpEphemeralPortIter {
        let rand = self.port_rand.lock().rand_u32();
        UdpEphemeralPortIter::new(rand)
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

impl UdpPortRand {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn rand_u32(&mut self) -> u32 {
        // sPCG32 keeps a 64-bit linear-congruential state and
        // derives the output with a data-dependent shift.
        const M: u64 = 0xbb2e_fcec_3c39_611d;
        const A: u64 = 0x7590_ef39;

        let state = self.state.wrapping_mul(M).wrapping_add(A);
        self.state = state;

        let shift = 29 - (state >> 61);
        (state >> shift) as u32
    }
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

#[cfg(unittest)]
mod tests {
    use alloc::{sync::Arc, vec};

    use ::core::net::{SocketAddr, SocketAddrV4};
    use unittest::def_test;

    use super::*;
    use crate::ip::Ipv4Address;

    fn endpoint(addr: Ipv4Address, port: u16) -> IpEndpoint {
        SocketAddrV4::new(ipv4_to_core(addr), port).into()
    }

    fn socket_addr(addr: Ipv4Address, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(ipv4_to_core(addr), port))
    }

    fn clear_registry() {
        clear_udp_registry_for_test();
    }

    fn bind_test_pcb(local: IpEndpoint, reuse_address: bool) -> Arc<UdpPcb> {
        let pcb = UdpPcb::new(UdpSocketState::new());
        bind_udp_pcb(pcb.clone(), local, reuse_address).unwrap();
        pcb
    }

    #[def_test]
    fn bucket_index_uses_high_port_bits() {
        let first = 40000;
        let second = first + UDP_REGISTRY_BUCKETS as u16;

        assert_ne!(bucket_index_for_port(first), bucket_index_for_port(second));
    }

    #[def_test]
    fn ephemeral_port_iter_visits_every_candidate_once() {
        let mut seen = vec![false; UDP_EPHEMERAL_PORT_RANGE as usize];
        let mut count = 0;

        for port in UdpEphemeralPortIter::new(0x1234_5679) {
            assert!((UDP_EPHEMERAL_PORT_START..=UDP_EPHEMERAL_PORT_END).contains(&port));
            let index = (port - UDP_EPHEMERAL_PORT_START) as usize;
            assert!(!seen[index]);
            seen[index] = true;
            count += 1;
        }

        assert_eq!(count, UDP_EPHEMERAL_PORT_RANGE as usize);
    }

    #[def_test(serial)]
    fn bind_registry_rejects_wildcard_conflict() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::UNSPECIFIED, 8080), false);
        assert!(
            bind_udp_pcb(
                UdpPcb::new(UdpSocketState::new()),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8080),
                false
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn reuse_bind_is_recorded_for_later_nonreuse_conflict() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8081), true);
        assert!(
            bind_udp_pcb(
                UdpPcb::new(UdpSocketState::new()),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
                true
            )
            .is_ok()
        );
        assert!(
            bind_udp_pcb(
                UdpPcb::new(UdpSocketState::new()),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8081),
                false
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn reuse_bind_rejects_reuse_after_nonreuse() {
        clear_registry();

        bind_test_pcb(endpoint(Ipv4Address::new(10, 0, 0, 2), 8082), false);
        assert!(
            bind_udp_pcb(
                UdpPcb::new(UdpSocketState::new()),
                endpoint(Ipv4Address::new(10, 0, 0, 2), 8082),
                true
            )
            .is_err()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn ephemeral_bind_assigns_distinct_registered_ports() {
        clear_registry();

        let first = bind_udp_auto_ephemeral_pcb(
            UdpPcb::new(UdpSocketState::new()),
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            false,
        )
        .unwrap();
        let second = bind_udp_auto_ephemeral_pcb(
            UdpPcb::new(UdpSocketState::new()),
            IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            false,
        )
        .unwrap();

        assert_ne!(first.port, second.port);

        clear_registry();
    }

    #[def_test(serial)]
    fn connected_pcb_takes_lookup_precedence() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8080);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

        let bound_state = UdpSocketState::new();
        bound_state.set_local_endpoint(Some(local));
        let bound = UdpPcb::new(bound_state);
        register_udp_pcb(bound);

        let connected_state = UdpSocketState::new();
        connected_state.set_local_endpoint(Some(local));
        connected_state.set_peer_endpoint(Some((remote, local.addr)));
        let connected = UdpPcb::new(connected_state);
        register_udp_pcb(connected.clone());

        let selected = lookup_udp_pcb(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8080),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
        )
        .expect("connected PCB should match");

        assert!(Arc::ptr_eq(&selected, &connected));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_scans_only_matching_local_port_bucket() {
        clear_registry();

        let first_state = UdpSocketState::new();
        first_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 8080)));
        let first = UdpPcb::new(first_state);
        register_udp_pcb(first);

        let second_state = UdpSocketState::new();
        second_state.set_local_endpoint(Some(endpoint(Ipv4Address::new(10, 0, 0, 2), 5353)));
        let second = UdpPcb::new(second_state);
        register_udp_pcb(second.clone());

        let selected = lookup_udp_pcb(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 5353),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152),
        )
        .expect("same-port UDP PCB should match");

        assert!(Arc::ptr_eq(&selected, &second));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_scores_specific_and_wildcard_addrs() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8083);
        let wildcard = endpoint(Ipv4Address::UNSPECIFIED, 8083);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);

        let specific_state = UdpSocketState::new();
        specific_state.set_local_endpoint(Some(local));
        let specific = UdpPcb::new(specific_state);
        register_udp_pcb(specific);

        let wildcard_state = UdpSocketState::new();
        wildcard_state.set_local_endpoint(Some(wildcard));
        wildcard_state.set_peer_endpoint(Some((remote, wildcard.addr)));
        let wildcard_connected = UdpPcb::new(wildcard_state);
        register_udp_pcb(wildcard_connected.clone());

        let selected = lookup_udp_pcb(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8083),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353),
        )
        .expect("wildcard connected PCB should match");

        assert!(Arc::ptr_eq(&selected, &wildcard_connected));
        clear_registry();
    }

    #[def_test(serial)]
    fn lookup_skips_pcb_bound_to_another_device() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8068);
        let bound_state = UdpSocketState::new();
        bound_state.set_local_endpoint(Some(local));
        bound_state.set_bound_dev_if_for_test(2);
        let bound = UdpPcb::new(bound_state);
        register_udp_pcb(bound.clone());

        assert!(
            lookup_udp_pcb_on_device(
                socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
                socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
                1,
            )
            .is_none()
        );
        let selected = lookup_udp_pcb_on_device(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 8068),
            socket_addr(Ipv4Address::new(192, 0, 2, 1), 67),
            2,
        )
        .expect("bound PCB should match its device");
        assert!(Arc::ptr_eq(&selected, &bound));
        clear_registry();
    }

    #[def_test(serial)]
    fn unregister_removes_pcb_and_releases_port() {
        clear_registry();

        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 8084);
        let pcb = bind_test_pcb(local, false);

        unregister_udp_pcb(&pcb);

        assert!(
            lookup_udp_pcb(
                socket_addr(Ipv4Address::new(10, 0, 0, 2), 8084),
                socket_addr(Ipv4Address::new(192, 0, 2, 1), 49152)
            )
            .is_none()
        );
        assert!(udp_port_available(listen_endpoint(local)));
        clear_registry();
    }
}
