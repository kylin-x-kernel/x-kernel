// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use core::slice;

use kerrno::{KError, KResult};
use kfs::FileBackend as FsFileBackend;
use khal::{
    mem::p2v,
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use kspin::SpinNoIrq;
use ksync::Mutex;
use memaddr::{PhysAddr, VirtAddr, VirtAddrRange};
use memspace::{
    AddrSpace,
    backend::{Backend, DynBackendOps, alloc_frame, dealloc_frame, pages_in},
};

struct FrameRefCnt(u32);

impl FrameRefCnt {
    fn drop_frame(&mut self, pa: PhysAddr, pgsize: PageSize) {
        assert!(self.0 > 0, "dropping unreferenced frame");
        self.0 -= 1;
        if self.0 == 0 {
            FRAME_TABLE.lock().remove_frame(pa);
            dealloc_frame(pa, pgsize);
        }
    }
}

struct FrameTableRefCount {
    table: BTreeMap<PhysAddr, Arc<SpinNoIrq<FrameRefCnt>>>,
}

impl FrameTableRefCount {
    const INITIAL_CNT: u32 = 1;

    const fn new() -> Self {
        Self {
            table: BTreeMap::new(),
        }
    }

    fn get_frame_ref(&mut self, pa: PhysAddr) -> Option<Arc<SpinNoIrq<FrameRefCnt>>> {
        self.table.get(&pa).cloned()
    }

    fn init_frame(&mut self, pa: PhysAddr) {
        assert!(
            !self.table.contains_key(&pa),
            "initializing already referenced frame"
        );
        self.table
            .insert(pa, Arc::new(SpinNoIrq::new(FrameRefCnt(Self::INITIAL_CNT))));
    }

    fn remove_frame(&mut self, pa: PhysAddr) {
        assert!(self.table.contains_key(&pa), "removing unreferenced frame");
        self.table.remove(&pa);
    }
}

static FRAME_TABLE: SpinNoIrq<FrameTableRefCount> = SpinNoIrq::new(FrameTableRefCount::new());

#[derive(Clone)]
pub struct CowBackend {
    start: VirtAddr,
    size: PageSize,
    file: Option<(FsFileBackend, u64, Option<u64>)>,
}

impl CowBackend {
    pub fn start(&self) -> VirtAddr {
        self.start
    }

    pub fn file_mapping(&self) -> Option<(&FsFileBackend, u64)> {
        self.file
            .as_ref()
            .map(|(file, file_start, _)| (file, *file_start))
    }

    fn alloc_new_frame(&self, zeroed: bool) -> KResult<PhysAddr> {
        let frame = alloc_frame(zeroed, self.size)?;
        FRAME_TABLE.lock().init_frame(frame);
        Ok(frame)
    }

    fn alloc_new_at(&self, va: VirtAddr, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        let frame = self.alloc_new_frame(true)?;

        if let Some((file, file_start, file_end)) = &self.file {
            // SAFETY: `frame` is a freshly allocated mapped frame of `self.size`
            // bytes, so it may be exposed as a mutable byte slice for file fill.
            let buf = unsafe { slice::from_raw_parts_mut(p2v(frame).as_mut_ptr(), self.size as _) };
            let start = self.start.as_usize().saturating_sub(va.as_usize());
            assert!(start < self.size as _);

            let file_start =
                *file_start + va.as_usize().saturating_sub(self.start.as_usize()) as u64;
            let max_read = file_end
                .map_or(u64::MAX, |end| end.saturating_sub(file_start))
                .min((buf.len() - start) as u64) as usize;

            file.read_at(&mut &mut buf[start..start + max_read], file_start)?;
        }
        pgtbl
            .map(va, frame, self.size, flags)
            .map_err(memspace::backend::map_paging_err)?;

        if flags.contains(MappingFlags::EXECUTE) {
            karch::flush_icache_range(p2v(frame), self.size.into());
        }
        Ok(())
    }

    fn cow_fault(
        &self,
        va: VirtAddr,
        pa: PhysAddr,
        flags: MappingFlags,
        pgtble: &mut PageTableMut,
    ) -> KResult {
        let mut frame_table = FRAME_TABLE.lock();
        let frame = frame_table.get_frame_ref(pa).ok_or(KError::BadAddress)?;
        drop(frame_table);
        let mut frame = frame.lock();
        assert!(frame.0 > 0, "invalid frame reference count");
        match frame.0 {
            1 => {
                pgtble
                    .protect(va, flags)
                    .map_err(memspace::backend::map_paging_err)?;
                return Ok(());
            }
            _ => {
                let new_frame = self.alloc_new_frame(false)?;
                // SAFETY: `pa` and `new_frame` are distinct frame-sized mappings
                // of `self.size` bytes, so copying the full page contents is valid.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        p2v(pa).as_ptr(),
                        p2v(new_frame).as_mut_ptr(),
                        self.size as _,
                    );
                }
                pgtble
                    .remap(va, new_frame, flags)
                    .map_err(memspace::backend::map_paging_err)?;
                frame.drop_frame(pa, self.size);

                if flags.contains(MappingFlags::EXECUTE) {
                    karch::flush_icache_range(p2v(new_frame), self.size.into());
                }
            }
        }

        Ok(())
    }
}

