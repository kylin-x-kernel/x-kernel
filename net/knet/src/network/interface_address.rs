// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 address configuration owned by each registered interface.

use alloc::{vec, vec::Vec};

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
    /// Stored Linux `ifa_broadcast`; `None` represents the zero address.
    ///
    /// This is not derived from the CIDR. Netlink address creation without
    /// `IFA_BROADCAST` stores zero, while `SIOCSIFADDR` stores the computed
    /// broadcast for a broadcast-capable interface with a prefix shorter
    /// than `/31`.
    pub broadcast: Option<Ipv4Address>,
    /// Linux `IFA_F_NOPREFIXROUTE`: no connected-prefix route is installed.
    pub no_prefix_route: bool,
    /// Stored Linux `ifa_label`, excluding the terminating NUL.
    ///
    /// `None` means the default device-name label.  The fixed-size array keeps
    /// the entry `Copy` while covering every valid `IFNAMSIZ - 1` label.
    pub label: Option<[u8; 16]>,
}

/// An interface address and its interface identity for configuration queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrSnapshot {
    pub dev: NetDeviceId,
    pub entry: Ipv4AddrEntry,
    pub label: [u8; 16],
    /// Whether this is a Linux secondary address on its device.
    ///
    /// Linux recomputes `IFA_F_SECONDARY` on every insertion instead of
    /// trusting the flag supplied by userspace.  The flag is therefore
    /// represented as an ordering relationship: the first address for a
    /// `(prefix length, network)` pair is primary and every later matching
    /// address on the same device is secondary.  Deriving the state avoids a
    /// stale flag after replacement and deletion.
    pub secondary: bool,
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

    fn same_subnet(left: &Ipv4AddrEntry, right: &Ipv4AddrEntry) -> bool {
        left.addr.prefix_len() == right.addr.prefix_len()
            && left.addr.network() == right.addr.network()
    }

    fn is_secondary_index(&self, index: usize) -> bool {
        let Some(entry) = self.addresses.get(index) else {
            return false;
        };
        self.addresses[..index]
            .iter()
            .any(|earlier| Self::same_subnet(earlier, entry))
    }

    /// Returns whether `addr` is a secondary address on this device.
    pub(crate) fn is_secondary(&self, addr: Ipv4Cidr) -> bool {
        self.entry_index(addr)
            .is_some_and(|index| self.is_secondary_index(index))
    }

    /// Returns the primary address that owns `entry`'s shared subnet.
    ///
    /// Linux derives prefix, broadcast, and source metadata from the primary
    /// address, including for routes installed for a secondary address.
    pub(crate) fn primary_for(&self, entry: &Ipv4AddrEntry) -> Option<Ipv4AddrEntry> {
        self.addresses
            .iter()
            .find(|candidate| Self::same_subnet(candidate, entry))
            .copied()
    }

    fn insertion_error(&self, entry: Ipv4AddrEntry) -> Option<LinuxError> {
        if entry.addr.address().is_unspecified() {
            return None;
        }
        if !entry.addr.address().is_unicast() {
            return Some(LinuxError::EINVAL);
        }
        for existing in &self.addresses {
            if existing.addr == entry.addr {
                return Some(LinuxError::EEXIST);
            }
            if Self::same_subnet(existing, &entry) && existing.scope != entry.scope {
                return Some(LinuxError::EINVAL);
            }
        }
        None
    }

    /// Places an already validated entry at Linux's list position.
    ///
    /// A primary is inserted after the last primary whose scope is at least as
    /// broad as the new address's scope.  A secondary is appended to the tail,
    /// matching `__inet_insert_ifa`.
    fn place(&mut self, entry: Ipv4AddrEntry) {
        let secondary = self
            .addresses
            .iter()
            .any(|existing| Self::same_subnet(existing, &entry));
        if secondary {
            self.addresses.push(entry);
            return;
        }
        let mut insert_at = 0;
        for (index, existing) in self.addresses.iter().enumerate() {
            if !self.is_secondary_index(index) && entry.scope <= existing.scope {
                insert_at = index + 1;
            }
        }
        self.addresses.insert(insert_at, entry);
    }

    /// Inserts a unicast CIDR using Linux's secondary-address rules.
    ///
    /// A matching `(mask, network)` entry with a different scope is an
    /// `EINVAL` conflict.  A later matching address becomes secondary without
    /// mutating the existing primary.  A zero local address is ignored, as in
    /// `__inet_insert_ifa`.
    pub(crate) fn insert(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if entry.addr.address().is_unspecified() {
            return Ok(());
        }
        if let Some(error) = self.insertion_error(entry) {
            return Err(error);
        }
        self.place(entry);
        Ok(())
    }

    /// Replaces an address using Linux's delete-then-insert update path.
    ///
    /// `SIOCSIFADDR`, `SIOCSIFNETMASK`, and `SIOCSIFBRDADDR` all delete the old
    /// address before inserting the replacement.  Deleting a primary also
    /// deletes its matching secondaries, so replacement must not preserve
    /// stale secondary state or list position.
    ///
    /// Returns every entry removed by the delete-then-insert path, including
    /// matching secondaries.  `EADDRNOTAVAIL` means `addr` is absent, `EINVAL`
    /// a non-unicast or scope-conflicting replacement, and `EEXIST` an
    /// unrelated entry already using the replacement CIDR.  Errors leave the
    /// set intact.
    pub(crate) fn replace(
        &mut self,
        addr: Ipv4Cidr,
        entry: Ipv4AddrEntry,
    ) -> Result<Vec<Ipv4AddrEntry>, LinuxError> {
        let index = self.entry_index(addr).ok_or(LinuxError::EADDRNOTAVAIL)?;
        if !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        let old = self.addresses[index];
        // The ioctl update path mutates an existing `in_ifaddr` in place, so it
        // keeps the configured label unless the replacement explicitly sets
        // one.  Netlink replacement never reaches this path: Linux only updates
        // metric/protocol/lifetime for an existing netlink address.
        let mut entry = entry;
        if entry.label.is_none() {
            entry.label = old.label;
        }
        let replacing_primary = !self.is_secondary_index(index);
        for (other_index, existing) in self.addresses.iter().enumerate() {
            if other_index == index {
                continue;
            }
            // Replacing a primary removes every matching secondary before the
            // replacement is inserted, so those entries cannot conflict.
            if replacing_primary && Self::same_subnet(existing, &old) {
                continue;
            }
            if existing.addr == entry.addr {
                return Err(LinuxError::EEXIST);
            }
            if Self::same_subnet(existing, &entry) && existing.scope != entry.scope {
                return Err(LinuxError::EINVAL);
            }
        }
        let removed = self.remove_with_secondaries(addr);
        self.place(entry);
        Ok(removed)
    }

    #[cfg(unittest)]
    pub(crate) fn remove(&mut self, addr: Ipv4Cidr) -> Option<Ipv4AddrEntry> {
        let index = self.entry_index(addr)?;
        Some(self.addresses.remove(index))
    }

    /// Removes `addr` and, for a primary, every following matching secondary.
    ///
    /// Linux's default `promote_secondaries=0` behavior deletes secondaries
    /// that share the primary's mask and network before deleting the primary.
    /// A secondary deletion removes only that address.
    pub(crate) fn remove_with_secondaries(&mut self, addr: Ipv4Cidr) -> Vec<Ipv4AddrEntry> {
        let Some(index) = self.entry_index(addr) else {
            return Vec::new();
        };
        if self.is_secondary_index(index) {
            return vec![self.addresses.remove(index)];
        }
        let primary = self.addresses[index];
        let mut removed = Vec::new();
        let mut cursor = index + 1;
        while cursor < self.addresses.len() {
            if Self::same_subnet(&self.addresses[cursor], &primary) {
                removed.push(self.addresses.remove(cursor));
            } else {
                cursor += 1;
            }
        }
        removed.push(self.addresses.remove(index));
        removed
    }

    /// Selects a source address using Linux `inet_select_addr` semantics.
    ///
    /// Secondary addresses never provide a source.  Address scope is capped by
    /// the local-network scope, defaulting to host scope, and an address that
    /// matches the next hop is preferred immediately.  Otherwise the first
    /// eligible address is retained.
    pub(crate) fn select_source(
        &self,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let localnet_scope = crate::network::router::ROUTE_SCOPE_HOST;
        let scope = route_scope.min(localnet_scope);
        let mut first = None;
        for (index, entry) in self.addresses.iter().enumerate() {
            if self.is_secondary_index(index) {
                continue;
            }
            if entry.scope.min(localnet_scope) > scope {
                continue;
            }
            if next_hop.is_unspecified() || entry.addr.contains_addr(next_hop) {
                return Some(entry.addr.address());
            }
            if first.is_none() {
                first = Some(entry.addr.address());
            }
        }
        first
    }

    /// Selects a fallback source from any device, as `in_dev_select_addr`
    /// does during `inet_select_addr`'s all-device walk.
    ///
    /// Link-scope addresses are deliberately excluded from this fallback.
    pub(crate) fn select_fallback_source(&self, route_scope: u8) -> Option<Ipv4Address> {
        self.addresses
            .iter()
            .enumerate()
            .find(|(index, entry)| {
                !self.is_secondary_index(*index)
                    && entry.scope != crate::network::router::ROUTE_SCOPE_LINK
                    && entry.scope <= route_scope
            })
            .map(|(_, entry)| entry.addr.address())
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/network/interface_address/tests.rs");
