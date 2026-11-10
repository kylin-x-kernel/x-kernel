// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The per-cdev handle of a card.
//!
//! `DrmMinor` is the only `DeviceFileOps` implementation of the crate: it is
//! what devfs registers for each minor (`cardN`, `renderD*`) and the only
//! code that touches VFS types. Everything below `io/drmdevice` returns
//! [`DrmError`], which this adapter maps to `VfsError`.

use crate::prelude::*;

/// One character device of a card.
pub(super) struct DrmMinor {
    device: Arc<DrmDevice>,
    kind: MinorKind,
    /// The node's minor number, the identity of the card's `EVENT` unit.
    unit: u32,
}

impl DrmMinor {
    /// Creates the handle for one minor of `device`.
    ///
    /// `unit` is the node's minor number: Linux derives the card's `EVENT`
    /// unit from it, so `cardN` and `renderD*` of one device share it.
    pub(super) fn new(device: Arc<DrmDevice>, kind: MinorKind, unit: u32) -> Self {
        Self { device, kind, unit }
    }

    fn resolve(&self) -> DrmResult<Arc<DrmDevice>> {
        Ok(self.device.clone())
    }
}

impl DeviceFileOps for DrmMinor {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let state = self.resolve()?.open_file(self.kind)?;
        file.set_private_data(state);
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        state.dev.release_master_on_close(&state);
        // `drm_fb_release()`: framebuffers are per-file objects here, so a file
        // that exits without RMFB must not leave them in the device registry
        // holding their GEM objects forever. Removing them unbinds the plane
        // and keeps any scanned resource retained until the next present.
        let owned_fbs: Vec<FbId> = state.fbs.lock().iter().copied().collect();
        for fb in owned_fbs {
            let _ = state.dev.remove_framebuffer(&state, fb);
        }
        // `drm_gem_release()` + `drm_property_destroy_user_blobs()`: closing
        // the file drops every handle and destroys every blob it created.
        // A resource that a framebuffer or the scanout still names stays
        // alive through that reference and is retired with it.
        state.handles.lock().clear();
        // One commit section covers the legacy-slot clear, the device registry
        // and the file's own set; see `destroy_file_blobs`.
        state.dev.destroy_file_blobs(&state);
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(self.resolve()?.read_events(buf)?)
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        let device = self.resolve()?;
        device.backend()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        Ok(crate::ioctl::dispatch(
            &device, &state, self.unit, cmd, arg,
        )?)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let device = self.resolve()?;
        device.backend()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        let offset_bytes = mapper.offset() as u64;
        // The card's offset namespace names the object and must contain the
        // whole mapping; the per-file handle table then decides whether this
        // file may map it, so a render node cannot map a buffer it never
        // received.
        let object = state
            .object_by_offset_range(offset_bytes, PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let range_start = state
            .dev
            .vma
            .lookup(offset_bytes)
            .ok_or(VfsError::InvalidInput)?;
        // `drm_gem_mmap` maps from the client's own offset inside the object.
        let within = offset_bytes - range_start.offset;
        let (phys, size_bytes) = object.memory_range();
        if within >= size_bytes as u64 {
            return Err(VfsError::InvalidInput);
        }
        let phys = memaddr::PhysAddr::from(
            usize::try_from(phys)
                .map_err(|_| VfsError::InvalidInput)?
                .checked_add(within as usize)
                .ok_or(VfsError::InvalidInput)?,
        );
        let range = PhysAddrRange::from_start_size(phys, size_bytes - within as usize);
        // Retain only after the mapper accepts the range: the retention table
        // lives for the device's lifetime, so retaining before a failed mmap
        // (for example an over-long request) would pin the GEM backing forever.
        mapper.map_physical(range)?;
        device.retain_mapping(offset_bytes, object.clone());
        Ok(())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(self.device.poll_events())
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.device.events.register(context, events)
    }
}