impl DynBackendOps for CowBackend {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn page_size(&self) -> PageSize {
        self.size
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, _pgtbl: &mut PageTableMut) -> KResult {
        debug!("Cow::map: {range:?} {flags:?}");
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        debug!("Cow::unmap: {range:?}");
        for addr in pages_in(range, self.size)? {
            if let Ok((frame, _flags, page_size)) = pgtbl.unmap(addr) {
                assert_eq!(page_size, self.size);
                let frame_ref = FRAME_TABLE
                    .lock()
                    .get_frame_ref(frame)
                    .ok_or(KError::BadAddress)?;
                let mut frame_ref = frame_ref.lock();
                frame_ref.drop_frame(frame, self.size);
            }
        }
        Ok(())
    }

    fn on_protect(
        &self,
        _range: VirtAddrRange,
        _new_flags: MappingFlags,
        _pgtbl: &mut PageTableMut,
    ) -> KResult {
        Ok(())
    }

    fn populate(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        access_flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<(usize, Option<Box<dyn FnOnce(&mut AddrSpace)>>)> {
        let mut pages = 0;
        for addr in pages_in(range, self.size)? {
            match pgtbl.query(addr) {
                Ok((paddr, page_flags, page_size)) => {
                    assert_eq!(self.size, page_size);
                    if access_flags.contains(MappingFlags::WRITE)
                        && !page_flags.contains(MappingFlags::WRITE)
                    {
                        self.cow_fault(addr, paddr, flags, pgtbl)?;
                        pages += 1;
                    } else if page_flags.contains(access_flags) {
                        pages += 1;
                    }
                }
                Err(PagingError::NotMapped) => {
                    self.alloc_new_at(addr, flags, pgtbl)?;
                    pages += 1;
                }
                Err(_) => return Err(KError::BadAddress),
            }
        }
        Ok((pages, None))
    }

    fn clone_map(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old_pgtbl: &mut PageTableMut,
        new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        let cow_flags = flags - MappingFlags::WRITE;

        for vaddr in pages_in(range, self.size)? {
            match old_pgtbl.query(vaddr) {
                Ok((paddr, _, page_size)) => {
                    assert_eq!(page_size, self.size);
                    let frame = FRAME_TABLE
                        .lock()
                        .get_frame_ref(paddr)
                        .ok_or(KError::BadAddress)?;
                    let mut frame = frame.lock();
                    assert!(frame.0 > 0, "referencing unreferenced frame");
                    let new_cnt = frame.0.checked_add(1).ok_or_else(|| {
                        warn!("frame reference count overflow");
                        KError::BadAddress
                    })?;
                    frame.0 = new_cnt;
                    old_pgtbl
                        .protect(vaddr, cow_flags)
                        .map_err(memspace::backend::map_paging_err)?;
                    new_pgtbl
                        .map(vaddr, paddr, self.size, cow_flags)
                        .map_err(memspace::backend::map_paging_err)?;
                }
                Err(PagingError::NotMapped) => {}
                Err(_) => return Err(KError::BadAddress),
            };
        }

        Ok(Backend::new_dynamic(Arc::new(self.clone())))
    }

    fn relocated(&self, new_start: VirtAddr, _aspace: &Arc<Mutex<AddrSpace>>) -> KResult<Backend> {
        // CowBackend uses a global FRAME_TABLE for frame lifetime management
        // and does not need per-aspace eviction listeners unlike FileBackend.
        Ok(Backend::new_dynamic(Arc::new(CowBackend {
            start: new_start,
            size: self.size,
            file: self.file.clone(),
        })))
    }

    fn is_anonymous(&self) -> bool {
        self.file.is_none()
    }
}

pub fn new_cow(
    start: VirtAddr,
    size: PageSize,
    file: FsFileBackend,
    file_start: u64,
    file_end: Option<u64>,
) -> Backend {
    Backend::new_dynamic(Arc::new(CowBackend {
        start,
        size,
        file: Some((file, file_start, file_end)),
    }))
}

pub fn new_alloc(start: VirtAddr, size: PageSize) -> Backend {
    Backend::new_dynamic(Arc::new(CowBackend {
        start,
        size,
        file: None,
    }))
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use kerrno::KError;
    use khal::paging::{MappingFlags, PageSize, PagingError};
    use ksync::Mutex;
    use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange};
    use memspace::{
        AddrSpace,
        backend::{BackendOps, dealloc_frame},
    };
    use unittest::def_test;

