// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy authentication and master ownership (`drm_auth.c`).
//!
//! A master owns the identity and the one-shot magic tokens. Tokens are
//! compatibility identifiers, not credentials: they gate nothing on their own,
//! and the dispatcher never consults them. `MASTER` permissions are enforced
//! from the ioctl table.

use alloc::{
    collections::BTreeMap,
    format,
    string::String,
    sync::{Arc, Weak},
};

use crate::prelude::*;

/// One card master: its identity and the tokens it issued.
pub(crate) struct DrmMaster {
    /// Name reported by `GET_UNIQUE`, if `SET_VERSION` negotiated one.
    pub(crate) unique: Mutex<Option<String>>,
    /// Tokens issued to clients, keyed by value.
    pub(crate) magic_registry: Mutex<DrmMagicRegistry>,
}

impl Default for DrmMaster {
    fn default() -> Self {
        Self {
            unique: Mutex::new(None),
            magic_registry: Mutex::new(DrmMagicRegistry::default()),
        }
    }
}

/// One-shot magic tokens of a master.
#[derive(Default)]
pub(crate) struct DrmMagicRegistry {
    next_magic: u32,
    files: BTreeMap<u32, Weak<DrmFile>>,
}

impl DrmMagicRegistry {
    /// Issues the next unused token to `file`.
    pub(crate) fn allocate(&mut self, file: &Arc<DrmFile>) -> Option<u32> {
        let first = self.next_magic.max(1);
        let mut magic = first;
        loop {
            if let alloc::collections::btree_map::Entry::Vacant(entry) = self.files.entry(magic) {
                entry.insert(Arc::downgrade(file));
                self.next_magic = magic.wrapping_add(1).max(1);
                return Some(magic);
            }
            magic = magic.wrapping_add(1).max(1);
            if magic == first {
                return None;
            }
        }
    }
}

/// Invalidates every token of a master that is no longer active.
pub(crate) fn invalidate_magic_tokens(master: &DrmMaster) {
    master.magic_registry.lock().files.clear();
}

/// Card-wide master ownership.
#[derive(Default)]
pub(crate) struct MasterState {
    /// The file that currently owns the card.
    pub(crate) active: Option<ActiveMaster>,
}

/// The active owner and its master object.
pub(crate) struct ActiveMaster {
    /// Master object owned by the active file.
    pub(crate) master: Arc<DrmMaster>,
    /// Weak owner, so ownership does not keep the file alive.
    pub(crate) owner: Weak<DrmFile>,
}

impl DrmDevice {
    /// Acquires ownership for `file`, or succeeds if it already owns the card.
    pub(crate) fn set_master(&self, file: &Arc<DrmFile>) -> DrmResult<()> {
        let mut masters = self.master.lock();
        if let Some(active) = &masters.active {
            return if Weak::ptr_eq(&active.owner, &Arc::downgrade(file)) {
                Ok(())
            } else {
                Err(DrmError::Busy)
            };
        }
        let mut binding = file.auth.lock();
        if !binding.owns_master_object {
            // A former owner reacquires its own master; a client becoming the
            // owner starts with a fresh, nameless one.
            if let Some(magic) = binding.magic.take() {
                binding.master.magic_registry.lock().files.remove(&magic);
            }
            binding.master = Arc::new(DrmMaster::default());
            binding.owns_master_object = true;
        }
        masters.active = Some(ActiveMaster {
            master: binding.master.clone(),
            owner: Arc::downgrade(file),
        });
        Ok(())
    }

    /// Releases ownership, invalidating the tokens of the released master.
    pub(crate) fn drop_master(&self, file: &Arc<DrmFile>) -> DrmResult<()> {
        let mut masters = self.master.lock();
        if !masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
        {
            return Err(DrmError::InvalidInput);
        }
        let released = masters.active.take().map(|active| active.master);
        if let Some(master) = &released {
            invalidate_magic_tokens(master);
        }
        Ok(())
    }

    /// Applies the ownership rollback a closing file needs.
    pub(crate) fn release_master_on_close(&self, file: &Arc<DrmFile>) {
        let mut masters = self.master.lock();
        let released = masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
            .then_some(())
            .and_then(|()| masters.active.take().map(|active| active.master));
        if let Some(master) = &released {
            invalidate_magic_tokens(master);
        }
        drop(masters);

        let binding = file.auth.lock();
        if let Some(magic) = binding.magic {
            binding.master.magic_registry.lock().files.remove(&magic);
        }
    }
}

impl Ioctl for DrmAuth {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, auth: &mut Self) -> DrmResult<usize> {
        let masters = dev.master.lock();
        let mut binding = file.auth.lock();
        if !masters
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&binding.master, &active.master))
        {
            if let Some(magic) = binding.magic.take() {
                let mut registry = binding.master.magic_registry.lock();
                if registry
                    .files
                    .get(&magic)
                    .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(file)))
                {
                    registry.files.remove(&magic);
                }
            }
            return Err(DrmError::PermissionDenied);
        }
        let master = binding.master.clone();
        let mut registry = master.magic_registry.lock();
        if let Some(magic) = binding.magic {
            if registry
                .files
                .get(&magic)
                .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(file)))
            {
                auth.magic = magic;
                return Ok(0);
            }
            binding.magic = None;
        }
        let magic = registry.allocate(file).ok_or(DrmError::NoMemory)?;
        drop(registry);
        binding.magic = Some(magic);
        auth.magic = magic;
        Ok(0)
    }
}

impl Ioctl for DrmAuthMagic {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, auth: &mut Self) -> DrmResult<usize> {
        // Keep active-master ownership stable through token consumption. The
        // target Arc outlives the lock scope so its final drop cannot re-enter
        // `DrmFile` teardown while the master-state lock is held.
        let _target;
        {
            let masters = dev.master.lock();
            let active = masters
                .active
                .as_ref()
                .filter(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
                .ok_or(DrmError::PermissionDenied)?;
            _target = active
                .master
                .magic_registry
                .lock()
                .files
                .remove(&auth.0.magic)
                .and_then(|target| target.upgrade())
                .ok_or(DrmError::InvalidInput)?;
        }
        Ok(0)
    }
}

/// A one-shot auth token request (`DRM_IOCTL_AUTH_MAGIC`).
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmAuthMagic(DrmAuth);

/// Builds the name a master reports to `GET_UNIQUE`.
pub(crate) fn driver_unique_name() -> String {
    format!("{DRIVER_NAME}:0")
}
