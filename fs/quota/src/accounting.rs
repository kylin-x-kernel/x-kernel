// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-filesystem quota lifecycle and all-or-nothing per-inode charges.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
    vec::Vec,
};

use kerrno::{KResult, LinuxError};
use ksync::Mutex;

use crate::{
    ChargeFlags, Dqblk, QuotaFields, QuotaFormat, QuotaId, QuotaIo, QuotaState, QuotaType,
    overflow,
    tree::QuotaTree,
    v2::{read_info, write_info},
};

/// Loaded information and retained backing file for one quota class.
///
/// `F` is the filesystem's canonical file reference; quota disable returns it
/// to the caller only after pending state has been flushed successfully.
#[derive(Debug)]
pub struct Dqinfo<F> {
    file: F,
    state: QuotaState,
    space_grace_seconds: u32,
    inode_grace_seconds: u32,
    tree: QuotaTree,
}

impl<F> Dqinfo<F> {
    /// Returns the backing-file reference retained until disable succeeds.
    pub fn file(&self) -> &F {
        &self.file
    }

    /// Upper bound on file growth when instantiating one v2 record.
    pub const fn record_growth_bytes_bound(&self) -> u64 {
        crate::tree::record_growth_bytes_bound()
    }

    /// Upper bound on distinct 1-KiB quota blocks dirtied by one record edit.
    /// Includes tree/free-list links and file information, not filesystem extents.
    pub const fn record_write_blocks_bound(&self) -> u32 {
        crate::tree::record_write_blocks_bound()
    }

    /// Returns the loaded quota format.
    pub const fn format(&self) -> QuotaFormat {
        self.tree.format
    }

    /// Returns accounting, enforcement and suspension state.
    pub const fn state(&self) -> QuotaState {
        self.state
    }

    /// Returns whether quota-file information needs writeback.
    pub fn is_dirty(&self) -> bool {
        self.tree.is_dirty
    }

    /// Returns the duration of a newly started space grace period.
    pub const fn space_grace_seconds(&self) -> u32 {
        self.space_grace_seconds
    }

    /// Returns the duration of a newly started inode grace period.
    pub const fn inode_grace_seconds(&self) -> u32 {
        self.inode_grace_seconds
    }

    /// Returns whether this class currently accounts usage.
    pub fn is_active(&self) -> bool {
        self.state.contains(QuotaState::USAGE_ENABLED)
            && !self.state.contains(QuotaState::SUSPENDED)
    }

    fn sync(&mut self, io: &mut dyn QuotaIo<File = F>) -> KResult<()> {
        if self.tree.is_dirty {
            write_info(
                io,
                &self.file,
                self.space_grace_seconds,
                self.inode_grace_seconds,
                &mut self.tree,
            )?;
        }
        Ok(())
    }
}

/// A cached quota record whose state is shared with resident inodes.
#[derive(Clone, Copy, Debug)]
pub struct Dquot {
    id: QuotaId,
    data: Dqblk,
    offset: u64,
    is_dirty: bool,
    is_active: bool,
}

/// Shared ownership of one cached quota record.
pub type DquotRef = Arc<Mutex<Dquot>>;

impl Dquot {
    /// Returns this record's quota identity.
    pub const fn id(&self) -> QuotaId {
        self.id
    }

    fn alloc_space<F>(
        &mut self,
        info: &Dqinfo<F>,
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        charge_space(&mut self.data, info, bytes, now_seconds, flags)?;
        if !flags.contains(ChargeFlags::RESERVE) {
            self.is_dirty = true;
        }
        Ok(())
    }

    fn free_space(&mut self, bytes: i64, is_reserved: bool) -> KResult<()> {
        if is_reserved {
            self.data.reserved_bytes = self.data.reserved_bytes.saturating_sub(bytes).max(0);
        } else {
            self.data.used_bytes = self.data.used_bytes.saturating_sub(bytes).max(0);
            self.is_dirty = true;
        }
        if self.data.total_bytes()? <= self.data.space_soft_limit_bytes {
            self.data.space_expires_at = 0;
        }
        Ok(())
    }

    fn claim_space(&mut self, bytes: i64) -> KResult<()> {
        if self.data.reserved_bytes < bytes {
            return Err(LinuxError::EINVAL.into());
        }
        self.data.used_bytes = self
            .data
            .used_bytes
            .checked_add(bytes)
            .ok_or_else(overflow)?;
        self.data.reserved_bytes -= bytes;
        self.is_dirty = true;
        Ok(())
    }

