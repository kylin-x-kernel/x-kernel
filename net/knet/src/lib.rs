// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! It provides unified networking primitives for TCP/UDP communication.
//!
//! # Organization
//!
//! - [`TcpSocket`]: A TCP socket that provides POSIX-like APIs.
//! - [`UdpSocket`]: A UDP socket that provides POSIX-like APIs.
//! - [`dns_query`]: Function for DNS query.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

#[macro_use]
extern crate log;
extern crate alloc;

mod consts;
mod device;
mod ip;
mod link;
pub mod netlink;
mod poller;
mod socket;
mod stack;
mod transport;
pub mod unix;
#[cfg(feature = "vsock")]
pub mod vsock;

use alloc::{borrow::ToOwned, boxed::Box, sync::Arc};
use core::net::SocketAddr;

#[cfg(feature = "vsock")]
use kclass::{ClassDevice, VsockDeviceImpl, subscribe_vsock_available, vsock_devices};
use kclass::{NetDevice, net_devices};
use kdevice::subscribe_device_removed;
use kerrno::KError;
use lazyinit::LazyInit;
pub use link::packet;
pub(crate) use link::{buf, wire};
pub use socket::{
    file::{sock_alloc_file, sock_from_file},
    options, *,
};
pub(crate) use socket::{general, state};
pub(crate) use stack::{ipv4, listen_table, router, service, wrapper};
pub(crate) use transport::udp_err;
pub use transport::{raw, tcp, udp};

use crate::{
    consts::{GATEWAY, IP, IP_PREFIX, TIMER_SAMPLE_PERIOD},
    device::{EthernetDevice, LoopbackDevice},
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    listen_table::ListenTable,
    router::{Ipv4AddrEntry, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE, Router, Rule},
    service::Service,
    wrapper::SocketSetWrapper,
};

static LISTEN_TABLE: LazyInit<ListenTable> = LazyInit::new();
static SOCKET_SET: LazyInit<SocketSetWrapper> = LazyInit::new();

static SERVICE: LazyInit<Service> = LazyInit::new();

/// Initializes the network subsystem by NIC devices.
pub fn init_network() {
    info!("Initialize network subsystem...");

    let mut net_devs = net_devices();

    let mut router = Router::new();
    let lo_dev = router.add_device(Box::new(LoopbackDevice::new()));

    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: lo_dev,
            addr: lo_ip,
            scope: ROUTE_SCOPE_HOST,
        })
        .expect("loopback IPv4 address must fit the interface");

    if let Some(handle) = net_devs.pop() {
        let device_id = handle.id();
        info!("  use NIC 0: {:?}", handle.name());

        let eth0_address = wire::MacAddress(handle.mac().0);
        let eth0_ip = Ipv4Cidr::new(IP.parse().expect("Invalid IPv4 address"), IP_PREFIX);

        let eth0 = EthernetDevice::new("eth0".to_owned(), handle);
        let eth0_dev = router.add_device(Box::new(eth0));

        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: eth0_dev,
                addr: eth0_ip,
                scope: ROUTE_SCOPE_UNIVERSE,
            })
            .expect("Ethernet IPv4 address must fit the interface");
        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(
                GATEWAY.parse().expect("Invalid gateway address"),
            )),
            eth0_dev,
            eth0_ip.address().into(),
        ));

        info!("eth0:");
        info!("  mac:  {}", eth0_address);
        info!("  ip:   {}", eth0_ip);
        subscribe_network_unregister(device_id);
    } else {
        warn!("  No network device found!");
    }

    for dev in &router.devices {
        info!("Device: {}", dev.name());
    }

    let service = Service::new(router);
    SERVICE.init_once(service);

    SOCKET_SET.init_once(SocketSetWrapper::new());
    LISTEN_TABLE.init_once(ListenTable::new());
    udp::init_udp_registry();
    poller::network_poller().start();
    ktask::register_timer_callback(TIMER_SAMPLE_PERIOD, |_| SERVICE.handle_timer_tick());
}

fn subscribe_network_unregister(id: kdevice::DeviceId) {
    subscribe_device_removed(Arc::new(move |removed_id| {
        if removed_id != id {
            return;
        }
        if unregister_netdev(id) {
            warn!("network: detached removed device {:?}", id);
        }
    }));
}

