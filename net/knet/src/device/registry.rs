// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registered interfaces, link configuration, and aggregate protocol-stack MTU.

use alloc::{boxed::Box, vec::Vec};

use hashbrown::HashMap;
use kerrno::LinuxError;

use crate::{
    consts::STANDARD_MTU,
    device::{LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NetDevice, NetDeviceId},
};

struct DeviceEntry {
    id: NetDeviceId,
    device: Box<dyn NetDevice>,
}

/// Owns registered devices and validates collection-wide link configuration.
///
/// Maintains `slots[&entries[index].id] == index` and refreshes the aggregate
/// MTU on registration, removal, and successful MTU updates. Callers serialize
/// registry access with the enclosing Router mutex.
pub(crate) struct DeviceRegistry {
    entries: Vec<DeviceEntry>,
    slots: HashMap<NetDeviceId, usize>,
    next_ifindex: Option<i32>,
    loopback_registered: bool,
    effective_mtu: usize,
}

impl DeviceRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            slots: HashMap::new(),
            next_ifindex: Some(2),
            loopback_registered: false,
            effective_mtu: STANDARD_MTU,
        }
    }

    /// Returns the MTU advertised through the smoltcp device capabilities.
    ///
    /// Uses the minimum device IP MTU capped at 65535, or the default for an empty set.
    pub(crate) fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub(crate) fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.iter()
            .enumerate()
            .map(|(slot, device)| device.link_snapshot(self.id_at(slot).ifindex()))
            .collect()
    }

    pub(crate) fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.get_by_id(NetDeviceId::new(ifindex)?)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub(crate) fn has_device(&self, ifindex: i32) -> bool {
        NetDeviceId::new(ifindex)
            .and_then(|id| self.slot(id))
            .is_some()
    }

    pub(crate) fn device_id_by_name(&self, name: &str) -> Option<NetDeviceId> {
        self.iter()
            .position(|device| device.name() == name)
            .map(|slot| self.id_at(slot))
    }

    pub(crate) fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.get_by_id(NetDeviceId::new(ifindex)?)
            .map(|device| device.link_send_snapshot())
    }

    /// Validates and applies a link update, preserving name uniqueness.
    ///
    /// Returns `Ok(true)` when the effective device MTU changed and the
    /// smoltcp interface must be rebuilt to refresh its cached capabilities.
    pub(crate) fn update_device_link(
        &mut self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        let slot = NetDeviceId::new(ifindex)
            .and_then(|id| self.slot(id))
            .ok_or(LinuxError::ENODEV)?;
        let device = self.get_by_slot(slot).ok_or(LinuxError::ENODEV)?;
        if let Some(name) = update.name.as_deref() {
            crate::device::validate_interface_name(name)?;
            if self
                .iter()
                .enumerate()
                .any(|(other_slot, device)| other_slot != slot && device.name() == name)
            {
                return Err(LinuxError::EEXIST);
            }
        }
        if let Some(mtu) = update.mtu {
            device.link_kind().validate_mtu(mtu)?;
        }

        let is_mtu_changed = update.mtu.is_some_and(|mtu| mtu != device.mtu());
        let effective_mtu = self.effective_mtu;

        let LinkConfigUpdate { name, mtu, is_up } = update;
        let device = self.get_mut_by_slot(slot).expect("validated device slot");
        if let Some(mtu) = mtu {
            device.set_mtu(mtu)?;
        }
        if let Some(name) = name {
            device.set_name(name);
        }
        if let Some(is_up) = is_up {
            device.set_link_up(is_up);
        }
        if is_mtu_changed {
            self.recompute_effective_mtu();
        }
        Ok(self.effective_mtu != effective_mtu)
    }

    fn recompute_effective_mtu(&mut self) {
        self.effective_mtu = self
            .iter()
            .map(|device| device.mtu().min(u16::MAX as usize))
            .min()
            .unwrap_or(STANDARD_MTU);
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn register(
        &mut self,
        device: Box<dyn NetDevice>,
    ) -> Result<NetDeviceId, LinuxError> {
        let id = if device.link_kind() == LinkKind::Loopback {
            if self.loopback_registered {
                return Err(LinuxError::EEXIST);
            }
            self.loopback_registered = true;
            NetDeviceId::LOOPBACK
        } else {
            let id = self
                .next_ifindex
                .and_then(NetDeviceId::new)
                .ok_or(LinuxError::ENOSPC)?;
            self.next_ifindex = id.ifindex().checked_add(1);
            id
        };
        let slot = self.entries.len();
        self.entries.push(DeviceEntry { id, device });
        self.slots.insert(id, slot);
        self.recompute_effective_mtu();
        Ok(id)
    }

    pub(crate) fn remove_slot(&mut self, slot: usize) -> Box<dyn NetDevice> {
        let removed_entry = self.entries.remove(slot);
        self.slots.remove(&removed_entry.id);
        if removed_entry.id == NetDeviceId::LOOPBACK {
            self.loopback_registered = false;
        }
        for (new_slot, entry) in self.entries.iter().enumerate().skip(slot) {
            self.slots.insert(entry.id, new_slot);
        }
        self.recompute_effective_mtu();
        removed_entry.device
    }

    pub(crate) fn slot(&self, id: NetDeviceId) -> Option<usize> {
        self.slots.get(&id).copied()
    }

    pub(crate) fn id_at(&self, slot: usize) -> NetDeviceId {
        self.entries[slot].id
    }

    pub(crate) fn get_by_id(&self, id: NetDeviceId) -> Option<&dyn NetDevice> {
        self.slot(id).map(|slot| self.entries[slot].device.as_ref())
    }

    pub(crate) fn get_mut_by_id(&mut self, id: NetDeviceId) -> Option<&mut dyn NetDevice> {
        let slot = self.slot(id)?;
        Some(self.entries[slot].device.as_mut())
    }

    /// Returns the device at a storage slot, which can change after removal.
    fn get_by_slot(&self, slot: usize) -> Option<&dyn NetDevice> {
        self.entries.get(slot).map(|entry| entry.device.as_ref())
    }

    /// Returns the device at a storage slot, which can change after removal.
    pub(crate) fn get_mut_by_slot(&mut self, slot: usize) -> Option<&mut (dyn NetDevice + '_)> {
        let entry = self.entries.get_mut(slot)?;
        Some(entry.device.as_mut())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &dyn NetDevice> {
        self.entries.iter().map(|entry| entry.device.as_ref())
    }

    pub(crate) fn iter_mut<'a>(
        &'a mut self,
    ) -> impl Iterator<Item = &'a mut (dyn NetDevice + 'static)> + 'a {
        self.entries.iter_mut().map(|entry| entry.device.as_mut())
    }

    pub(crate) fn iter_mut_with_ids<'a>(
        &'a mut self,
    ) -> impl DoubleEndedIterator<Item = (NetDeviceId, &'a mut (dyn NetDevice + 'static))> + 'a
    {
        self.entries
            .iter_mut()
            .map(|entry| (entry.id, entry.device.as_mut()))
    }
}
