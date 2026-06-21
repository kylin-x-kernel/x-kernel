// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};

use kerrno::{KError, KResult};
use kfs::{CachedFile, EvictRegistration, FileFlags, PageIndex};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use memspace::{
    AddrSpace,
    backend::{Backend, DynBackendOps, map_paging_err, pages_in},
};

pub struct FileBackendInner {
    start: VirtAddr,
    cache: CachedFile,
    flags: FileFlags,
    offset_page: PageIndex,
    registration: Mutex<Option<EvictRegistration>>,
    futex_handle: Arc<()>,
}

impl FileBackendInner {
    pub fn register_listener(self: &Arc<Self>, aspace: &Arc<Mutex<AddrSpace>>) {
        let mut registration = self.registration.lock();
        assert!(registration.is_none(), "listener already registered");
        let aspace = Arc::downgrade(aspace);
        let guard = self.cache.add_evict_listener({
            let this = Arc::downgrade(self);
            move |pn, _page| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let Some(aspace) = aspace.upgrade() else {
                    return;
                };
                let Some(mut aspace) = aspace.try_lock() else {
                    return;
                };
                this.on_evict(pn, &mut aspace);
            }
        });
        *registration = Some(guard);
    }

    fn on_evict(self: &Arc<Self>, pn: PageIndex, aspace: &mut AddrSpace) {
        let Some(pn) = pn.checked_sub(self.offset_page) else {
            return;
        };
        if pn > usize::MAX as PageIndex {
            return;
        }
        let Some(offset) = (pn as usize).checked_mul(PageSize::Size4K as usize) else {
            return;
        };
        let vaddr = self.start + offset;
        if !aspace.find_area(vaddr).is_some_and(|it| {
            it.backend()
                .downcast_dynamic_ref::<FileBackend>()
                .is_some_and(|file| Arc::ptr_eq(&file.0, self))
        }) {
            return;
        }

        let pt = aspace.page_table_mut();
        match pt.modify().unmap(vaddr) {
            Ok(_) | Err(PagingError::NotMapped) => {}
            Err(err) => warn!("Failed to unmap page {:?}: {:?}", vaddr, err),
        }
    }
}

#[derive(Clone)]
pub struct FileBackend(pub(crate) Arc<FileBackendInner>);

impl FileBackend {
    pub fn offset_for(&self, addr: VirtAddr) -> u64 {
        let base = self.0.start.as_usize();
        let rel = addr.as_usize().saturating_sub(base) as u64;
        self.0.offset_page * PAGE_SIZE_4K as u64 + rel
    }

    pub fn cache(&self) -> &CachedFile {
        &self.0.cache
    }

    pub fn futex_handle(&self) -> Weak<()> {
        Arc::downgrade(&self.0.futex_handle)
    }

    fn check_flags(&self, flags: MappingFlags) -> KResult {
        let mut required_flags = FileFlags::empty();
        if flags.contains(MappingFlags::READ) {
            required_flags |= FileFlags::READ;
        }
        if flags.contains(MappingFlags::WRITE) {
            required_flags |= FileFlags::WRITE;
        }

        if !self.0.flags.contains(required_flags) {
            return Err(KError::PermissionDenied);
        }
        Ok(())
    }
}

