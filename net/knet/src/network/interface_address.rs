// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 address configuration owned by each registered interface.

use alloc::{string::String, vec::Vec};

use kerrno::LinuxError;

use crate::{
    device::NetDeviceId,
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
};

/// An IPv4 address configured on a network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrEntry {
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

/// An interface address and its interface identity for configuration queries.
#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub dev: NetDeviceId,
    pub entry: Ipv4AddrEntry,
    pub label: String,
}

/// IPv4 addresses owned by one registered device.
///
/// Mutations identify entries by CIDR and preserve uniqueness and device-local order.
/// Callers select which address to update and apply operation-specific collision
/// policy. Rejected mutations leave the set intact.
/// DeviceRegistry enforces the caller-supplied capacity and synchronizes device
/// projections. The enclosing Router lock protects route reconciliation.
#[derive(Default)]
pub(crate) struct Ipv4AddressSet {
    addresses: Vec<Ipv4AddrEntry>,
}

impl Ipv4AddressSet {
    pub(crate) fn as_slice(&self) -> &[Ipv4AddrEntry] {
        &self.addresses
    }

    fn entry_index(&self, addr: Ipv4Cidr) -> Option<usize> {
        self.addresses.iter().position(|entry| entry.addr == addr)
    }

    /// Tests CIDR identity, excluding mutable attributes.
    pub(crate) fn contains_entry(&self, addr: Ipv4Cidr) -> bool {
        self.entry_index(addr).is_some()
    }

    /// Inserts a unicast CIDR, accepting an existing identity without changes.
    pub(crate) fn insert(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if self.contains_entry(entry.addr) {
            return Ok(());
        }
        self.addresses.push(entry);
        Ok(())
    }

    /// Replaces the entry identified by `addr` without changing address order.
    ///
    /// Returns `EADDRNOTAVAIL` when `addr` is absent, `EINVAL` for a non-unicast
    /// replacement, or `EEXIST` when another entry has the replacement CIDR.
    /// Errors leave the set intact.
    pub(crate) fn replace(
        &mut self,
        addr: Ipv4Cidr,
        entry: Ipv4AddrEntry,
    ) -> Result<Ipv4AddrEntry, LinuxError> {
        let index = self.entry_index(addr).ok_or(LinuxError::EADDRNOTAVAIL)?;
        if !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if entry.addr != addr && self.contains_entry(entry.addr) {
            return Err(LinuxError::EEXIST);
        }
        Ok(core::mem::replace(&mut self.addresses[index], entry))
    }

    pub(crate) fn remove(&mut self, addr: Ipv4Cidr) -> Option<Ipv4AddrEntry> {
        let index = self.entry_index(addr)?;
        Some(self.addresses.remove(index))
    }

    /// Prefers a covering address eligible by scope, then the first eligible one.
    pub(crate) fn select_source(
        &self,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let mut eligible = self
            .addresses
            .iter()
            .filter(|entry| entry.scope <= route_scope);
        let first = eligible.next()?;
        let selected = if first.addr.contains_addr(next_hop) {
            first
        } else {
            eligible
                .find(|entry| entry.addr.contains_addr(next_hop))
                .unwrap_or(first)
        };
        Some(selected.addr.address())
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/network/interface_address/tests.rs");
