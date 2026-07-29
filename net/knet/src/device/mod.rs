// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network device abstractions.
use ::core::task::Waker;
use kerrno::KResult;
use khal::time::TimeValue;

mod ethernet;
mod loopback;
#[cfg(feature = "vsock")]
mod vsock;

pub use ethernet::*;
pub use loopback::*;
#[cfg(feature = "vsock")]
pub use vsock::*;

use crate::{
    buf::PacketBuf,
    ip::{IpAddress, Ipv4Cidr},
};

/// Trait implemented by network device backends.
pub trait NetDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Device-model ID for driver-backed devices.
    fn device_id(&self) -> Option<kdevice::DeviceId> {
        None
    }

    /// Polls one received packet from the device.
    fn poll_rx(&mut self, ifindex: i32, timestamp: TimeValue) -> Option<PacketBuf>;
    /// Sends an IP packet to the next hop.
    ///
    /// Returns `true` if this operation resulted in the readiness of receive
    /// operation. This is true for loopback devices and can be used to speed
    /// up packet processing.
    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        packet: PacketBuf,
        timestamp: TimeValue,
    ) -> bool;

    /// Sends a link-layer frame through the device.
    ///
    /// `ifindex` identifies the egress network interface and `frame` contains
    /// the complete link-layer frame to transmit. Devices that do not support
    /// raw link-layer transmission keep the default implementation, which
    /// returns [`kerrno::KError::OperationNotSupported`].
    fn send_link_frame(&mut self, _ifindex: i32, _frame: &[u8]) -> KResult<usize> {
        Err(kerrno::KError::OperationNotSupported)
    }

    /// Registers the aggregated Service RX/timeout waker for this device.
    ///
    /// Callers must install the same upstream source waker used by
    /// [`crate::stack::service::Service::register_rx_waker`] (the Service
    /// `timeout_poll`). Devices are not multi-waiter hubs; task fan-out stays
    /// on that aggregated [`kpoll::PollSet`].
    fn register_rx_waker(&self, waker: &Waker);

    /// Synchronizes device state prepared by the control-plane adapter.
    fn sync_netlink(
        &mut self,
        _name: Option<&str>,
        _ipv4_addr: Option<Ipv4Cidr>,
        _neighbors: &[(IpAddress, [u8; 6])],
    ) {
    }
}