    fn free_inode(&mut self) {
        self.data.used_inodes = self.data.used_inodes.saturating_sub(1).max(0);
        if self.data.used_inodes <= self.data.inode_soft_limit {
            self.data.inode_expires_at = 0;
        }
        self.is_dirty = true;
    }

    fn writeback<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        id: QuotaId,
        info: &mut Dqinfo<F>,
    ) -> KResult<()> {
        if !self.is_active {
            // Initialization owns structural allocation. Report this failed
            // acquisition once without trapping class teardown in writeback.
            self.is_dirty = false;
            return Err(LinuxError::EIO.into());
        }
        self.commit(io, id, info)
    }

    fn commit<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        id: QuotaId,
        info: &mut Dqinfo<F>,
    ) -> KResult<()> {
        if self.is_dirty || self.offset == 0 {
            self.is_dirty = true;
            info.tree
                .write(io, &info.file, id.id(), &mut self.offset, self.data)?;
            self.is_dirty = false;
        }
        Ok(())
    }
}

/// Canonical quota state and cached dquots for one filesystem.
///
/// Mutation requires exclusive access, including across quota-file I/O. The
/// containing filesystem provides a sleepable lock. Each loaded class owns its
/// canonical backing-file reference until disable succeeds. Resident inodes
/// hold references to the same records as the cache.
#[derive(Debug)]
pub struct QuotaInfo<F> {
    info: [Option<Dqinfo<F>>; 3],
    dquots: BTreeMap<QuotaId, DquotRef>,
}

impl<F> Default for QuotaInfo<F> {
    fn default() -> Self {
        Self {
            info: core::array::from_fn(|_| None),
            dquots: BTreeMap::new(),
        }
    }
}

impl<F> QuotaInfo<F> {
    /// Creates quota state with all classes disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of resident quota records.
    pub fn cached_records(&self) -> usize {
        self.dquots.len()
    }

    /// Loads a pre-existing quota file, enabling accounting and optionally limits.
    ///
    /// Only USAGE_ENABLED and LIMITS_ENABLED are accepted here; usage must be
    /// enabled. `file` is retained after header validation and supplied to all later
    /// I/O for this class. Failure leaves the class disabled and takes no reference.
    /// The caller retains `file` until after leaving its filesystem locks.
    pub fn enable(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        kind: QuotaType,
        file: &F,
        format: QuotaFormat,
        state: QuotaState,
    ) -> KResult<()>
    where
        F: Clone,
    {
        if self.info[kind.index()].is_some() {
            return Err(LinuxError::EBUSY.into());
        }
        let allowed = QuotaState::USAGE_ENABLED | QuotaState::LIMITS_ENABLED;
        if !state.contains(QuotaState::USAGE_ENABLED) || state.bits() & !allowed.bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let (space_grace_seconds, inode_grace_seconds, tree) = read_info(io, file, kind, format)?;
        self.info[kind.index()] = Some(Dqinfo {
            file: file.clone(),
            state,
            space_grace_seconds,
            inode_grace_seconds,
            tree,
        });
        Ok(())
    }

    /// Returns loaded file information, including when suspended.
    pub fn info(&self, kind: QuotaType) -> Option<&Dqinfo<F>> {
        self.info[kind.index()].as_ref()
    }

    /// Stops new accounting before inode references and records are released.
    ///
    /// The caller serializes class lifecycle transitions. Outstanding inode
    /// reservations remain with their inodes; they no longer belong to this
    /// class and are not stored in its backing file.
    ///
    /// # Errors
    /// Returns ESRCH if the class is not loaded.
    pub fn deactivate(&mut self, kind: QuotaType) -> KResult<()> {
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        info.state
            .remove(QuotaState::USAGE_ENABLED | QuotaState::LIMITS_ENABLED);
        for (id, record) in &self.dquots {
            if id.kind() == kind {
                record.lock().data.reserved_bytes = 0;
            }
        }
        Ok(())
    }

    /// Flushes and disables a quota class, discarding its record cache.
    ///
    /// Outstanding reservations or inode references return EBUSY. The
    /// filesystem must detach records before removing its accounting source. I/O errors
    /// retain the loaded state and all dirty records for diagnosis or retry.
    /// Returns the retained file for release after filesystem/quota locks are
    /// dropped: final inode release may reenter filesystem eviction.
    pub fn disable(&mut self, io: &mut dyn QuotaIo<File = F>, kind: QuotaType) -> KResult<F> {
        self.loaded_info(kind)?;
        self.check_no_inode_references(kind)?;
        self.check_no_reservations(kind)?;
        self.sync(io, kind)?;
        let unused: Vec<_> = self
            .dquots
            .iter()
            .filter_map(|(&id, dquot)| {
                (id.kind() == kind && dquot.lock().data.is_unused()).then_some(id)
            })
            .collect();
        for id in unused {
            self.release(io, id)?;
        }
        self.dquots.retain(|id, _| id.kind() != kind);
        self.unload(kind)
    }

