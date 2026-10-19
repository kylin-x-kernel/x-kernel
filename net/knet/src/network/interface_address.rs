// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 address assignments to network interfaces.
//!
//! Each record combines address values from `crate::ip` with an owning
//! interface, scope, and optional broadcast override.

use alloc::string::String;

use crate::{
    device::NetDeviceId,
    ip::{Ipv4Address, Ipv4Cidr},
};

/// An IPv4 address configured on a network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrEntry {
    pub dev: NetDeviceId,
    pub addr: Ipv4Cidr,
    pub scope: u8,
    /// Linux `ifa_broadcast` override. `None` means derive from the CIDR.
    pub broadcast: Option<Ipv4Address>,
}

impl Ipv4AddrEntry {
    pub(crate) fn effective_broadcast(self) -> Option<Ipv4Address> {
        self.broadcast.or_else(|| self.addr.broadcast())
    }
}

/// An interface address and its interface label for configuration queries.
#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub entry: Ipv4AddrEntry,
    pub label: String,
}