    use super::{
        CowBackend, DynBackendOps, FRAME_TABLE, FrameRefCnt, FrameTableRefCount, new_alloc,
    };

    fn new_test_aspace() -> AddrSpace {
        AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K * 32)
            .expect("test address space should be constructible")
    }

    fn new_test_aspace_handle() -> Arc<Mutex<AddrSpace>> {
        Arc::new(Mutex::new(new_test_aspace()))
    }

    fn new_test_backend(start: usize) -> CowBackend {
        CowBackend {
            start: VirtAddr::from(start),
            size: PageSize::Size4K,
            file: None,
        }
    }

    #[def_test]
    fn test_frame_table_ref_count_tracks_lookup_and_remove() {
        let pa = PhysAddr::from(0x1000usize);
        let mut table = FrameTableRefCount::new();

        assert!(table.get_frame_ref(pa).is_none());

        table.init_frame(pa);
        let frame_ref = table
            .get_frame_ref(pa)
            .expect("initialized frame should be tracked");
        assert_eq!(frame_ref.lock().0, FrameTableRefCount::INITIAL_CNT);

        table.remove_frame(pa);
        assert!(table.get_frame_ref(pa).is_none());
    }

    #[def_test]
    fn test_frame_ref_count_drop_frame_decrements_shared_reference() {
        let mut refcnt = FrameRefCnt(2);

        refcnt.drop_frame(PhysAddr::from(0x2000usize), PageSize::Size4K);

        assert_eq!(refcnt.0, 1);
    }

    #[def_test]
    fn test_new_alloc_backend_exposes_expected_metadata() {
        let start = VirtAddr::from(0x4000usize);
        let backend = new_alloc(start, PageSize::Size4K);

        assert!(backend.is_anonymous());

        let cow = backend
            .downcast_dynamic_ref::<CowBackend>()
            .expect("new_alloc should build a CowBackend");
        assert_eq!(cow.start(), start);
        assert_eq!(cow.page_size(), PageSize::Size4K);
        assert!(cow.file_mapping().is_none());
    }

    #[def_test]
    fn test_relocated_updates_start_and_preserves_anonymous_mapping() {
        let backend = new_test_backend(0x8000);
        let relocated = DynBackendOps::relocated(
            &backend,
            VirtAddr::from(0xc000usize),
            &new_test_aspace_handle(),
        )
        .expect("relocation should succeed");

        let relocated = relocated
            .downcast_dynamic_ref::<CowBackend>()
            .expect("relocation should keep CowBackend type");
        assert_eq!(relocated.start(), VirtAddr::from(0xc000usize));
        assert!(relocated.file_mapping().is_none());
        assert!(relocated.is_anonymous());
    }

    #[def_test]
    fn test_clone_map_skips_unmapped_pages() {
        let backend = new_test_backend(0x10000);
        let addr = VirtAddr::from(0x10000usize);
        let range = VirtAddrRange::from_start_size(addr, PAGE_SIZE_4K);
        let mut old_aspace = new_test_aspace();
        let mut new_aspace = new_test_aspace();
        let mut old_pgtbl = old_aspace.page_table_mut().modify();
        let mut new_pgtbl = new_aspace.page_table_mut().modify();

        let cloned = DynBackendOps::clone_map(
            &backend,
            range,
            MappingFlags::READ | MappingFlags::WRITE,
            &mut old_pgtbl,
            &mut new_pgtbl,
            &new_test_aspace_handle(),
        )
        .expect("clone_map should skip absent mappings");

        assert!(cloned.downcast_dynamic_ref::<CowBackend>().is_some());
        assert!(matches!(old_pgtbl.query(addr), Err(PagingError::NotMapped)));
        assert!(matches!(new_pgtbl.query(addr), Err(PagingError::NotMapped)));
    }

    #[def_test]
    fn test_clone_map_returns_bad_address_when_frame_is_not_tracked() {
        let backend = new_test_backend(0x14000);
        let addr = VirtAddr::from(0x14000usize);
        let range = VirtAddrRange::from_start_size(addr, PAGE_SIZE_4K);
        let frame = super::alloc_frame(true, PageSize::Size4K)
            .expect("test frame allocation should succeed");
        let mut old_aspace = new_test_aspace();
        let mut new_aspace = new_test_aspace();

        {
            let mut old_pgtbl = old_aspace.page_table_mut().modify();
            old_pgtbl
                .map(
                    addr,
                    frame,
                    PageSize::Size4K,
                    MappingFlags::READ | MappingFlags::WRITE,
                )
                .expect("test mapping should succeed");
            let mut new_pgtbl = new_aspace.page_table_mut().modify();

            let err = match DynBackendOps::clone_map(
                &backend,
                range,
                MappingFlags::READ | MappingFlags::WRITE,
                &mut old_pgtbl,
                &mut new_pgtbl,
                &new_test_aspace_handle(),
            ) {
                Ok(_) => panic!("clone_map should reject untracked shared frames"),
                Err(err) => err,
            };
            assert!(matches!(err, KError::BadAddress));
        }

        old_aspace
            .page_table_mut()
            .modify()
            .unmap(addr)
            .expect("cleanup unmap should succeed");
        dealloc_frame(frame, PageSize::Size4K);
    }

    #[def_test]
    fn test_clone_map_converts_writable_mapping_to_cow_and_tracks_refs() {
        let backend = new_test_backend(0x18000);
        let addr = VirtAddr::from(0x18000usize);
        let range = VirtAddrRange::from_start_size(addr, PAGE_SIZE_4K);
        let frame = backend
            .alloc_new_frame(true)
            .expect("tracked frame allocation should succeed");
        let mut old_aspace = new_test_aspace();
        let mut new_aspace = new_test_aspace();

        let cloned = {
            let mut old_pgtbl = old_aspace.page_table_mut().modify();
            old_pgtbl
                .map(
                    addr,
                    frame,
                    PageSize::Size4K,
                    MappingFlags::READ | MappingFlags::WRITE,
                )
                .expect("test mapping should succeed");
            let mut new_pgtbl = new_aspace.page_table_mut().modify();

            let cloned = DynBackendOps::clone_map(
                &backend,
                range,
                MappingFlags::READ | MappingFlags::WRITE,
                &mut old_pgtbl,
                &mut new_pgtbl,
                &new_test_aspace_handle(),
            )
            .expect("clone_map should succeed for tracked frames");

            let (old_frame, old_flags, old_size) = old_pgtbl
                .query(addr)
                .expect("old mapping should remain present");
            let (new_frame, new_flags, new_size) = new_pgtbl
                .query(addr)
                .expect("new mapping should be installed");

            assert_eq!(old_frame, frame);
            assert_eq!(new_frame, frame);
            assert_eq!(old_size, PageSize::Size4K);
            assert_eq!(new_size, PageSize::Size4K);
            assert!(!old_flags.contains(MappingFlags::WRITE));
            assert!(!new_flags.contains(MappingFlags::WRITE));
            assert!(cloned.downcast_dynamic_ref::<CowBackend>().is_some());

            let frame_ref = FRAME_TABLE
                .lock()
                .get_frame_ref(frame)
                .expect("tracked frame should remain in table");
            assert_eq!(frame_ref.lock().0, 2);
            cloned
        };

        {
            let mut old_pgtbl = old_aspace.page_table_mut().modify();
            DynBackendOps::unmap(&backend, range, &mut old_pgtbl)
                .expect("first unmap should drop one shared reference");
        }
        let frame_ref = FRAME_TABLE
            .lock()
            .get_frame_ref(frame)
            .expect("one reference should remain after first unmap");
        assert_eq!(frame_ref.lock().0, 1);

        {
            let mut new_pgtbl = new_aspace.page_table_mut().modify();
            BackendOps::unmap(&cloned, range, &mut new_pgtbl)
                .expect("second unmap should release the last shared reference");
        }
        assert!(FRAME_TABLE.lock().get_frame_ref(frame).is_none());
    }

    #[def_test]
    fn test_unmap_returns_bad_address_when_mapping_is_not_tracked() {
        let backend = new_test_backend(0x1c000);
        let addr = VirtAddr::from(0x1c000usize);
        let range = VirtAddrRange::from_start_size(addr, PAGE_SIZE_4K);
        let frame = super::alloc_frame(true, PageSize::Size4K)
            .expect("test frame allocation should succeed");
        let mut aspace = new_test_aspace();

        {
            let mut pgtbl = aspace.page_table_mut().modify();
            pgtbl
                .map(
                    addr,
                    frame,
                    PageSize::Size4K,
                    MappingFlags::READ | MappingFlags::WRITE,
                )
                .expect("test mapping should succeed");

            let err = DynBackendOps::unmap(&backend, range, &mut pgtbl)
                .expect_err("unmap should reject frames missing from FRAME_TABLE");
            assert!(matches!(err, KError::BadAddress));
            assert!(matches!(pgtbl.query(addr), Err(PagingError::NotMapped)));
        }

        dealloc_frame(frame, PageSize::Size4K);
    }
}
