// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service wrapper around smoltcp interface.
use alloc::{boxed::Box, sync::Arc};
use core::{
    pin::Pin,
    task::{Context, Waker},
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::{NANOS_PER_MICROS, TimeValue, monotonic_time, monotonic_time_nanos};
use kpoll::PollSet;
use ktask::future::sleep;
use smoltcp::{
    iface::{Interface, SocketSet},
    time::Instant,
    wire::{
        HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr as SmoltcpIpCidr,
        IpListenEndpoint as SmoltcpIpListenEndpoint,
    },
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    ip::{IpAddress, IpListenEndpoint},
    netlink::RtnetlinkState,
    router::Router,
};

const IPV4_HEADER_LEN: usize = 20;

fn now() -> Instant {
    Instant::from_micros_const((monotonic_time_nanos() / NANOS_PER_MICROS) as i64)
}

pub struct Service {
    pub iface: Interface,
    router: Router,
    timeout: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    timeout_deadline: Option<TimeValue>,
    timeout_poll: Arc<PollSet>,
}
impl Service {
    pub fn new(mut router: Router) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, now());

        Self {
            iface,
            router,
            timeout: None,
            timeout_deadline: None,
            timeout_poll: Arc::new(PollSet::new()),
        }
    }

    pub fn poll(&mut self, sockets: &mut SocketSet) -> bool {
        let smoltcp_timestamp = now();
        let device_timestamp = monotonic_time();

        self.router.poll(device_timestamp, sockets);
        self.iface
            .poll(smoltcp_timestamp, &mut self.router, sockets);
        LISTEN_TABLE.wake_touched_acceptors(sockets);
        self.router.dispatch(device_timestamp)
    }

    pub fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .table
            .lookup(&dst_addr)
            .map(|rule| super::to_smoltcp_ip_address(rule.src))
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn smoltcp_device_mask_for(&self, endpoint: &SmoltcpIpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.smoltcp_device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn smoltcp_device_mask_for_addr(&self, addr: &SmoltcpIpAddress) -> u32 {
        let addr = super::from_smoltcp_ip_address(*addr);
        self.router
            .table
            .lookup(&addr)
            .map_or(u32::MAX, |it| 1u32 << it.dev)
    }

    pub fn get_source_address(&self, dst_addr: &IpAddress) -> KResult<IpAddress> {
        self.router
            .table
            .lookup(dst_addr)
            .map(|rule| rule.src)
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn can_send_ip_packet(&self) -> bool {
        self.router.can_enqueue_tx_packet()
    }

    pub fn prepare_ipv4_packet_send(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        packet_len: usize,
    ) -> KResult<IpAddress> {
        if !self.router.can_enqueue_tx_packet() {
            return Err(KError::WouldBlock);
        }

        let source_addr = match bound_src {
            Some(addr) => addr,
            None => self
                .router
                .table
                .lookup(dst_addr)
                .map(|rule| rule.src)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?,
        };
        let packet_count = output_ipv4_packet_count(&self.router, dst_addr, packet_len)?;
        if !self.router.can_enqueue_tx_packets(packet_count) {
            return Err(KError::WouldBlock);
        }

        Ok(source_addr)
    }

    pub fn send_ipv4_packet(&mut self, packet: alloc::vec::Vec<u8>) -> KResult {
        self.router.queue_ipv4_packet(packet)
    }

    pub fn ipv4_route_mtu(&self, dst_addr: &IpAddress) -> Option<usize> {
        self.router.route_mtu(dst_addr)
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn device_mask_for_addr(&self, addr: &IpAddress) -> u32 {
        self.router
            .table
            .lookup(addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn register_rx_waker(&mut self, mask: u32, waker: &Waker) {
        self.timeout_poll.register(waker);

        let current = now();
        let next = self.iface.poll_at(current, &SOCKET_SET.inner.lock());

        if let Some(t) = next {
            let delay_micros = t.total_micros().saturating_sub(current.total_micros()) as u64;
            let delay = core::time::Duration::from_micros(delay_micros);
            let deadline = monotonic_time() + delay;

            let should_reset = match self.timeout_deadline {
                None => true,
                Some(old_deadline) => monotonic_time() >= old_deadline || deadline < old_deadline,
            };

            if should_reset {
                self.timeout = None;
                self.timeout_deadline = Some(deadline);

                let mut fut = Box::pin(sleep(delay));
                let wake = Waker::from(self.timeout_poll.clone());
                let mut cx = Context::from_waker(&wake);

                if fut.as_mut().poll(&mut cx).is_ready() {
                    self.timeout_deadline = None;
                    self.timeout_poll.wake();
                    return;
                } else {
                    self.timeout = Some(fut);
                }
            }
        } else {
            self.timeout = None;
            self.timeout_deadline = None;
        }

        for (i, device) in self.router.devices.iter().enumerate() {
            if mask & (1 << i) != 0 {
                device.register_rx_waker(waker);
            }
        }
    }

    pub fn sync_netlink(&mut self, state: &RtnetlinkState) {
        self.router.sync_netlink(state);
        self.iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.clear();
            for addr in &state.addrs {
                if let SmoltcpIpAddress::Ipv4(ipv4) = addr.address {
                    let _ = ip_addrs.push(SmoltcpIpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                        ipv4,
                        addr.prefix_len,
                    )));
                }
            }
        });
    }

    pub fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> bool {
        self.router.remove_device_by_model_id(id)
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.router.send_link_frame(ifindex, frame)
    }
}

fn output_ipv4_packet_count(
    router: &Router,
    dst_addr: &IpAddress,
    packet_len: usize,
) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = router.route_mtu(dst_addr) else {
        return Ok(1);
    };
    if packet_len <= mtu {
        return Ok(1);
    }

    let payload_len = packet_len
        .checked_sub(IPV4_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(IPV4_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len
        .checked_add(max_fragment_payload_len - 1)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?
        / max_fragment_payload_len)
}