    /// Returns the backing file after bounded record release has completed.
    ///
    /// Performs no I/O. Cached records or dirty file information return EBUSY;
    /// an unloaded class returns ESRCH. Filesystems call this only after their
    /// last release transaction has stopped, and drop the file outside locks.
    pub fn unload(&mut self, kind: QuotaType) -> KResult<F> {
        let info = self.loaded_info(kind)?;
        if info.is_dirty() || self.dquots.keys().any(|id| id.kind() == kind) {
            return Err(LinuxError::EBUSY.into());
        }
        self.invalidate(kind)
            .ok_or_else(|| LinuxError::ESRCH.into())
    }

    /// Releases a quiesced class and its cache without backing-file I/O.
    ///
    /// The provider must first detach inode references and synchronize the class
    /// or irreversibly abort its mutation domain and preserve on-disk recovery
    /// evidence. Dirty usage and
    /// reservations are discarded; this does not make them durable. Returns the
    /// retained file for release outside provider locks, or None if unloaded.
    pub fn invalidate(&mut self, kind: QuotaType) -> Option<F> {
        let info = self.info[kind.index()].take()?;
        self.dquots.retain(|id, _| id.kind() != kind);
        Some(info.file)
    }

    /// Flushes a loaded class before suspending its accounting and limits.
    pub fn suspend(&mut self, io: &mut dyn QuotaIo<File = F>, kind: QuotaType) -> KResult<()> {
        self.active_info(kind)?;
        self.check_no_reservations(kind)?;
        self.sync(io, kind)?;
        self.suspend_cached(kind)
    }

    /// Pauses a loaded class without I/O, retaining its file and dirty active records.
    ///
    /// The filesystem drains writeback before publishing a successful read-only
    /// transition. A failed resume may retain dirty state for recovery. Reserved
    /// bytes or inode references return EBUSY; an unloaded class returns ESRCH.
    /// Repeated calls are harmless.
    pub fn suspend_cached(&mut self, kind: QuotaType) -> KResult<()> {
        self.loaded_info(kind)?;
        self.check_no_inode_references(kind)?;
        self.check_no_reservations(kind)?;
        if let Some(info) = &mut self.info[kind.index()] {
            info.state.insert(QuotaState::SUSPENDED);
        }
        self.dquots.retain(|id, dquot| {
            if id.kind() != kind {
                return true;
            }
            let dquot = dquot.lock();
            dquot.is_active && dquot.is_dirty
        });
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
        io: &mut dyn QuotaIo<File = F>,
        kind: QuotaType,
        space_seconds: u32,
        inode_seconds: u32,
    ) -> KResult<()> {
        self.active_info(kind)?;
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        info.space_grace_seconds = space_seconds;
        info.inode_grace_seconds = inode_seconds;
        info.tree.is_dirty = true;
        info.sync(io)
    }

    /// Loads a record into the canonical cache without allocating its storage.
    ///
    /// Existing on-disk records become active. An absent record remains inactive
    /// until acquisition creates its backing storage. No writes are issued;
    /// disabled or suspended classes return ESRCH.
    pub fn read_record(&mut self, io: &mut dyn QuotaIo<File = F>, id: QuotaId) -> KResult<()> {
        self.active_info(id.kind())?;
        let info = self.info[id.kind().index()]
            .as_ref()
            .ok_or(LinuxError::ESRCH)?;
        if let Entry::Vacant(entry) = self.dquots.entry(id) {
            let (offset, data) = info.tree.read(io, &info.file, id.id())?;
            entry.insert(Arc::new(Mutex::new(Dquot {
                id,
                data,
                offset,
                is_dirty: false,
                is_active: offset != 0,
            })));
        }
        Ok(())
    }

    /// Acquires a record and returns its native usage snapshot.
    ///
    /// Missing records are instantiated on disk before acquisition completes.
    /// Obtaining a snapshot can therefore require writes; I/O errors propagate.
    /// Disabled or suspended classes return ESRCH. Use Dqblk::total_bytes for
    /// quota-control space usage, which includes reservations.
    pub fn get(&mut self, io: &mut dyn QuotaIo<File = F>, id: QuotaId) -> KResult<Dqblk> {
        self.acquire(io, id)?;
        self.dquots
            .get(&id)
            .map(|dquot| dquot.lock().data)
            .ok_or(LinuxError::ESRCH.into())
    }

