// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-filesystem quota lifecycle and all-or-nothing per-inode charges.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    vec::Vec,
};

use kerrno::{KResult, LinuxError};

use crate::{
    ChargeFlags, Dqblk, QuotaFields, QuotaFormat, QuotaId, QuotaIo, QuotaState, QuotaType,
    overflow,
    tree::QuotaTree,
    v2::{read_info, write_info},
};

/// Loaded information for one quota class.
#[derive(Debug)]
pub struct Dqinfo {
    format: QuotaFormat,
    state: QuotaState,
    space_grace_seconds: u32,
    inode_grace_seconds: u32,
    tree: QuotaTree,
}

impl Dqinfo {
    /// Returns the loaded quota format.
    pub const fn format(&self) -> QuotaFormat {
        self.format
    }

    /// Returns accounting, enforcement and suspension state.
    pub const fn state(&self) -> QuotaState {
        self.state.union(QuotaState::USAGE_ENABLED)
    }

    /// Returns the duration of a newly started space grace period.
    pub const fn space_grace_seconds(&self) -> u32 {
        self.space_grace_seconds
    }

    /// Returns the duration of a newly started inode grace period.
    pub const fn inode_grace_seconds(&self) -> u32 {
        self.inode_grace_seconds
    }

    fn is_active(&self) -> bool {
        !self.state.contains(QuotaState::SUSPENDED)
    }

