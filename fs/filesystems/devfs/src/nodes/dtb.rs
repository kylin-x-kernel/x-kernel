// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec};
use core::any::Any;

use kvfs::{DeviceFileOps, DeviceId, NodeFlags, NodeType, VfsResult};
use kvfs_simple::{DirMapping, SimpleFs};
use lazyinit::LazyInit;

use crate::DeviceFile;

static DTB_SNAPSHOT: LazyInit<Box<[u8]>> = LazyInit::new();

pub(crate) fn capture_snapshot() {
    let Some((paddr, vaddr, size)) = khal::firmware::dtb_capture_region() else {
        return;
    };
    if DTB_SNAPSHOT.get().is_some() {
        return;
    }

    let mut snapshot = vec![0u8; size];
    let src = vaddr as *const u8;
    unsafe {
        core::ptr::copy_nonoverlapping(src, snapshot.as_mut_ptr(), size);
    }
    DTB_SNAPSHOT.init_once(snapshot.into_boxed_slice());
    info!(
        "Captured DTB snapshot: paddr={:#x} vaddr={:#x} size={:#x}",
        paddr, vaddr, size
    );
}

pub(crate) fn snapshot_available() -> bool {
    DTB_SNAPSHOT.get().is_some()
}

pub(crate) struct DtbSnapshot;

impl DeviceFileOps for DtbSnapshot {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
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

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    if snapshot_available() {
        root.add(
            "firmware-dtb",
            DeviceFile::new(
                fs.clone(),
                NodeType::CharacterDevice,
                DeviceId::new(30, 2),
                Arc::new(DtbSnapshot),
            ),
        );
    }
}
