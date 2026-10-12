// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

use kvfs::{DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsResult};
use lazyinit::LazyInit;

use crate::{DeviceFile, add_device_entry};

static DTB_SNAPSHOT: LazyInit<Box<[u8]>> = LazyInit::new();

pub(crate) fn capture_snapshot() {
    let Some(dtb) = khal::firmware::dtb_bytes() else {
        return;
    };
    if DTB_SNAPSHOT.get().is_some() {
        return;
    }

    DTB_SNAPSHOT.init_once(dtb.to_vec().into_boxed_slice());
    info!(
        "Captured DTB snapshot: vaddr={:#x} size={:#x}",
        dtb.as_ptr() as usize,
        dtb.len()
    );
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

#[unittest::mod_test]
mod tests {
    use alloc::vec;

    use kvfs::{Filename, Mount, NodePermission, Path, SuperBlockFlags};
    use unittest::def_test;

    #[def_test]
    fn bootstrap_snapshot_is_readable_from_shared_devtmpfs() {
        let Some(expected) = khal::firmware::dtb_bytes() else {
            return unittest::TestResult::Ignored;
        };
        let super_block = crate::new_devfs(SuperBlockFlags::empty());
        let second_mount = crate::new_devfs(SuperBlockFlags::empty());
        assert!(alloc::sync::Arc::ptr_eq(&super_block, &second_mount));
        let root = Path::new(Mount::new_root(&super_block), super_block.root_dir());
        let file = Filename::new("firmware-dtb")
            .open_with_flags_at(
                &root,
                &root,
                0,
                NodePermission::default(),
                NodePermission::default(),
                kcred::initial_cred(),
            )
            .expect("bootstrap devtmpfs must publish firmware snapshot");
        let mut bytes = vec![0; expected.len()];
        let mut offset = 0;
        while offset < expected.len() {
            let count = file.read(&mut bytes[offset..]).expect("snapshot read");
            assert!(count > 0, "snapshot must not end before firmware blob");
            offset += count;
        }
        assert_eq!(bytes.as_slice(), expected);
        assert_eq!(file.read(&mut [0; 1]).expect("snapshot EOF"), 0);
    }
}