    fn sync(&mut self, io: &dyn QuotaIo, kind: QuotaType) -> KResult<()> {
        if self.tree.is_dirty {
            write_info(
                io,
                kind,
                self.space_grace_seconds,
                self.inode_grace_seconds,
                &mut self.tree,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct Dquot {
    data: Dqblk,
    offset: u64,
    is_dirty: bool,
    is_active: bool,
}

impl Dquot {
    fn commit(&mut self, io: &dyn QuotaIo, id: QuotaId, info: &mut Dqinfo) -> KResult<()> {
        if self.is_dirty || self.offset == 0 {
            self.is_dirty = true;
            info.tree
                .write(io, info.format, id, &mut self.offset, self.data)?;
            self.is_dirty = false;
        }
        Ok(())
    }
}

/// Canonical quota state and cached dquots for one filesystem.
///
/// Mutation requires exclusive access, including across quota-file I/O. The
/// containing filesystem provides a sleepable lock and keeps its I/O source
/// stable until disable succeeds. Record IDs occur only in the cache key;
/// there is no independent owner or duplicate identity in a cached dquot.
#[derive(Debug, Default)]
pub struct QuotaInfo {
    info: [Option<Dqinfo>; 3],
    dquots: BTreeMap<QuotaId, Dquot>,
}

impl QuotaInfo {
    /// Creates quota state with all classes disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a pre-existing quota file, enabling accounting and optionally limits.
    ///
    /// Only USAGE_ENABLED and LIMITS_ENABLED are accepted here; usage must be
    /// enabled. Failure leaves the quota class disabled.
    pub fn enable(
        &mut self,
        io: &dyn QuotaIo,
        kind: QuotaType,
        format: QuotaFormat,
        state: QuotaState,
    ) -> KResult<()> {
        if self.info[kind.index()].is_some() {
            return Err(LinuxError::EBUSY.into());
        }
        let allowed = QuotaState::USAGE_ENABLED | QuotaState::LIMITS_ENABLED;
        if !state.contains(QuotaState::USAGE_ENABLED) || state.bits() & !allowed.bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let (space_grace_seconds, inode_grace_seconds, tree) = read_info(io, kind, format)?;
        self.info[kind.index()] = Some(Dqinfo {
            format,
            state: state.difference(QuotaState::USAGE_ENABLED),
            space_grace_seconds,
            inode_grace_seconds,
            tree,
        });
        Ok(())
    }

    /// Returns loaded file information, including when suspended.
    pub fn info(&self, kind: QuotaType) -> Option<&Dqinfo> {
        self.info[kind.index()].as_ref()
    }

    /// Flushes and disables a quota class, discarding its record cache.
    ///
    /// Outstanding reservations return EBUSY: the filesystem must drain
    /// delayed allocation before removing its accounting source. I/O errors
    /// retain the loaded state and all dirty records for diagnosis or retry.
    pub fn disable(&mut self, io: &dyn QuotaIo, kind: QuotaType) -> KResult<()> {
        self.loaded_info(kind)?;
        self.check_no_reservations(kind)?;
        self.sync(io, kind)?;
        let unused: Vec<_> = self
            .dquots
            .iter()
            .filter_map(|(&id, dquot)| (id.kind() == kind && dquot.data.is_unused()).then_some(id))
            .collect();
        for id in unused {
            self.release(io, id)?;
        }
        self.dquots.retain(|id, _| id.kind() != kind);
        self.info[kind.index()] = None;
        Ok(())
    }

    /// Flushes a loaded class before suspending its accounting and limits.
    pub fn suspend(&mut self, io: &dyn QuotaIo, kind: QuotaType) -> KResult<()> {
        self.active_info(kind)?;
        self.check_no_reservations(kind)?;
        self.sync(io, kind)?;
        if let Some(info) = &mut self.info[kind.index()] {
            info.state.insert(QuotaState::SUSPENDED);
        }
        Ok(())
    }

    /// Resumes the same backing quota file after filesystem writes are allowed.
    pub fn resume(&mut self, kind: QuotaType) -> KResult<()> {
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        if !info.state.contains(QuotaState::SUSPENDED) {
            return Err(LinuxError::EINVAL.into());
        }
        info.state.remove(QuotaState::SUSPENDED);
        Ok(())
    }

    /// Enables or disables limits while preserving usage accounting.
    pub fn set_limits_enabled(&mut self, kind: QuotaType, is_enabled: bool) -> KResult<()> {
        self.active_info(kind)?;
        if let Some(info) = &mut self.info[kind.index()] {
            info.state.set(QuotaState::LIMITS_ENABLED, is_enabled);
        }
        Ok(())
    }

    /// Changes grace durations for subsequent crossings, preserving active deadlines.
    ///
    /// Writes the updated file information before returning. A failed write
    /// leaves the in-memory update dirty for a later synchronization attempt.
    pub fn set_grace_times(
        &mut self,
        io: &dyn QuotaIo,
        kind: QuotaType,
        space_seconds: u32,
        inode_seconds: u32,
    ) -> KResult<()> {
        self.active_info(kind)?;
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        info.space_grace_seconds = space_seconds;
        info.inode_grace_seconds = inode_seconds;
        info.tree.is_dirty = true;
        info.sync(io, kind)
    }

    /// Acquires a record and returns its native usage snapshot.
    ///
    /// Missing records are instantiated on disk before acquisition completes.
    /// Obtaining a snapshot can therefore require writes; I/O errors propagate.
    /// Disabled or suspended classes return ESRCH. Use Dqblk::total_bytes for
    /// quota-control space usage, which includes reservations.
    pub fn get(&mut self, io: &dyn QuotaIo, id: QuotaId) -> KResult<Dqblk> {
        self.acquire(io, id)?;
        self.dquots
            .get(&id)
            .map(|dquot| dquot.data)
            .ok_or(LinuxError::ESRCH.into())
    }

    /// Returns the smallest on-disk ID at least minimum, without wrapping.
    pub fn next_id(
        &self,
        io: &dyn QuotaIo,
        kind: QuotaType,
        minimum: u32,
    ) -> KResult<Option<QuotaId>> {
        let info = self.active_info(kind)?;
        info.tree
            .next_id(io, kind, minimum)?
            .map(|id| QuotaId::new(kind, id))
            .transpose()
    }

    /// Applies selected administrative fields without changing reservations.
    ///
    /// SPACE_USED supplies total bytes, including reservations; the allocated
    /// counter is adjusted accordingly. Supplied timers are honored only while
    /// above the corresponding soft limit. Otherwise the deadline is cleared.
    pub fn set(
        &mut self,
        io: &dyn QuotaIo,
        id: QuotaId,
        values: Dqblk,
        fields: QuotaFields,
        now_seconds: i64,
    ) -> KResult<()> {
        let info = self.active_info(id.kind())?;
        if fields.bits() & !QuotaFields::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        validate_update(values, fields, info.format)?;
        self.acquire(io, id)?;
        let info = self.active_info(id.kind())?;
        let mut dquot = *self.dquots.get(&id).ok_or(LinuxError::ESRCH)?;
        apply_update(&mut dquot.data, values, fields, info, now_seconds)?;
        dquot.is_dirty = true;
        self.dquots.insert(id, dquot);
        Ok(())
    }

    /// Charges bytes to at most one ID per quota class.
    ///
    /// Disabled/suspended classes are skipped. Admission is atomic across the
    /// supplied IDs: a rejected charge changes no usage or grace deadlines.
    /// Caller authorization is expressed by BYPASS_LIMITS, never inferred here.
    pub fn alloc_space(
        &mut self,
        io: &dyn QuotaIo,
        ids: &[QuotaId],
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        if bytes < 0 || flags.bits() & !ChargeFlags::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(io, ids, |dquot, info| {
            charge_space(&mut dquot.data, info, bytes, now_seconds, flags)?;
            if !flags.contains(ChargeFlags::RESERVE) {
                dquot.is_dirty = true;
            }
            Ok(())
        })
    }

    /// Charges one inode to each supplied active quota class.
    pub fn alloc_inode(
        &mut self,
        io: &dyn QuotaIo,
        ids: &[QuotaId],
        now_seconds: i64,
        can_bypass_limits: bool,
    ) -> KResult<()> {
        self.update_ids(io, ids, |dquot, info| {
            charge_inode(&mut dquot.data, info, now_seconds, can_bypass_limits)?;
            dquot.is_dirty = true;
            Ok(())
        })
    }

    /// Releases allocated bytes or delayed-allocation reservations.
    ///
    /// Free counters stop at zero. A deadline is cleared once total space is
    /// at or below the soft limit. Reservation-only changes do not dirty the
    /// on-disk allocated-space counter.
    pub fn free_space(
        &mut self,
        io: &dyn QuotaIo,
        ids: &[QuotaId],
        bytes: i64,
        is_reserved: bool,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(io, ids, |dquot, _| {
            if is_reserved {
                dquot.data.reserved_bytes = dquot.data.reserved_bytes.saturating_sub(bytes).max(0);
            } else {
                dquot.data.used_bytes = dquot.data.used_bytes.saturating_sub(bytes).max(0);
                dquot.is_dirty = true;
            }
            if dquot.data.total_bytes()? <= dquot.data.space_soft_limit_bytes {
                dquot.data.space_expires_at = 0;
            }
            Ok(())
        })
    }

    /// Releases one inode, stopping at zero and resetting an ended grace period.
    pub fn free_inode(&mut self, io: &dyn QuotaIo, ids: &[QuotaId]) -> KResult<()> {
        self.update_ids(io, ids, |dquot, _| {
            dquot.data.used_inodes = dquot.data.used_inodes.saturating_sub(1).max(0);
            if dquot.data.used_inodes <= dquot.data.inode_soft_limit {
                dquot.data.inode_expires_at = 0;
            }
            dquot.is_dirty = true;
            Ok(())
        })
    }

    /// Converts already admitted reservations to allocated bytes without rechecking limits.
    ///
    /// An amount exceeding any active record's reservation is an accounting
    /// error and returns EINVAL before changing any record.
    pub fn claim_space(&mut self, io: &dyn QuotaIo, ids: &[QuotaId], bytes: i64) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(io, ids, |dquot, _| {
            if dquot.data.reserved_bytes < bytes {
                return Err(LinuxError::EINVAL.into());
            }
            dquot.data.used_bytes = dquot
                .data
                .used_bytes
                .checked_add(bytes)
                .ok_or_else(overflow)?;
            dquot.data.reserved_bytes -= bytes;
            dquot.is_dirty = true;
            Ok(())
        })
    }

    /// Converts allocated bytes back into reservations without changing total usage.
    ///
    /// An amount exceeding any active record's allocated bytes returns EINVAL
    /// before changing any record. Signed administrative reconciliation does
    /// not create allocated space that this conversion can transfer.
    pub fn reclaim_space(&mut self, io: &dyn QuotaIo, ids: &[QuotaId], bytes: i64) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(io, ids, |dquot, _| {
            if dquot.data.used_bytes < bytes {
                return Err(LinuxError::EINVAL.into());
            }
            dquot.data.used_bytes = dquot
                .data
                .used_bytes
                .checked_sub(bytes)
                .ok_or_else(overflow)?;
            dquot.data.reserved_bytes = dquot
                .data
                .reserved_bytes
                .checked_add(bytes)
                .ok_or_else(overflow)?;
            dquot.is_dirty = true;
            Ok(())
        })
    }

    /// Writes dirty records and file information; the filesystem makes them durable.
    ///
    /// Attempts every dirty record and then file information, preserving the
    /// first error. Failed records and file information retain dirty state.
    pub fn sync(&mut self, io: &dyn QuotaIo, kind: QuotaType) -> KResult<()> {
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        let mut result = Ok(());
        for (id, dquot) in &mut self.dquots {
            if id.kind() != kind || !dquot.is_dirty {
                continue;
            }
            let write_result = dquot.commit(io, *id, info);
            result = result.and(write_result);
        }
        let info_result = info.sync(io, kind);
        result.and(info_result)
    }

    /// Drops a cached record, deleting its disk entry when no limits or usage remain.
    ///
    /// Reservations return EBUSY. IDs are values rather than borrowed cache
    /// entries, so later operations can reacquire a released record. Failed
    /// writes/deletes retain the record and any updated disk offset.
    /// A structural deletion error requires filesystem abort/recovery before
    /// further use; retaining the record does not make that deletion retryable.
    pub fn release(&mut self, io: &dyn QuotaIo, id: QuotaId) -> KResult<()> {
        self.loaded_info(id.kind())?;
        let Some(dquot) = self.dquots.get(&id).copied() else {
            return Ok(());
        };
        if dquot.data.reserved_bytes != 0 {
            return Err(LinuxError::EBUSY.into());
        }
        if !dquot.data.is_unused() {
            let info = self.info[id.kind().index()]
                .as_mut()
                .ok_or(LinuxError::ESRCH)?;
            let record = self.dquots.get_mut(&id).ok_or(LinuxError::ESRCH)?;
            let result = record.commit(io, id, info);
            let info_result = info.sync(io, id.kind());
            result.and(info_result)?;
            self.dquots.remove(&id);
            return Ok(());
        }
        let info = self.info[id.kind().index()]
            .as_mut()
            .ok_or(LinuxError::ESRCH)?;
        let record = self.dquots.get_mut(&id).ok_or(LinuxError::ESRCH)?;
        let result = info.tree.delete(io, info.format, id, &mut record.offset);
        if record.offset == 0 {
            record.is_active = false;
        }
        let info_result = info.sync(io, id.kind());
        result.and(info_result)?;
        self.dquots.remove(&id);
        Ok(())
    }

    fn loaded_info(&self, kind: QuotaType) -> KResult<&Dqinfo> {
        self.info[kind.index()]
            .as_ref()
            .ok_or(LinuxError::ESRCH.into())
    }

    fn active_info(&self, kind: QuotaType) -> KResult<&Dqinfo> {
        let info = self.loaded_info(kind)?;
        if !info.is_active() {
            return Err(LinuxError::ESRCH.into());
        }
        Ok(info)
    }

    fn check_no_reservations(&self, kind: QuotaType) -> KResult<()> {
        if self
            .dquots
            .iter()
            .any(|(id, record)| id.kind() == kind && record.data.reserved_bytes != 0)
        {
            return Err(LinuxError::EBUSY.into());
        }
        Ok(())
    }

    fn acquire(&mut self, io: &dyn QuotaIo, id: QuotaId) -> KResult<()> {
        self.active_info(id.kind())?;
        let info = self.info[id.kind().index()]
            .as_ref()
            .ok_or(LinuxError::ESRCH)?;
        if let Entry::Vacant(entry) = self.dquots.entry(id) {
            let (offset, data) = info.tree.read(io, info.format, id)?;
            entry.insert(Dquot {
                data,
                offset,
                is_dirty: false,
                is_active: offset != 0,
            });
        }
        let (info, records) = (&mut self.info[id.kind().index()], &mut self.dquots);
        let info = info.as_mut().ok_or(LinuxError::ESRCH)?;
        let dquot = records.get_mut(&id).ok_or(LinuxError::ESRCH)?;
        if dquot.is_active {
            return Ok(());
        }
        let result = dquot.commit(io, id, info);
        let info_result = info.sync(io, id.kind());
        result.and(info_result)?;
        dquot.is_active = true;
        Ok(())
    }

    fn update_ids(
        &mut self,
        io: &dyn QuotaIo,
        ids: &[QuotaId],
        mut update_fn: impl FnMut(&mut Dquot, &Dqinfo) -> KResult<()>,
    ) -> KResult<()> {
        let mut seen = [false; 3];
        for id in ids {
            if core::mem::replace(&mut seen[id.kind().index()], true) {
                return Err(LinuxError::EINVAL.into());
            }
        }
        let mut updated = [None; 3];
        for &id in ids {
            if !self.info(id.kind()).is_some_and(Dqinfo::is_active) {
                continue;
            }
            self.acquire(io, id)?;
            let info = self.active_info(id.kind())?;
            let mut dquot = *self.dquots.get(&id).ok_or(LinuxError::ESRCH)?;
            update_fn(&mut dquot, info)?;
            updated[id.kind().index()] = Some((id, dquot));
        }
        for (id, dquot) in updated.into_iter().flatten() {
            self.dquots.insert(id, dquot);
        }
        Ok(())
    }
}

fn charge_space(
    data: &mut Dqblk,
    info: &Dqinfo,
    bytes: i64,
    now: i64,
    flags: ChargeFlags,
) -> KResult<()> {
    let total = data
        .total_bytes()?
        .checked_add(bytes)
        .ok_or_else(overflow)?;
    if info.state.contains(QuotaState::LIMITS_ENABLED) && data.has_limits() {
        let can_bypass_limits = flags.contains(ChargeFlags::BYPASS_LIMITS);
        let is_hard_exceeded = data.space_hard_limit_bytes != 0
            && total > data.space_hard_limit_bytes
            && !can_bypass_limits;
        let is_above_soft_limit =
            data.space_soft_limit_bytes != 0 && total > data.space_soft_limit_bytes;
        let has_grace_expired = is_above_soft_limit
            && data.space_expires_at != 0
            && now >= data.space_expires_at
            && !can_bypass_limits;
        if is_hard_exceeded || has_grace_expired {
            if !flags.contains(ChargeFlags::NOFAIL) {
                return Err(LinuxError::EDQUOT.into());
            }
        } else if is_above_soft_limit && data.space_expires_at == 0 {
            if flags.contains(ChargeFlags::WARN) {
                data.space_expires_at = now
                    .checked_add(i64::from(info.space_grace_seconds))
                    .ok_or_else(overflow)?;
            } else if !flags.contains(ChargeFlags::NOFAIL) {
                return Err(LinuxError::EDQUOT.into());
            }
        }
    }
    if flags.contains(ChargeFlags::RESERVE) {
        data.reserved_bytes = data
            .reserved_bytes
            .checked_add(bytes)
            .ok_or_else(overflow)?;
    } else {
        data.used_bytes = data.used_bytes.checked_add(bytes).ok_or_else(overflow)?;
    }
    Ok(())
}

fn charge_inode(data: &mut Dqblk, info: &Dqinfo, now: i64, can_bypass_limits: bool) -> KResult<()> {
    let count = data.used_inodes.checked_add(1).ok_or_else(overflow)?;
    if info.state.contains(QuotaState::LIMITS_ENABLED) && data.has_limits() {
        if data.inode_hard_limit != 0 && count > data.inode_hard_limit && !can_bypass_limits {
            return Err(LinuxError::EDQUOT.into());
        }
        if data.inode_soft_limit != 0 && count > data.inode_soft_limit {
            if data.inode_expires_at != 0 && now >= data.inode_expires_at && !can_bypass_limits {
                return Err(LinuxError::EDQUOT.into());
            }
            if data.inode_expires_at == 0 {
                data.inode_expires_at = now
                    .checked_add(i64::from(info.inode_grace_seconds))
                    .ok_or_else(overflow)?;
            }
        }
    }
    data.used_inodes = count;
    Ok(())
}

fn validate_update(data: Dqblk, fields: QuotaFields, format: QuotaFormat) -> KResult<()> {
    for (field, value, maximum) in [
        (
            QuotaFields::SPACE_SOFT,
            data.space_soft_limit_bytes,
            format.max_space_limit_bytes(),
        ),
        (
            QuotaFields::SPACE_HARD,
            data.space_hard_limit_bytes,
            format.max_space_limit_bytes(),
        ),
        (
            QuotaFields::INODE_SOFT,
            data.inode_soft_limit,
            format.max_inode_limit(),
        ),
        (
            QuotaFields::INODE_HARD,
            data.inode_hard_limit,
            format.max_inode_limit(),
        ),
    ] {
        if fields.contains(field) && (value < 0 || value > maximum) {
            return Err(LinuxError::ERANGE.into());
        }
    }
    if (fields.contains(QuotaFields::SPACE_USED) && data.used_bytes < 0)
        || (fields.contains(QuotaFields::INODES) && data.used_inodes < 0)
    {
        return Err(LinuxError::EINVAL.into());
    }
    Ok(())
}

fn apply_update(
    data: &mut Dqblk,
    values: Dqblk,
    fields: QuotaFields,
    info: &Dqinfo,
    now: i64,
) -> KResult<()> {
    for (field, target, value) in [
        (
            QuotaFields::SPACE_SOFT,
            &mut data.space_soft_limit_bytes,
            values.space_soft_limit_bytes,
        ),
        (
            QuotaFields::SPACE_HARD,
            &mut data.space_hard_limit_bytes,
            values.space_hard_limit_bytes,
        ),
        (
            QuotaFields::INODE_SOFT,
            &mut data.inode_soft_limit,
            values.inode_soft_limit,
        ),
        (
            QuotaFields::INODE_HARD,
            &mut data.inode_hard_limit,
            values.inode_hard_limit,
        ),
        (
            QuotaFields::INODES,
            &mut data.used_inodes,
            values.used_inodes,
        ),
        (
            QuotaFields::SPACE_TIME,
            &mut data.space_expires_at,
            values.space_expires_at,
        ),
        (
            QuotaFields::INODE_TIME,
            &mut data.inode_expires_at,
            values.inode_expires_at,
        ),
    ] {
        if fields.contains(field) {
            *target = value;
        }
    }
    if fields.contains(QuotaFields::SPACE_USED) {
        data.used_bytes = values
            .used_bytes
            .checked_sub(data.reserved_bytes)
            .ok_or_else(overflow)?;
    }
    if fields.intersects(
        QuotaFields::SPACE_SOFT
            | QuotaFields::SPACE_HARD
            | QuotaFields::SPACE_USED
            | QuotaFields::SPACE_TIME,
    ) {
        if data.space_soft_limit_bytes == 0 || data.total_bytes()? <= data.space_soft_limit_bytes {
            data.space_expires_at = 0;
        } else if !fields.contains(QuotaFields::SPACE_TIME) {
            data.space_expires_at = now
                .checked_add(i64::from(info.space_grace_seconds))
                .ok_or_else(overflow)?;
        }
    }
    if fields.intersects(
        QuotaFields::INODE_SOFT
            | QuotaFields::INODE_HARD
            | QuotaFields::INODES
            | QuotaFields::INODE_TIME,
    ) {
        if data.inode_soft_limit == 0 || data.used_inodes <= data.inode_soft_limit {
            data.inode_expires_at = 0;
        } else if !fields.contains(QuotaFields::INODE_TIME) {
            data.inode_expires_at = now
                .checked_add(i64::from(info.inode_grace_seconds))
                .ok_or_else(overflow)?;
        }
    }
    Ok(())
}
