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
        HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr,
        IpListenEndpoint as SmoltcpIpListenEndpoint,
    },
};

use crate::{LISTEN_TABLE, SOCKET_SET, netlink::RtnetlinkState, router::Router};

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

    pub fn get_source_address(&self, dst_addr: &SmoltcpIpAddress) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .table
            .lookup(&dst_addr)
            .map(|rule| super::to_smoltcp_ip_address(rule.src))
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn device_mask_for(&self, endpoint: &SmoltcpIpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn device_mask_for_addr(&self, addr: &SmoltcpIpAddress) -> u32 {
        let addr = super::from_smoltcp_ip_address(*addr);
        self.router
            .table
            .lookup(&addr)
            .map_or(u32::MAX, |it| 1u32 << it.dev)
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
                    let _ = ip_addrs.push(IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
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
