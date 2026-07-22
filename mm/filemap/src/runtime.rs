// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize};
use kvfs::{AddressSpace, AddressSpaceViewGuard, VfsFile};
use memaddr::VirtAddr;
use memspace::{
    FileMappingInfo, InvalidateHandle, MmObserver, VmArea, VmBackingInfo, VmRuntimeRef,
};
use vmobj::{MappingViewKind, MappingViewSpec, VmObjectId};

use crate::{
    FileMappingMode,
    invalidate::MmSpaceInvalidate,
    private::new_private_runtime,
    shared::{FileSharedRuntimeSpec, new_shared_runtime},
};

pub(crate) struct FileVmaSpec {
    pub start: VirtAddr,
    pub len: usize,
    pub page_size: PageSize,
    pub flags: MappingFlags,
    pub max_flags: MappingFlags,
    pub offset: u64,
    pub inode: u64,
    pub path: Option<alloc::string::String>,
}

pub(crate) struct FileRuntimeContext<'a> {
    pub mm_id: u64,
    pub observer: &'a MmObserver,
    pub invalidate: InvalidateHandle,
}

pub(crate) struct AddressSpaceViewSpec {
    pub mm_id: u64,
    pub start: VirtAddr,
    pub len: usize,
    pub object_start: u64,
    pub object_len: usize,
    pub kind: MappingViewKind,
}

pub(crate) struct SharedFileSourceSpec {
    pub mm_id: u64,
    pub start: VirtAddr,
    pub len: usize,
    pub offset_page: u64,
    pub invalidate: InvalidateHandle,
}

pub(crate) struct SharedFileSourceAdapter {
    start: VirtAddr,
    len: usize,
    file: Arc<VfsFile>,
    offset_page: u64,
    _mapping_view: Option<AddressSpaceViewGuard>,
}

impl SharedFileSourceAdapter {
    pub(crate) fn new(file: Arc<VfsFile>, spec: SharedFileSourceSpec) -> Self {
        let address_space = file.mapping();
        let mapping_view = register_address_space_view(
            address_space,
            spec.invalidate,
            AddressSpaceViewSpec {
                mm_id: spec.mm_id,
                start: spec.start,
                len: spec.len,
                object_start: spec.offset_page * PageSize::Size4K as u64,
                object_len: spec.len,
                kind: MappingViewKind::Shared,
            },
        );
        Self {
            start: spec.start,
            len: spec.len,
            file,
            offset_page: spec.offset_page,
            _mapping_view: mapping_view,
        }
    }

