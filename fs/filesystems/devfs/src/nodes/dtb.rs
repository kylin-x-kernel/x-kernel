// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kalloc_types::{AllocFlags, KBox};
use kvfs::{DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsResult};
use lazyinit::LazyInit;

use crate::{DeviceFile, add_device_entry};

static DTB_SNAPSHOT: LazyInit<KBox<[u8]>> = LazyInit::new();

/// Captures firmware under the serialized DEVFS initializer using NOWAIT.
/// Failure leaves the snapshot unset; completed captures are reused on retry.
pub(crate) fn capture_snapshot() -> VfsResult<()> {
    capture_snapshot_with(&DTB_SNAPSHOT, khal::firmware::dtb_bytes(), |bytes| {
        KBox::from_slice_with_flags(bytes, AllocFlags::NOWAIT)
    })
}

fn capture_snapshot_with(
    snapshot: &LazyInit<KBox<[u8]>>,
    dtb: Option<&[u8]>,
    prepare: impl FnOnce(&[u8]) -> VfsResult<KBox<[u8]>>,
) -> VfsResult<()> {
    let Some(dtb) = dtb else {
        return Ok(());
    };
    if snapshot.get().is_some() {
        return Ok(());
    }

    snapshot.init_once(prepare(dtb)?);
    info!(
        "Captured DTB snapshot: vaddr={:#x} size={:#x}",
        dtb.as_ptr() as usize,
        dtb.len()
    );
    Ok(())
}

pub(crate) fn snapshot_available() -> bool {
    DTB_SNAPSHOT.get().is_some()
}

pub(crate) struct DtbSnapshot;

impl DeviceFileOps for DtbSnapshot {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let Some(snapshot) = DTB_SNAPSHOT.get() else {
            return Ok(0);
        };
        let offset = offset as usize;
        if offset >= snapshot.len() {
            return Ok(0);
        }
        let len = buf.len().min(snapshot.len() - offset);
        buf[..len].copy_from_slice(&snapshot[offset..offset + len]);
        Ok(len)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if snapshot_available() {
        add_device_entry(
            root,
            "firmware-dtb",
            DeviceFile::new_character(fs.clone(), DeviceId::new(30, 2), Arc::new(DtbSnapshot)),
        );
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/dtb.rs");
