// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Property blobs (`drm_property.c`).
//!
//! System blobs (the IN_FORMATS table) and client blobs live in one registry;
//! the `system` flag is the only difference, and it is what `DESTROY_BLOB`
//! refuses. The mode blob a legacy `SETCRTC` builds is an ordinary client blob,
//! so `GET_BLOB` needs no special case.

use alloc::sync::Arc;

use crate::prelude::*;

/// One registered property blob.
#[derive(Clone)]
pub(crate) struct Blob {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) system: bool,
}

impl Blob {
    /// Bytes of the blob.
    pub(crate) fn bytes(&self) -> Arc<Vec<u8>> {
        self.bytes.clone()
    }

    /// Whether the blob is driver-owned and thus not destroyable.
    pub(crate) const fn is_system(&self) -> bool {
        self.system
    }
}

impl Ioctl for DrmModeCreateBlob {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, c: &mut Self) -> DrmResult<usize> {
        if c.length == 0 || c.length as usize > MAX_BLOB_BYTES {
            return Err(DrmError::InvalidInput);
        }
        let bytes = c
            .data
            .load_vm_vec(c.length as usize)
            .map_err(|_| DrmError::BadAddress)?;
        let (id, _) = dev.create_blob(Some(file), Arc::new(bytes), false)?;
        c.blob_id = id.raw();
        Ok(0)
    }
}

impl Ioctl for DrmModeDestroyBlob {
    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, d: &mut Self) -> DrmResult<usize> {
        // The legacy-slot clear, the device-registry removal and this file's
        // bookkeeping are one commit-section operation (see
        // `destroy_client_blob`).
        let id = BlobId::from_raw(d.blob_id);
        dev.destroy_client_blob(Some(file), id)?;
        Ok(0)
    }
}

impl Ioctl for DrmModeGetBlob {
    fn handle(dev: &DrmDevice, _file: &Arc<DrmFile>, g: &mut Self) -> DrmResult<usize> {
        let bytes = dev
            .blob_bytes(BlobId::from_raw(g.blob_id))
            .ok_or(DrmError::NotFound)?;
        let bytes = bytes.as_slice();
        // Linux copies only when the caller supplied exactly the blob's
        // length.  A zero/short/oversized length is a size probe: report the
        // required length, copy nothing and still succeed.
        if (g.length as usize) == bytes.len() && !bytes.is_empty() {
            g.data
                .write_vm_slice(bytes)
                .map_err(|_| DrmError::BadAddress)?;
        }
        g.length = bytes.len() as u32;
        Ok(0)
    }
}

/// Builds the `IN_FORMATS` blob: one format, one linear modifier.
pub(crate) fn build_in_formats_blob() -> Vec<u8> {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct Header {
        version: u32,
        flags: u32,
        count_formats: u32,
        formats_offset: u32,
        count_modifiers: u32,
        modifiers_offset: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct ModifierEntry {
        formats: u64,
        offset: u32,
        _pad: u32,
        modifier: u64,
    }
    let n_formats = SUPPORTED_FORMATS.len() as u32;
    let formats_off = core::mem::size_of::<Header>() as u32;
    let modifiers_off = formats_off + n_formats * 4;
    let hdr = Header {
        version: 1,
        flags: 0,
        count_formats: n_formats,
        formats_offset: formats_off,
        count_modifiers: 1,
        modifiers_offset: modifiers_off,
    };
    let format_mask = (1u64 << n_formats) - 1;
    let me = ModifierEntry {
        formats: format_mask,
        offset: 0,
        _pad: 0,
        modifier: DRM_FORMAT_MOD_LINEAR,
    };
    let mut buf = Vec::with_capacity(
        core::mem::size_of::<Header>()
            + (n_formats as usize) * 4
            + core::mem::size_of::<ModifierEntry>(),
    );
    buf.extend_from_slice(bytes_of(&hdr));
    for fmt in SUPPORTED_FORMATS {
        buf.extend_from_slice(&fmt.to_le_bytes());
    }
    buf.extend_from_slice(bytes_of(&me));
    buf
}