    /// Returns the smallest on-disk ID at least minimum, without wrapping.
    pub fn next_id(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        kind: QuotaType,
        minimum: u32,
    ) -> KResult<Option<QuotaId>> {
        let info = self.active_info(kind)?;
        info.tree
            .next_id(io, &info.file, minimum)?
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
        io: &mut dyn QuotaIo<File = F>,
        id: QuotaId,
        values: Dqblk,
        fields: QuotaFields,
        now_seconds: i64,
    ) -> KResult<()> {
        let info = self.active_info(id.kind())?;
        if fields.bits() & !QuotaFields::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        validate_update(values, fields, info.tree.format)?;
        self.acquire(io, id)?;
        let info = self.active_info(id.kind())?;
        let record = self.dquots.get(&id).ok_or(LinuxError::ESRCH)?;
        let mut dquot = *record.lock();
        apply_update(&mut dquot.data, values, fields, info, now_seconds)?;
        dquot.is_dirty = true;
        *record.lock() = dquot;
        Ok(())
    }

    /// Returns whether acquisition has completed for this cached record.
    pub fn is_cached_active(&self, id: QuotaId) -> bool {
        self.dquots
            .get(&id)
            .is_some_and(|record| record.lock().is_active)
    }

    /// Binds acquired records to an inode and imports its current reservation.
    ///
    /// The caller holds the inode's reservation lock. Bound slots are retained,
    /// and disabled classes are skipped. Every active class must have an
    /// acquired record. A failed operation changes no slot or reservation.
    ///
    /// # Errors
    /// Returns EINVAL for an invalid ID set or reservation, ESRCH for a missing
    /// record, and EOVERFLOW if a sum cannot be represented.
    pub fn bind_cached(
        &mut self,
        ids: &[QuotaId],
        reserved_bytes: i64,
        records: &mut [Option<DquotRef>; 3],
    ) -> KResult<()> {
        if reserved_bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let mut seen = [false; 3];
        let mut pending = [None; 3];
        for &id in ids {
            let index = id.kind().index();
            if core::mem::replace(&mut seen[index], true) {
                return Err(LinuxError::EINVAL.into());
            }
            if let Some(bound) = &records[index] {
                if bound.lock().id != id
                    || (self.info(id.kind()).is_some_and(Dqinfo::is_active)
                        && !self
                            .dquots
                            .get(&id)
                            .is_some_and(|cached| Arc::ptr_eq(cached, bound)))
                {
                    return Err(LinuxError::EINVAL.into());
                }
                continue;
            }
            if !self.info(id.kind()).is_some_and(Dqinfo::is_active) {
                continue;
            }
            let record = self.dquots.get(&id).ok_or(LinuxError::ESRCH)?;
            let mut dquot = *record.lock();
            if !dquot.is_active {
                return Err(LinuxError::ESRCH.into());
            }
            dquot.data.reserved_bytes = dquot
                .data
                .reserved_bytes
                .checked_add(reserved_bytes)
                .ok_or_else(overflow)?;
            pending[index] = Some((record, dquot));
        }
        for (index, update) in pending.into_iter().enumerate() {
            if let Some((record, dquot)) = update {
                *record.lock() = dquot;
                records[index] = Some(record.clone());
            }
        }
        Ok(())
    }

    /// Returns an acquired record for an active class without changing usage.
    ///
    /// # Errors
    /// Returns ESRCH if an active class has no acquired record for `id`.
    pub fn cached_ref(&self, id: QuotaId) -> KResult<Option<DquotRef>> {
        if !self.info(id.kind()).is_some_and(Dqinfo::is_active) {
            return Ok(None);
        }
        let record = self.dquots.get(&id).ok_or(LinuxError::ESRCH)?;
        if !record.lock().is_active {
            return Err(LinuxError::ESRCH.into());
        }
        Ok(Some(record.clone()))
    }

    /// Reserves against the records currently bound to an inode.
    ///
    /// Empty slots and inactive classes do not participate. The caller holds
    /// the inode's reservation lock through the corresponding inode update.
    /// Records must have been bound by this quota owner for the same inode.
    ///
    /// # Errors
    /// Returns EINVAL for invalid inputs, EDQUOT for a limit, or EOVERFLOW
    /// if accounting cannot represent the result.
    pub fn reserve_space_refs(
        &mut self,
        records: &[Option<DquotRef>; 3],
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        if bytes < 0 || flags.bits() & !ChargeFlags::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_refs(records, |dquot, info| {
            dquot.alloc_space(info, bytes, now_seconds, flags | ChargeFlags::RESERVE)
        })
    }