/// Removes a NIC from the network stack.
///
/// Takes [`netlink::rtnl_lock`] so teardown is serialized with rtnetlink
/// mutations, matching Linux `unregister_netdev()` in `net/core/dev.c`.
pub(crate) fn unregister_netdev(id: kdevice::DeviceId) -> bool {
    if !SERVICE.is_inited() {
        return false;
    }
    let _rtnl = netlink::rtnl_lock();
    SERVICE.remove_device_by_model_id(id).is_some()
}

/// Init vsock subsystem by vsock devices.
#[cfg(feature = "vsock")]
pub fn init_vsock() {
    info!("Initialize vsock subsystem...");
    let mut vsock_devs = vsock_devices();
    if let Some(handle) = vsock_devs.pop() {
        register_vsock_handle(handle);
    } else {
        warn!("  No vsock device found!");
    }
    subscribe_vsock_available(Arc::new(register_vsock_handle));
}

/// Starts vsock runtime workers that require all scheduler run queues.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn start_vsock_bridge() {
    crate::vsock::bridge::start();
}

#[cfg(feature = "vsock")]
fn register_vsock_handle(handle: ClassDevice<VsockDeviceImpl>) {
    use crate::vsock::connection_manager::{register_vsock_dev, unregister_vsock_dev};

    let id = handle.id();
    info!(
        "  use vsock 0: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );
    if let Err(e) = register_vsock_dev(handle) {
        if e != KError::AlreadyExists {
            warn!("Failed to initialize vsock device: {:?}", e);
        }
        return;
    }
    #[cfg(feature = "vsock_tipc_bridge")]
    if let Err(e) = crate::vsock::bridge::init() {
        warn!("Failed to initialize vsock-TIPC bridge: {:?}", e);
    }
    subscribe_device_removed(Arc::new(move |removed_id| {
        if removed_id == id && unregister_vsock_dev(id) {
            warn!("vsock: detached removed device {:?}", id);
        }
    }));
}

/// Assists one already scheduled network polling round from task context.
///
/// The call does not publish new work or wait for another executor.
pub fn poll_interfaces() {
    poller::network_poller().assist_once();
}

/// Send a complete link-layer frame through the interface identified by `ifindex`.
pub fn send_link_frame(ifindex: i32, frame: &[u8]) -> kerrno::KResult<usize> {
    if !SERVICE.is_inited() {
        return Err(kerrno::KError::OperationNotSupported);
    }
    SERVICE.send_link_frame(ifindex, frame)
}

/// Send a UDP datagram through the kernel network stack.
pub fn send_udp_payload(dst: SocketAddr, payload: &[u8]) -> kerrno::KResult<usize> {
    if !SERVICE.is_inited() {
        return Err(kerrno::KError::OperationNotSupported);
    }

    let socket = udp::UdpSocket::new();
    socket.send_datagram_now(dst, payload)
}

/// Persistent UDP relay socket for request/reply datagram flows.
pub struct UdpDatagramRelay {
    socket: udp::UdpSocket,
}

impl UdpDatagramRelay {
    /// Create a UDP relay socket with an ephemeral local port.
    pub fn new() -> kerrno::KResult<Self> {
        Self::new_with_port(0)
    }

    /// Create a UDP relay socket bound to `local_port`.
    pub fn new_with_port(local_port: u16) -> kerrno::KResult<Self> {
        if !SERVICE.is_inited() {
            return Err(kerrno::KError::OperationNotSupported);
        }

        let socket = udp::UdpSocket::new();
        socket.bind(SocketAddrEx::Ip(SocketAddr::new(
            core::net::IpAddr::V4(core::net::Ipv4Addr::UNSPECIFIED),
            local_port,
        )))?;
        Ok(Self { socket })
    }

    /// Send a UDP datagram through this relay socket.
    pub fn send_to(&self, dst: SocketAddr, payload: &[u8]) -> kerrno::KResult<usize> {
        self.socket.send_datagram_now(dst, payload)
    }

    /// Try to receive a UDP datagram without blocking.
    pub fn try_recv(&self, buf: &mut [u8]) -> kerrno::KResult<Option<(usize, SocketAddr)>> {
        self.socket.recv_datagram_now(buf)
    }
}
