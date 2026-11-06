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
}

impl DrmMinor {
    /// Creates the handle for one minor of `device`.
    pub(super) fn new(device: Arc<DrmDevice>, kind: MinorKind) -> Self {
        Self { device, kind }
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
        Ok(crate::ioctl::dispatch(&device, &state, cmd, arg)?)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let device = self.resolve()?;
        device.backend()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        let offset_bytes = mapper.offset() as u64;
        // The offset table is per open file: a render node cannot map a buffer
        // it never created, and a handle is the only way to name the buffer.
        let (_, object) = state
            .handle_by_offset_bytes(offset_bytes)
            .ok_or(VfsError::InvalidInput)?;
        let pages = object.pages();
        let phys = pages.phys_addr();
        let size_bytes = object.length_bytes();
        let range = PhysAddrRange::from_start_size(phys, size_bytes);
        // Retain only after the mapper accepts the range: the retention table
        // lives for the device's lifetime, so retaining before a failed mmap
        // (for example an over-long request) would pin the GEM backing forever.
        mapper.map_physical(range)?;
        device.retain_mapping(offset_bytes, pages);
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
