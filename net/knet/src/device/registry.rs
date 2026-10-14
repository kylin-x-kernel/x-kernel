// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable network interface IDs backed by compact device storage.

use alloc::{boxed::Box, vec::Vec};

use hashbrown::HashMap;
use kerrno::LinuxError;

use crate::device::{LinkKind, NetDevice, NetDeviceId};

struct DeviceEntry {
    id: NetDeviceId,
    device: Box<dyn NetDevice>,
}

/// Maintains `slots[&entries[index].id] == index` for every stored entry.
pub(crate) struct DeviceRegistry {
    entries: Vec<DeviceEntry>,
    slots: HashMap<NetDeviceId, usize>,
    next_ifindex: Option<i32>,
    loopback_registered: bool,
}

impl DeviceRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            slots: HashMap::new(),
            next_ifindex: Some(2),
            loopback_registered: false,
        }
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
    pub(crate) fn get_by_slot(&self, slot: usize) -> Option<&dyn NetDevice> {
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
