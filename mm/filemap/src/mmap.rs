// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File mapping callback implementation.
use alloc::sync::Arc;

use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize};
use ksync::Mutex;
use kvfs::{MmapMapper, VfsFile};
use memaddr::{MemoryAddr, PhysAddrRange, VirtAddr};
use memfs::shmem;
use memspace::{InvalidateHandle, MmSpace, VmArea, VmRuntimeRef};

use crate::{
    FileMappingMode,
    runtime::{FileRuntimeContext, FileVmaSpec, build_file_runtime, build_file_vma},
};

struct FileMapper {
    req: FileMmapRequest,
    mode: FileMappingMode,
    runtime: Option<VmRuntimeRef>,
}

impl FileMapper {
    fn into_runtime(self) -> KResult<VmRuntimeRef> {
        self.runtime.ok_or(kerrno::KError::NoSuchDevice)
    }
}

pub struct FileMmapRequest {
    pub start: VirtAddr,
    pub length: usize,
    pub offset: usize,
    pub page_size: PageSize,
    pub flags: MappingFlags,
    pub max_flags: MappingFlags,
    pub file: Arc<VfsFile>,
    pub mm_id: u64,
    pub aspace: Arc<Mutex<MmSpace>>,
    pub invalidate: InvalidateHandle,
}

impl MmapMapper for FileMapper {
    fn offset(&self) -> usize {
        self.req.offset
    }

    fn map_physical(&mut self, mut range: PhysAddrRange) -> kvfs::VfsResult<()> {
        range.start += self.req.offset;
        if range.is_empty() {
            return Err(kvfs::VfsError::InvalidInput);
        }
        self.req.length = self
            .req
            .length
            .min(range.size().align_down(self.req.page_size));
        self.runtime = Some(VmRuntimeRef::new_linear(
            self.req.start.as_usize() as isize - range.start.as_usize() as isize,
        ));
        Ok(())
    }

    fn map_file_backed(&mut self) -> kvfs::VfsResult<()> {
        self.runtime = Some(build_file_runtime(
            self.req.start,
            self.req.length,
            &self.req.file,
            self.req.offset,
            self.req.page_size,
            self.mode,
            FileRuntimeContext {
                mm_id: self.req.mm_id,
                aspace: &self.req.aspace,
                invalidate: self.req.invalidate.clone(),
            },
        )?);
        Ok(())
    }

    fn map_anonymous_shared(&mut self) -> kvfs::VfsResult<()> {
        self.runtime = Some(VmRuntimeRef::new_anon_shared(
            self.req.start,
            self.req.length,
            self.req.page_size,
        )?);
        Ok(())
    }
}

/// Resolves a file-backed `mmap` request into VMA metadata and runtime ops.
///
/// This keeps the VFS callback-driven `MmapMapper` object inside
/// `filemap` so syscall code only expresses Linux-facing policy:
/// shared/private mode, offset, and target address range.
fn mmap_file(req: FileMmapRequest, mode: FileMappingMode) -> KResult<(VmArea, VmRuntimeRef)> {
    let file = req.file.clone();
    let inode = file.inode().inode();
    let path = file
        .path()
        .absolute_path()
        .ok()
        .map(|it| it.as_str().into());
    let mut mapper = FileMapper {
        req,
        mode,
        runtime: None,
    };
    file.mmap(&mut mapper)?;
    let length = mapper.req.length;
    let start = mapper.req.start;
    let page_size = mapper.req.page_size;
    let flags = mapper.req.flags;
    let max_flags = mapper.req.max_flags;
    let offset = mapper.req.offset as u64;
    let runtime = mapper.into_runtime()?;
    let vma = build_file_vma(
        FileVmaSpec {
            start,
            len: length,
            page_size,
            flags,
            max_flags,
            offset,
            inode,
            path,
        },
        runtime.backing_info(),
    );
    Ok((vma, runtime))
}

/// Resolves a Linux `MAP_SHARED`-style file-backed mapping request.
pub fn mmap_shared_file(req: FileMmapRequest) -> KResult<(VmArea, VmRuntimeRef)> {
    if req.flags.contains(MappingFlags::WRITE) {
        shmem::check_shared_writable_mapping_allowed(req.file.path())?;
    }
    mmap_file(req, FileMappingMode::Shared)
}

/// Resolves a Linux `MAP_PRIVATE`-style file-backed mapping request.
pub fn mmap_private_file(req: FileMmapRequest) -> KResult<(VmArea, VmRuntimeRef)> {
    mmap_file(req, FileMappingMode::Private)
}