    /// Releases reservations against the records currently bound to an inode.
    ///
    /// # Errors
    /// Returns EINVAL for invalid inputs or EOVERFLOW for invalid accounting.
    pub fn release_space_refs(
        &mut self,
        records: &[Option<DquotRef>; 3],
        bytes: i64,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_refs(records, |dquot, _| dquot.free_space(bytes, true))
    }

    /// Charges allocated space to the records currently bound to an inode.
    ///
    /// # Errors
    /// Returns EINVAL for invalid inputs, EDQUOT for a limit, or EOVERFLOW
    /// if accounting cannot represent the result.
    pub fn alloc_space_refs(
        &mut self,
        records: &[Option<DquotRef>; 3],
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        if bytes < 0 || flags.bits() & !ChargeFlags::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_refs(records, |dquot, info| {
            dquot.alloc_space(info, bytes, now_seconds, flags)
        })
    }

    /// Releases allocated space from the records currently bound to an inode.
    ///
    /// # Errors
    /// Returns EINVAL for invalid inputs or EOVERFLOW for invalid accounting.
    pub fn free_space_refs(&mut self, records: &[Option<DquotRef>; 3], bytes: i64) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_refs(records, |dquot, _| dquot.free_space(bytes, false))
    }

    /// Validates a space release without changing the bound quota records.
    ///
    /// The caller must serialize the validation and the eventual release.
    ///
    /// # Errors
    /// Returns the same input or accounting errors as [`Self::free_space_refs`].
    pub fn validate_free_space_refs(
        &self,
        records: &[Option<DquotRef>; 3],
        bytes: i64,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.prepare_refs(records, |dquot, _| dquot.free_space(bytes, false))?;
        Ok(())
    }

    /// Converts an inode's existing reservations to allocated space.
    ///
    /// # Errors
    /// Returns EINVAL for invalid inputs or insufficient reservations, or
    /// EOVERFLOW if accounting cannot represent the result.
    pub fn claim_space_refs(&mut self, records: &[Option<DquotRef>; 3], bytes: i64) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_refs(records, |dquot, _| dquot.claim_space(bytes))
    }

    /// Releases one inode from the records currently bound to it.
    ///
    /// # Errors
    /// Returns EINVAL for an invalid record slot.
    pub fn free_inode_refs(&mut self, records: &[Option<DquotRef>; 3]) -> KResult<()> {
        self.update_refs(records, |dquot, _| {
            dquot.free_inode();
            Ok(())
        })
    }

    /// Reserves bytes using already acquired records, without backing-file I/O.
    ///
    /// The filesystem initializes records before taking its inode reservation
    /// lock. Missing records in active classes return ESRCH atomically.
    pub fn reserve_space_cached(
        &mut self,
        ids: &[QuotaId],
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        if bytes < 0 || flags.bits() & !ChargeFlags::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(None, ids, |dquot, info| {
            dquot.alloc_space(info, bytes, now_seconds, flags | ChargeFlags::RESERVE)
        })
    }

    /// Releases cached reservations without I/O; disabled classes are skipped.
    pub fn release_space_cached(&mut self, ids: &[QuotaId], bytes: i64) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(None, ids, |dquot, _| dquot.free_space(bytes, true))
    }

    /// Charges bytes to at most one ID per quota class.
    ///
    /// Disabled/suspended classes are skipped. Admission is atomic across the
    /// supplied IDs: a rejected charge changes no usage or grace deadlines.
    /// Caller authorization is expressed by BYPASS_LIMITS, never inferred here.
    pub fn alloc_space(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        ids: &[QuotaId],
        bytes: i64,
        now_seconds: i64,
        flags: ChargeFlags,
    ) -> KResult<()> {
        if bytes < 0 || flags.bits() & !ChargeFlags::all().bits() != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(Some(io), ids, |dquot, info| {
            dquot.alloc_space(info, bytes, now_seconds, flags)
        })
    }

    /// Charges one inode to each supplied active quota class.
    pub fn alloc_inode(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        ids: &[QuotaId],
        now_seconds: i64,
        can_bypass_limits: bool,
    ) -> KResult<()> {
        self.update_ids(Some(io), ids, |dquot, info| {
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
        io: &mut dyn QuotaIo<File = F>,
        ids: &[QuotaId],
        bytes: i64,
        is_reserved: bool,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(Some(io), ids, |dquot, _| {
            dquot.free_space(bytes, is_reserved)
        })
    }

    /// Releases one inode, stopping at zero and resetting an ended grace period.
    pub fn free_inode(&mut self, io: &mut dyn QuotaIo<File = F>, ids: &[QuotaId]) -> KResult<()> {
        self.update_ids(Some(io), ids, |dquot, _| {
            dquot.free_inode();
            Ok(())
        })
    }

    /// Converts already admitted reservations to allocated bytes without rechecking limits.
    ///
    /// An amount exceeding any active record's reservation is an accounting
    /// error and returns EINVAL before changing any record.
    pub fn claim_space(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        ids: &[QuotaId],
        bytes: i64,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(Some(io), ids, |dquot, _| dquot.claim_space(bytes))
    }

    /// Converts allocated bytes back into reservations without changing total usage.
    ///
    /// An amount exceeding any active record's allocated bytes returns EINVAL
    /// before changing any record. Signed administrative reconciliation does
    /// not create allocated space that this conversion can transfer.
    pub fn reclaim_space(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        ids: &[QuotaId],
        bytes: i64,
    ) -> KResult<()> {
        if bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        self.update_ids(Some(io), ids, |dquot, _| {
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

    /// Transfers one inode and its allocated/reserved bytes between owners.
    ///
    /// `owners` contains the source and target ID sets, in that order.
    /// Admission is atomic across classes and both owner sets. The caller
    /// serializes ownership and reservation changes on the inode. Acquiring
    /// records may instantiate empty disk entries before admission.
    /// A short source reservation is warned about and clamped to zero; the
    /// target still admits the full transferred reservation.
    /// Invalid ID sets or negative byte counts return EINVAL; new-owner limits
    /// return EDQUOT, arithmetic overflow returns EOVERFLOW, and I/O errors propagate.
    pub fn transfer(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        owners: (&[QuotaId], &[QuotaId]),
        allocated_bytes: i64,
        reserved_bytes: i64,
        now_seconds: i64,
        can_bypass_limits: bool,
    ) -> KResult<()> {
        if allocated_bytes < 0 || reserved_bytes < 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let (from, to) = owners;
        let mut source = [None; 3];
        let mut target = [None; 3];
        for (ids, slots) in [(from, &mut source), (to, &mut target)] {
            for &id in ids {
                if slots[id.kind().index()].replace(id).is_some() {
                    return Err(LinuxError::EINVAL.into());
                }
            }
        }
        let mut updated = [None; 6];
        for kind in QuotaType::ALL {
            let index = kind.index();
            if source[index] == target[index] {
                continue;
            }
            let (Some(old), Some(new)) = (source[index], target[index]) else {
                return Err(LinuxError::EINVAL.into());
            };
            if !self.info(kind).is_some_and(Dqinfo::is_active) {
                continue;
            }
            self.acquire(io, old)?;
            self.acquire(io, new)?;
            let mut previous = *self.dquots.get(&old).ok_or(LinuxError::ESRCH)?.lock();
            let mut next = *self.dquots.get(&new).ok_or(LinuxError::ESRCH)?.lock();
            let info = self.active_info(kind)?;
            let mut flags = ChargeFlags::WARN;
            flags.set(ChargeFlags::BYPASS_LIMITS, can_bypass_limits);
            charge_inode(&mut next.data, info, now_seconds, can_bypass_limits)?;
            charge_space(&mut next.data, info, allocated_bytes, now_seconds, flags)?;
            charge_space(
                &mut next.data,
                info,
                reserved_bytes,
                now_seconds,
                flags | ChargeFlags::RESERVE,
            )?;
            previous.data.used_inodes = previous.data.used_inodes.saturating_sub(1).max(0);
            previous.data.used_bytes = previous
                .data
                .used_bytes
                .saturating_sub(allocated_bytes)
                .max(0);
            if previous.data.reserved_bytes < reserved_bytes {
                log::warn!(
                    "quota reservation underflow for {old:?}: reserved={}, \
                     transferred={reserved_bytes}",
                    previous.data.reserved_bytes
                );
            }
            previous.data.reserved_bytes = previous
                .data
                .reserved_bytes
                .saturating_sub(reserved_bytes)
                .max(0);
            if previous.data.total_bytes()? <= previous.data.space_soft_limit_bytes {
                previous.data.space_expires_at = 0;
            }
            if previous.data.used_inodes <= previous.data.inode_soft_limit {
                previous.data.inode_expires_at = 0;
            }
            previous.is_dirty = true;
            next.is_dirty = true;
            updated[index * 2] = Some((old, previous));
            updated[index * 2 + 1] = Some((new, next));
        }
        for (id, record) in updated.into_iter().flatten() {
            *self.dquots.get(&id).ok_or(LinuxError::ESRCH)?.lock() = record;
        }
        Ok(())
    }

    /// Returns the next cached ID, exclusively after `after`.
    /// Used by filesystems to release records in bounded transactions.
    pub fn next_cached_id(&self, after: Option<QuotaId>) -> Option<QuotaId> {
        use core::ops::Bound::{Excluded, Unbounded};
        self.dquots
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .map(|(&id, _)| id)
    }

    /// Returns the next dirty cached ID, exclusively after `after`.
    ///
    /// A filesystem can use this cursor to give each record its own bounded
    /// transaction. Advancing past failed IDs preserves best-effort writeback
    /// without copying the record cache or retrying one failed ID forever.
    pub fn next_dirty_id(&self, after: Option<QuotaId>) -> Option<QuotaId> {
        use core::ops::Bound::{Excluded, Unbounded};

        let start = after.map_or(Unbounded, Excluded);
        self.dquots
            .range((start, Unbounded))
            .find_map(|(&id, dquot)| dquot.lock().is_dirty.then_some(id))
    }

    /// Writes one cached dirty record within the caller's transaction.
    ///
    /// Does not acquire a missing record or flush file information. Structural
    /// updates still require the caller's abort/recovery policy on I/O failure.
    /// A missing or clean cached ID needs no write. An inactive dirty record
    /// returns EIO without I/O; only acquisition may finish initialization.
    /// An unloaded class returns ESRCH; backing-file errors propagate unchanged.
    pub fn sync_dquot(&mut self, io: &mut dyn QuotaIo<File = F>, id: QuotaId) -> KResult<()> {
        let info = self.info[id.kind().index()]
            .as_mut()
            .ok_or(LinuxError::ESRCH)?;
        if let Some(dquot) = self.dquots.get(&id) {
            let mut dquot = dquot.lock();
            if dquot.is_dirty {
                dquot.writeback(io, id, info)?;
            }
        }
        Ok(())
    }

    /// Writes only dirty file information within the caller's transaction.
    ///
    /// An unloaded class returns ESRCH. Write errors retain dirty information
    /// and propagate unchanged.
    pub fn sync_info(&mut self, io: &mut dyn QuotaIo<File = F>, kind: QuotaType) -> KResult<()> {
        self.info[kind.index()]
            .as_mut()
            .ok_or(LinuxError::ESRCH)?
            .sync(io)
    }

    /// Writes dirty records and file information; the filesystem makes them durable.
    ///
    /// Attempts every dirty record and then file information, preserving the
    /// first error. Failed active records and file information retain dirty state.
    /// Inactive records report their initialization error once and clear dirty state.
    pub fn sync(&mut self, io: &mut dyn QuotaIo<File = F>, kind: QuotaType) -> KResult<()> {
        let info = self.info[kind.index()].as_mut().ok_or(LinuxError::ESRCH)?;
        let mut result = Ok(());
        for (id, dquot) in &self.dquots {
            let mut dquot = dquot.lock();
            if id.kind() != kind || !dquot.is_dirty {
                continue;
            }
            let write_result = dquot.writeback(io, *id, info);
            result = result.and(write_result);
        }
        let info_result = info.sync(io);
        result.and(info_result)
    }

    /// Drops a cached record, deleting its disk entry when no limits or usage remain.
    ///
    /// Reservations or inode references return EBUSY. Later operations can
    /// reacquire a released record by ID. Failed
    /// writes/deletes retain the record and any updated disk offset.
    /// A structural deletion error requires filesystem abort/recovery before
    /// further use; retaining the record does not make that deletion retryable.
    pub fn release(&mut self, io: &mut dyn QuotaIo<File = F>, id: QuotaId) -> KResult<()> {
        self.loaded_info(id.kind())?;
        let Some(record) = self.dquots.get(&id) else {
            return Ok(());
        };
        if Arc::strong_count(record) != 1 {
            return Err(LinuxError::EBUSY.into());
        }
        let dquot = *record.lock();
        if dquot.data.reserved_bytes != 0 {
            return Err(LinuxError::EBUSY.into());
        }
        if !dquot.data.is_unused() {
            let info = self.info[id.kind().index()]
                .as_mut()
                .ok_or(LinuxError::ESRCH)?;
            let mut record = record.lock();
            let result = record.commit(io, id, info);
            let info_result = info.sync(io);
            result.and(info_result)?;
            drop(record);
            self.dquots.remove(&id);
            return Ok(());
        }
        let info = self.info[id.kind().index()]
            .as_mut()
            .ok_or(LinuxError::ESRCH)?;
        let mut record = record.lock();
        let result = info
            .tree
            .delete(io, &info.file, id.id(), &mut record.offset);
        if record.offset == 0 {
            record.is_active = false;
        }
        let info_result = info.sync(io);
        result.and(info_result)?;
        drop(record);
        self.dquots.remove(&id);
        Ok(())
    }

    fn loaded_info(&self, kind: QuotaType) -> KResult<&Dqinfo<F>> {
        self.info[kind.index()]
            .as_ref()
            .ok_or(LinuxError::ESRCH.into())
    }

    fn active_info(&self, kind: QuotaType) -> KResult<&Dqinfo<F>> {
        let info = self.loaded_info(kind)?;
        if !info.is_active() {
            return Err(LinuxError::ESRCH.into());
        }
        Ok(info)
    }

    /// Checks whether a class can relinquish accounting without losing reservations.
    /// Returns EBUSY while any cached record owns reserved bytes; performs no I/O.
    pub fn check_no_reservations(&self, kind: QuotaType) -> KResult<()> {
        if self
            .dquots
            .iter()
            .any(|(id, record)| id.kind() == kind && record.lock().data.reserved_bytes != 0)
        {
            return Err(LinuxError::EBUSY.into());
        }
        Ok(())
    }

    fn check_no_inode_references(&self, kind: QuotaType) -> KResult<()> {
        if self
            .dquots
            .iter()
            .any(|(id, record)| id.kind() == kind && Arc::strong_count(record) != 1)
        {
            return Err(LinuxError::EBUSY.into());
        }
        Ok(())
    }

    fn acquire(&mut self, io: &mut dyn QuotaIo<File = F>, id: QuotaId) -> KResult<()> {
        self.read_record(io, id)?;
        let (info, records) = (&mut self.info[id.kind().index()], &mut self.dquots);
        let info = info.as_mut().ok_or(LinuxError::ESRCH)?;
        let mut dquot = records.get(&id).ok_or(LinuxError::ESRCH)?.lock();
        if dquot.is_active {
            return Ok(());
        }
        let result = dquot.commit(io, id, info);
        let info_result = info.sync(io);
        result.and(info_result)?;
        dquot.is_active = true;
        Ok(())
    }

    fn update_ids(
        &mut self,
        mut io: Option<&mut dyn QuotaIo<File = F>>,
        ids: &[QuotaId],
        mut update_fn: impl FnMut(&mut Dquot, &Dqinfo<F>) -> KResult<()>,
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
            if let Some(io) = io.as_deref_mut() {
                self.acquire(io, id)?;
            } else if !self.is_cached_active(id) {
                return Err(LinuxError::ESRCH.into());
            }
            let info = self.active_info(id.kind())?;
            let mut dquot = *self.dquots.get(&id).ok_or(LinuxError::ESRCH)?.lock();
            update_fn(&mut dquot, info)?;
            updated[id.kind().index()] = Some((id, dquot));
        }
        for (id, dquot) in updated.into_iter().flatten() {
            *self.dquots.get(&id).ok_or(LinuxError::ESRCH)?.lock() = dquot;
        }
        Ok(())
    }

    fn update_refs(
        &mut self,
        records: &[Option<DquotRef>; 3],
        update_fn: impl FnMut(&mut Dquot, &Dqinfo<F>) -> KResult<()>,
    ) -> KResult<()> {
        for (record, dquot) in self.prepare_refs(records, update_fn)?.into_iter().flatten() {
            *record.lock() = dquot;
        }
        Ok(())
    }

    fn prepare_refs<'a>(
        &self,
        records: &'a [Option<DquotRef>; 3],
        mut update_fn: impl FnMut(&mut Dquot, &Dqinfo<F>) -> KResult<()>,
    ) -> KResult<[Option<(&'a DquotRef, Dquot)>; 3]> {
        let mut updated = [None; 3];
        for kind in QuotaType::ALL {
            let index = kind.index();
            let Some(record) = &records[index] else {
                continue;
            };
            let Some(info) = self.info(kind).filter(|info| info.is_active()) else {
                continue;
            };
            let mut dquot = *record.lock();
            if dquot.id.kind() != kind || !dquot.is_active {
                return Err(LinuxError::EINVAL.into());
            }
            update_fn(&mut dquot, info)?;
            updated[index] = Some((record, dquot));
        }
        Ok(updated)
    }
}

fn charge_space<F>(
    data: &mut Dqblk,
    info: &Dqinfo<F>,
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

fn charge_inode<F>(
    data: &mut Dqblk,
    info: &Dqinfo<F>,
    now: i64,
    can_bypass_limits: bool,
) -> KResult<()> {
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

fn apply_update<F>(
    data: &mut Dqblk,
    values: Dqblk,
    fields: QuotaFields,
    info: &Dqinfo<F>,
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