impl DynBackendOps for FileBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn map(&self, _range: VirtAddrRange, flags: MappingFlags, _pt: &mut PageTableMut) -> KResult {
        self.check_flags(flags)
    }

    fn unmap(&self, range: VirtAddrRange, pt: &mut PageTableMut) -> KResult {
        for addr in pages_in(range, PageSize::Size4K)? {
            match pt.unmap(addr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => {
                    warn!("Failed to unmap page {:?}: {:?}", addr, err);
                    return Err(map_paging_err(err));
                }
            }
        }
        Ok(())
    }

    fn on_protect(
        &self,
        _range: VirtAddrRange,
        new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult {
        self.check_flags(new_flags)
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<(usize, Option<Box<dyn FnOnce(&mut AddrSpace)>>)> {
        let mut pages = 0;
        let mut to_be_evicted = Vec::new();
        let range_start_page = ((range.start - self.0.start) / PAGE_SIZE_4K) as PageIndex;
        let start_page = range_start_page
            .checked_add(self.0.offset_page)
            .ok_or(KError::InvalidInput)?;
        for (i, addr) in pages_in(range, PageSize::Size4K)?.enumerate() {
            let pn = start_page
                .checked_add(i as PageIndex)
                .ok_or(KError::InvalidInput)?;
            match pgtbl.query(addr) {
                Ok((paddr, page_flags, _)) => {
                    if access_flags.contains(MappingFlags::WRITE)
                        && !page_flags.contains(MappingFlags::WRITE)
                    {
                        let in_memory = self.0.cache.in_memory();
                        self.0.cache.with_page(pn, |page| {
                            if !in_memory {
                                page.expect("page should be present").mark_dirty();
                            }
                            pgtbl.remap(addr, paddr, flags).map_err(map_paging_err)?;
                            pages += 1;
                            KResult::Ok(())
                        })?;
                    } else if page_flags.contains(access_flags) {
                        pages += 1;
                    }
                }
                Err(PagingError::NotMapped) => {
                    let map_flags = if self.0.cache.in_memory() {
                        flags
                    } else {
                        flags - MappingFlags::WRITE
                    };
                    self.0.cache.with_page_or_insert(pn, |page, evicted| {
                        for pn in evicted {
                            to_be_evicted.push(pn);
                        }
                        pgtbl
                            .map(addr, page.paddr(), PageSize::Size4K, map_flags)
                            .map_err(map_paging_err)?;
                        pages += 1;
                        Ok(())
                    })?;
                }
                Err(_) => return Err(KError::BadAddress),
            }
        }
        Ok((
            pages,
            if to_be_evicted.is_empty() {
                None
            } else {
                let inner = self.0.clone();
                Some(Box::new(move |aspace: &mut AddrSpace| {
                    for pn in to_be_evicted {
                        inner.on_evict(pn, aspace);
                    }
                }))
            },
        ))
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        let inner = Arc::new(FileBackendInner {
            start: self.0.start,
            cache: self.0.cache.clone(),
            flags: self.0.flags,
            offset_page: self.0.offset_page,
            registration: Mutex::new(None),
            futex_handle: self.0.futex_handle.clone(),
        });
        inner.register_listener(new_aspace);
        Ok(Backend::new_dynamic(Arc::new(FileBackend(inner))))
    }

    fn relocated(&self, new_start: VirtAddr, aspace: &Arc<Mutex<AddrSpace>>) -> KResult<Backend> {
        let inner = Arc::new(FileBackendInner {
            start: new_start,
            cache: self.0.cache.clone(),
            flags: self.0.flags,
            offset_page: self.0.offset_page,
            registration: Mutex::new(None),
            futex_handle: self.0.futex_handle.clone(),
        });
        inner.register_listener(aspace);
        Ok(Backend::new_dynamic(Arc::new(FileBackend(inner))))
    }

    fn is_anonymous(&self) -> bool {
        false
    }
}

pub fn new_file(
    start: VirtAddr,
    cache: CachedFile,
    flags: FileFlags,
    offset: usize,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> Backend {
    let offset_page = (offset / PAGE_SIZE_4K) as PageIndex;
    let inner = Arc::new(FileBackendInner {
        start,
        cache,
        flags,
        offset_page,
        registration: Mutex::new(None),
        futex_handle: Arc::new(()),
    });
    inner.register_listener(aspace);
    Backend::new_dynamic(Arc::new(FileBackend(inner)))
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use kerrno::KError;
    use kfs::{CachedFile, FileFlags, PageIndex};
    use khal::paging::{MappingFlags, PageSize};
    use kvfs::{Mountpoint, OpenOptions};
    use memaddr::{PAGE_SIZE_4K, VirtAddr};
    use memfs::MemoryFs;
    use memspace::{
        AddrSpace,
        backend::{Backend, DynBackendOps},
    };
    use unittest::def_test;

    use super::{FileBackend, FileBackendInner, new_file};

    fn new_test_aspace() -> Arc<ksync::Mutex<AddrSpace>> {
        Arc::new(ksync::Mutex::new(
            AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K)
                .expect("test address space should be constructible"),
        ))
    }

    fn new_test_cache(name: &str) -> CachedFile {
        let fs = MemoryFs::new();
        let root = Mountpoint::new_root(&fs).root_location();
        let location = root
            .open_file(
                name,
                &OpenOptions {
                    create: true,
                    ..OpenOptions::default()
                },
            )
            .expect("test file should be creatable");
        CachedFile::get_or_create(location).expect("cached file should be constructible")
    }

    fn new_test_backend(start: usize, flags: FileFlags, offset: usize) -> FileBackend {
        FileBackend(Arc::new(FileBackendInner {
            start: VirtAddr::from(start),
            cache: new_test_cache("backend-file"),
            flags,
            offset_page: (offset / PAGE_SIZE_4K) as PageIndex,
            registration: ksync::Mutex::new(None),
            futex_handle: Arc::new(()),
        }))
    }

    fn downcast_file_backend(backend: &Backend) -> &FileBackend {
        backend
            .downcast_dynamic_ref::<FileBackend>()
            .expect("backend should be a FileBackend")
    }

    #[def_test]
    fn test_offset_for_includes_file_offset_and_saturates_before_start() {
        let backend = new_test_backend(0x4000, FileFlags::READ, PAGE_SIZE_4K * 2);

        assert_eq!(backend.offset_for(VirtAddr::from(0x4000usize)), 0x2000);
        assert_eq!(backend.offset_for(VirtAddr::from(0x4800usize)), 0x2800);
        assert_eq!(backend.offset_for(VirtAddr::from(0x3000usize)), 0x2000);
    }

    #[def_test]
    fn test_check_flags_enforces_requested_access_modes() {
        let read_only = new_test_backend(0x4000, FileFlags::READ, 0);
        assert!(read_only.check_flags(MappingFlags::empty()).is_ok());
        assert!(read_only.check_flags(MappingFlags::READ).is_ok());
        assert!(matches!(
            read_only.check_flags(MappingFlags::WRITE),
            Err(KError::PermissionDenied)
        ));

        let write_only = new_test_backend(0x4000, FileFlags::WRITE, 0);
        assert!(write_only.check_flags(MappingFlags::WRITE).is_ok());
        assert!(matches!(
            write_only.check_flags(MappingFlags::READ),
            Err(KError::PermissionDenied)
        ));
        assert!(matches!(
            write_only.check_flags(MappingFlags::READ | MappingFlags::WRITE),
            Err(KError::PermissionDenied)
        ));
    }

    #[def_test]
    fn test_new_file_rounds_offset_to_page_and_registers_listener() {
        let aspace = new_test_aspace();
        let backend = new_file(
            VirtAddr::from(0x8000usize),
            new_test_cache("new-file"),
            FileFlags::READ | FileFlags::WRITE,
            PAGE_SIZE_4K + 123,
            &aspace,
        );
        let backend = downcast_file_backend(&backend);

        assert_eq!(backend.0.offset_page, 1);
        assert!(backend.0.registration.lock().is_some());
        assert!(!backend.is_anonymous());
        assert_eq!(backend.page_size(), PageSize::Size4K);
    }

    #[def_test]
    fn test_relocated_preserves_shared_state_and_updates_start() {
        let original = new_test_backend(0x4000, FileFlags::READ | FileFlags::WRITE, PAGE_SIZE_4K);
        let original_futex = original.futex_handle();
        let aspace = new_test_aspace();
        let relocated = original
            .relocated(VirtAddr::from(0x9000usize), &aspace)
            .expect("relocation should succeed");
        let relocated = downcast_file_backend(&relocated);

        assert_eq!(relocated.0.start, VirtAddr::from(0x9000usize));
        assert_eq!(relocated.0.offset_page, original.0.offset_page);
        assert_eq!(relocated.0.flags.bits(), original.0.flags.bits());
        assert!(relocated.cache().ptr_eq(original.cache()));
        assert!(relocated.0.registration.lock().is_some());
        assert!(Arc::ptr_eq(
            &relocated
                .futex_handle()
                .upgrade()
                .expect("relocated futex handle should stay alive"),
            &original_futex
                .upgrade()
                .expect("original futex handle should stay alive"),
        ));
    }

    #[def_test]
    fn test_on_evict_ignores_indices_that_cannot_map_to_a_virtual_page() {
        let backend = Arc::new(FileBackendInner {
            start: VirtAddr::from(0x4000usize),
            cache: new_test_cache("evict"),
            flags: FileFlags::READ,
            offset_page: 2,
            registration: ksync::Mutex::new(None),
            futex_handle: Arc::new(()),
        });
        let mut aspace = AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K)
            .expect("test address space should be constructible");

        backend.on_evict(1, &mut aspace);
        backend.on_evict(PageIndex::MAX, &mut aspace);
    }
}
