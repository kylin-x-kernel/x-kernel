// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-capacity pending-record table for one queue binding.

use crate::{EntryKey, EntryOwner, WorkColor, WorkInstanceId, WorkKey, id::PendingRecordId};

#[derive(Clone, Copy)]
pub(crate) struct PendingRecord {
    pub work: WorkKey,
    pub owner: EntryOwner,
    pub key: EntryKey,
    pub instance: WorkInstanceId,
    pub color: WorkColor,
    pub active: bool,
}

#[derive(Clone, Copy)]
struct PendingRecordSlot {
    generation: usize,
    record: Option<PendingRecord>,
}

impl PendingRecordSlot {
    const fn new() -> Self {
        Self {
            generation: 0,
            record: None,
        }
    }
}

pub(crate) struct PendingRecordTable<const CAP: usize> {
    slots: [PendingRecordSlot; CAP],
}

impl<const CAP: usize> PendingRecordTable<CAP> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [PendingRecordSlot::new(); CAP],
        }
    }

    pub(crate) fn insert(
        &mut self,
        record: PendingRecord,
    ) -> Result<PendingRecordId, PendingRecord> {
        for (slot, entry) in self.slots.iter_mut().enumerate() {
            if entry.record.is_none() {
                entry.generation = entry.generation.wrapping_add(1).max(1);
                entry.record = Some(record);
                return Ok(PendingRecordId::new(slot, entry.generation));
            }
        }
        Err(record)
    }

    pub(crate) fn get(&self, id: PendingRecordId) -> Option<PendingRecord> {
        let slot = self.slots.get(id.slot())?;
        (slot.generation == id.generation())
            .then_some(slot.record)
            .flatten()
    }

    pub(crate) fn get_mut(&mut self, id: PendingRecordId) -> Option<&mut PendingRecord> {
        let slot = self.slots.get_mut(id.slot())?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.record.as_mut()
    }

    pub(crate) fn remove(&mut self, id: PendingRecordId) -> Option<PendingRecord> {
        let slot = self.slots.get_mut(id.slot())?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.record.take()
    }

    pub(crate) fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.record.is_some())
            .count()
    }
}
