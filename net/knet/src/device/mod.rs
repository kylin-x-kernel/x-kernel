// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network device abstractions.
use core::task::Waker;

use smoltcp::{storage::PacketBuffer, time::Instant, wire::IpAddress};

mod ethernet;
mod loopback;
#[cfg(feature = "vsock")]
mod vsock;

pub use ethernet::*;
pub use loopback::*;
#[cfg(feature = "vsock")]
pub use vsock::*;

/// Trait implemented by network device backends.
pub trait NetDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Polls the device and pushes received IP packets into `buffer`.
    fn poll_rx(&mut self, buffer: &mut PacketBuffer<()>, timestamp: Instant) -> bool;
    /// Sends an IP packet to the next hop.
    ///
    /// Returns `true` if this operation resulted in the readiness of receive
    /// operation. This is true for loopback devices and can be used to speed
    /// up packet processing.
    fn send_ip_packet(&mut self, next_hop: IpAddress, ip_packet: &[u8], timestamp: Instant)
    -> bool;

    /// Register a waker for receive readiness.
    fn register_rx_waker(&self, waker: &Waker);
}