    pub(crate) fn start(&self) -> VirtAddr {
        self.start
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn offset_page(&self) -> u64 {
        self.offset_page
    }

    pub(crate) fn file(&self) -> &VfsFile {
        &self.file
    }

    pub(crate) fn file_arc(&self) -> Arc<VfsFile> {
        self.file.clone()
    }

    pub(crate) fn address_space(&self) -> &Arc<AddressSpace> {
        self.file.mapping()
    }

    pub(crate) fn object(&self) -> VmObjectId {
        self.file.mapping().object_id()
    }

    #[cfg(unittest)]
    pub(crate) fn new_without_view(
        file: Arc<VfsFile>,
        mm_id: u64,
        start: VirtAddr,
        len: usize,
        offset_page: u64,
    ) -> Self {
        let _ = mm_id;
        Self {
            start,
            len,
            file,
            offset_page,
            _mapping_view: None,
        }
    }
}

pub(crate) fn register_address_space_view(
    address_space: &Arc<AddressSpace>,
    handle: InvalidateHandle,
    spec: AddressSpaceViewSpec,
) -> Option<AddressSpaceViewGuard> {
    Some(address_space.register_view(MappingViewSpec {
        mm_id: spec.mm_id,
        vma_start: spec.start.as_usize() as u64,
        vma_len: spec.len,
        object_start: spec.object_start,
        object_len: spec.object_len,
        kind: spec.kind,
        notifier: Some(MmSpaceInvalidate::new(handle)),
    }))
}

pub(crate) fn build_file_runtime(
    start: VirtAddr,
    len: usize,
    file: &Arc<VfsFile>,
    offset: usize,
    page_size: PageSize,
    mode: FileMappingMode,
    ctx: FileRuntimeContext<'_>,
) -> KResult<VmRuntimeRef> {
    match mode {
        FileMappingMode::Shared => Ok(new_shared_runtime(FileSharedRuntimeSpec {
            start,
            len,
            file: file.clone(),
            offset,
            mm_id: ctx.mm_id,
            invalidate: ctx.invalidate,
        })),
        FileMappingMode::Private => new_private_runtime(
            start,
            len,
            page_size,
            file.clone(),
            offset as u64,
            None,
            Some(ctx),
        ),
    }
}

pub(crate) fn build_file_vma(spec: FileVmaSpec, backing: VmBackingInfo) -> VmArea {
    VmArea::new(
        spec.start,
        spec.len,
        spec.flags,
        spec.max_flags,
        backing,
        spec.offset / spec.page_size as u64,
        Some(FileMappingInfo {
            offset: spec.offset,
            inode: spec.inode,
            path: spec.path,
        }),
    )
}

/// Creates a private file-backed VMA metadata record and runtime from an open file.
pub fn new_file_private_vma(
    start: VirtAddr,
    len: usize,
    page_size: PageSize,
    file: Arc<VfsFile>,
    offset: u64,
    file_end: Option<u64>,
    flags: MappingFlags,
) -> KResult<(VmArea, VmRuntimeRef)> {
    let runtime = new_private_runtime(start, len, page_size, file.clone(), offset, file_end, None)
        .expect("cached private file source must build runtime");
    let vma = build_file_vma(
        FileVmaSpec {
            start,
            len,
            page_size,
            flags,
            max_flags: flags,
            offset,
            inode: file.inode().inode(),
            path: file
                .path()
                .absolute_path()
                .ok()
                .map(|it| it.as_str().into()),
        },
        runtime.backing_info(),
    );
    Ok((vma, runtime))
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use khal::paging::MappingFlags;
    use memaddr::{PAGE_SIZE_4K, VirtAddr};
    use memspace::{MmObserver, VmBackingKind};
    use unittest::def_test;

    use super::{FileRuntimeContext, FileVmaSpec, build_file_runtime, build_file_vma};
    use crate::test_support::{anonymous_location, open_test_file};

    const O_RDWR: u32 = 2;

    #[def_test]
    fn cached_file_sources_share_inode_owned_object_identity() {
        let location = anonymous_location("cached-private-identity");
        let cached = open_test_file(location.clone(), O_RDWR);
        let reopened = open_test_file(location, O_RDWR);

        assert_eq!(cached.mapping().object_id(), reopened.mapping().object_id());
    }

    #[def_test]
    fn private_runtime_from_reopened_file_uses_inode_owned_cached_object_identity() {
        let location = anonymous_location("reopened-private-runtime");
        let file = open_test_file(location.clone(), O_RDWR);
        let aspace = Arc::new(ksync::Mutex::new(
            memspace::MmSpace::new_user_empty().expect("mm"),
        ));
        let observer = MmObserver::new(&aspace);
        let invalidate = observer.invalidate_handle();
        let runtime = build_file_runtime(
            VirtAddr::from_usize(0x20000),
            PAGE_SIZE_4K,
            &file,
            0,
            khal::paging::PageSize::Size4K,
            crate::FileMappingMode::Private,
            FileRuntimeContext {
                mm_id: aspace.lock().mm_id(),
                observer: &observer,
                invalidate,
            },
        )
        .expect("private runtime");
        let vma = build_file_vma(
            FileVmaSpec {
                start: VirtAddr::from_usize(0x20000),
                len: PAGE_SIZE_4K,
                page_size: khal::paging::PageSize::Size4K,
                flags: MappingFlags::READ | MappingFlags::WRITE,
                max_flags: MappingFlags::READ | MappingFlags::WRITE,
                offset: 0,
                inode: location.inode().inode(),
                path: None,
            },
            runtime.backing_info(),
        );

        let expected = open_test_file(location, O_RDWR).mapping().object_id();
        let VmBackingKind::FilePrivate { file_object, .. } = vma.backing().kind() else {
            panic!("expected file-private backing");
        };
        assert_eq!(file_object, expected);
    }
}
