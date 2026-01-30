// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core traits and types for network device drivers.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

extern crate alloc;

#[cfg(feature = "fxmac")]
/// fxmac driver for PhytiumPi
pub mod fxmac;
#[cfg(feature = "ixgbe")]
/// ixgbe NIC device driver.
pub mod ixgbe;

#[doc(no_inline)]
pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

mod net_buf;
pub use self::net_buf::{NetBuf, NetBufBox, NetBufHandle, NetBufPool};

/// The hardware (MAC) address of a NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddress(pub [u8; 6]);

/// Operations that require a network device (NIC) driver to implement.
pub trait NetDriverOps: DriverOps {
    /// The hardware address of the NIC.
    fn mac(&self) -> MacAddress;

    /// Whether the device can transmit packets.
    fn can_tx(&self) -> bool;

    /// Whether the device can receive packets.
    fn can_rx(&self) -> bool;

    /// Size of the receive queue.
    fn rx_queue_len(&self) -> usize;

    /// Size of the transmit queue.
    fn tx_queue_len(&self) -> usize;

    /// Gives back the `rx_buf` to the receive queue for later receiving.
    ///
    /// `rx_buf` should be the same as the one returned by
    /// [`NetDriverOps::recv`].
    fn recycle_rx(&mut self, rx_buf: NetBufHandle) -> DriverResult;

    /// Poll the transmit queue and gives back the buffers for previous transmissions.
    /// returns [`DriverResult`].
    fn recycle_tx(&mut self) -> DriverResult;

    /// Transmits a packet in the buffer to the network, without blocking.
    fn send(&mut self, tx_buf: NetBufHandle) -> DriverResult;

    /// Receives a packet from the network and stores it in the [`NetBuf`].
    ///
    /// Before receiving, the driver should have already populated some buffers
    /// in the receive queue by [`NetDriverOps::recycle_rx`].
    ///
    /// If currently no incoming packets, returns an error with type
    /// [`DriverError::WouldBlock`].
    fn recv(&mut self) -> DriverResult<NetBufHandle>;

    /// Allocate a memory buffer of a specified size for network transmission.
    fn alloc_tx_buf(&mut self, size: usize) -> DriverResult<NetBufHandle>;
}
