use core::{marker::PhantomData, ops::Deref};

use arrayvec::ArrayVec;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr};

use crate::defs::{
    PageSize, PageTableEntry, PagingFlags, PagingHandler, PagingMetaData, PtError, PtResult,
};

const ENTRY_COUNT: usize = 512;

const fn p4_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 27)) & (ENTRY_COUNT - 1)
}

const fn p3_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 18)) & (ENTRY_COUNT - 1)
}

const fn p2_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 9)) & (ENTRY_COUNT - 1)
}

const fn p1_idx(vaddr: usize) -> usize {
    (vaddr >> 12) & (ENTRY_COUNT - 1)
}

pub struct PageTable64<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> {
    root_paddr: PhysAddr,
    #[cfg(feature = "copy-from")]
    borrowed_entries: bitmaps::Bitmap<ENTRY_COUNT>,
    _phantom: PhantomData<(M, PTE, H)>,
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTable64<M, PTE, H> {
    pub fn try_new() -> PtResult<Self> {
        let root_paddr = Self::alloc_table()?;
        Ok(Self {
            root_paddr,
            #[cfg(feature = "copy-from")]
            borrowed_entries: bitmaps::Bitmap::new(),
            _phantom: PhantomData,
        })
    }

    pub const fn root_paddr(&self) -> PhysAddr {
        self.root_paddr
    }

    pub fn query(&self, vaddr: M::VirtAddr) -> PtResult<(PhysAddr, PagingFlags, PageSize)> {
        let (entry, size) = self.get_entry(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        let off = size.align_offset(vaddr.into());
        Ok((entry.paddr().add(off), entry.flags(), size))
    }

    pub fn modify(&mut self) -> PageTableMut<'_, M, PTE, H> {
        PageTableMut::new(self)
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTable64<M, PTE, H> {
    fn alloc_table() -> PtResult<PhysAddr> {
        if let Some(paddr) = H::alloc_frame() {
            let ptr = H::phys_to_virt(paddr).as_mut_ptr();
            unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE_4K) };
            Ok(paddr)
        } else {
            Err(PtError::NoMemory)
        }
    }

    fn table_of<'a>(&self, paddr: PhysAddr) -> &'a [PTE] {
        let ptr = H::phys_to_virt(paddr).as_ptr() as _;
        unsafe { core::slice::from_raw_parts(ptr, ENTRY_COUNT) }
    }

    fn next_table<'a>(&self, entry: &PTE) -> PtResult<&'a [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PtError::NotMapped)
        } else if entry.is_huge() {
            Err(PtError::MappedToHugePage)
        } else {
            Ok(self.table_of(entry.paddr()))
        }
    }

    fn get_entry(&self, vaddr: M::VirtAddr) -> PtResult<(&PTE, PageSize)> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of(self.root_paddr());
            let p4e = &p4[p4_idx(vaddr)];
            self.next_table(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &p3[p3_idx(vaddr)];
        if p3e.is_huge() {
            return Ok((p3e, PageSize::Size1G));
        }

        let p2 = self.next_table(p3e)?;
        let p2e = &p2[p2_idx(vaddr)];
        if p2e.is_huge() {
            return Ok((p2e, PageSize::Size2M));
        }

        let p1 = self.next_table(p2e)?;
        let p1e = &p1[p1_idx(vaddr)];
        Ok((p1e, PageSize::Size4K))
    }

    fn dealloc_tree(&self, table_paddr: PhysAddr, level: usize) {
        if level < M::LEVELS - 1 {
            for entry in self.table_of(table_paddr) {
                if self.next_table(entry).is_ok() {
                    self.dealloc_tree(entry.paddr(), level + 1);
                }
            }
        }
        H::dealloc_frame(table_paddr);
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop for PageTable64<M, PTE, H> {
    fn drop(&mut self) {
        let root = self.table_of(self.root_paddr);
        #[allow(unused_variables)]
        for (i, entry) in root.iter().enumerate() {
            #[cfg(feature = "copy-from")]
            if self.borrowed_entries.get(i) {
                continue;
            }
            if self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
        }
        H::dealloc_frame(self.root_paddr());
    }
}

const FLUSH_THRESHOLD: usize = 16;

enum ToFlush<M: PagingMetaData> {
    None,
    Addresses(ArrayVec<M::VirtAddr, FLUSH_THRESHOLD>),
    Full,
}

pub struct PageTableMut<'a, M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> {
    inner: &'a mut PageTable64<M, PTE, H>,
    flush: ToFlush<M>,
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Deref
    for PageTableMut<'_, M, PTE, H>
{
    type Target = PageTable64<M, PTE, H>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'a, M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTableMut<'a, M, PTE, H> {
    fn new(inner: &'a mut PageTable64<M, PTE, H>) -> Self {
        Self {
            inner,
            flush: ToFlush::None,
        }
    }

    fn flush(&mut self, vaddr: M::VirtAddr) {
        match self.flush {
            ToFlush::None => {
                let mut addresses = ArrayVec::new();
                addresses.push(vaddr);
                self.flush = ToFlush::Addresses(addresses);
            }
            ToFlush::Addresses(ref mut addrs) => {
                if addrs.try_push(vaddr).is_err() {
                    self.flush = ToFlush::Full;
                }
            }
            ToFlush::Full => {}
        }
    }

    fn table_of_mut(&mut self, paddr: PhysAddr) -> &'a mut [PTE] {
        let ptr = H::phys_to_virt(paddr).as_mut_ptr() as _;
        unsafe { core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
    }

    fn next_table_mut(&mut self, entry: &PTE) -> PtResult<&'a mut [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PtError::NotMapped)
        } else if entry.is_huge() {
            Err(PtError::MappedToHugePage)
        } else {
            Ok(self.table_of_mut(entry.paddr()))
        }
    }

    fn next_table_mut_or_create(&mut self, entry: &mut PTE) -> PtResult<&'a mut [PTE]> {
        if entry.is_unused() {
            let paddr = PageTable64::<M, PTE, H>::alloc_table()?;
            *entry = PageTableEntry::new_table(paddr);
            Ok(self.table_of_mut(paddr))
        } else {
            self.next_table_mut(entry)
        }
    }

    fn get_entry_mut(&mut self, vaddr: M::VirtAddr) -> PtResult<(&mut PTE, PageSize)> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of_mut(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of_mut(self.root_paddr());
            let p4e = &mut p4[p4_idx(vaddr)];
            self.next_table_mut(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &mut p3[p3_idx(vaddr)];
        if p3e.is_huge() {
            return Ok((p3e, PageSize::Size1G));
        }

        let p2 = self.next_table_mut(p3e)?;
        let p2e = &mut p2[p2_idx(vaddr)];
        if p2e.is_huge() {
            return Ok((p2e, PageSize::Size2M));
        }

        let p1 = self.next_table_mut(p2e)?;
        let p1e = &mut p1[p1_idx(vaddr)];
        Ok((p1e, PageSize::Size4K))
    }

    fn get_entry_mut_or_create(
        &mut self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PtResult<&mut PTE> {
        let vaddr: usize = vaddr.into();
        let p3 = if M::LEVELS == 3 {
            self.table_of_mut(self.root_paddr())
        } else if M::LEVELS == 4 {
            let p4 = self.table_of_mut(self.root_paddr());
            let p4e = &mut p4[p4_idx(vaddr)];
            self.next_table_mut_or_create(p4e)?
        } else {
            unreachable!()
        };
        let p3e = &mut p3[p3_idx(vaddr)];
        if page_size == PageSize::Size1G {
            return Ok(p3e);
        }

        let p2 = self.next_table_mut_or_create(p3e)?;
        let p2e = &mut p2[p2_idx(vaddr)];
        if page_size == PageSize::Size2M {
            return Ok(p2e);
        }

        let p1 = self.next_table_mut_or_create(p2e)?;
        let p1e = &mut p1[p1_idx(vaddr)];
        Ok(p1e)
    }

    pub fn map(
        &mut self,
        vaddr: M::VirtAddr,
        target: PhysAddr,
        page_size: PageSize,
        flags: PagingFlags,
    ) -> PtResult {
        let entry = self.get_entry_mut_or_create(vaddr, page_size)?;
        if !entry.is_unused() {
            return Err(PtError::AlreadyMapped);
        }
        *entry = PageTableEntry::new_page(target.align_down(page_size), flags, page_size.is_huge());
        self.flush(vaddr);
        Ok(())
    }

    pub fn remap(
        &mut self,
        vaddr: M::VirtAddr,
        paddr: PhysAddr,
        flags: PagingFlags,
    ) -> PtResult<PageSize> {
        let (entry, size) = self.get_entry_mut(vaddr)?;
        entry.set_paddr(paddr);
        entry.set_flags(flags, size.is_huge());
        self.flush(vaddr);
        Ok(size)
    }

    pub fn protect(&mut self, vaddr: M::VirtAddr, flags: PagingFlags) -> PtResult<PageSize> {
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        entry.set_flags(flags, size.is_huge());
        self.flush(vaddr);
        Ok(size)
    }

    pub fn unmap(&mut self, vaddr: M::VirtAddr) -> PtResult<(PhysAddr, PagingFlags, PageSize)> {
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            entry.clear();
            return Err(PtError::NotMapped);
        }
        let paddr = entry.paddr();
        let flags = entry.flags();
        entry.clear();
        self.flush(vaddr);
        Ok((paddr, flags, size))
    }

    pub fn map_region(
        &mut self,
        vaddr: M::VirtAddr,
        phys_getter: impl Fn(M::VirtAddr) -> PhysAddr,
        size: usize,
        flags: PagingFlags,
        allow_huge: bool,
    ) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        if !PageSize::Size4K.is_aligned(vaddr_val) || !PageSize::Size4K.is_aligned(rem_size) {
            return Err(PtError::NotAligned);
        }
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let p_addr = phys_getter(v_addr);
            let p_size = if allow_huge {
                if PageSize::Size1G.is_aligned(vaddr_val)
                    && p_addr.is_aligned(PageSize::Size1G)
                    && rem_size >= PageSize::Size1G as usize
                {
                    PageSize::Size1G
                } else if PageSize::Size2M.is_aligned(vaddr_val)
                    && p_addr.is_aligned(PageSize::Size2M)
                    && rem_size >= PageSize::Size2M as usize
                {
                    PageSize::Size2M
                } else {
                    PageSize::Size4K
                }
            } else {
                PageSize::Size4K
            };
            self.map(v_addr, p_addr, p_size, flags)?;

            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    pub fn unmap_region(&mut self, vaddr: M::VirtAddr, size: usize) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let (_, _, p_size) = self.unmap(v_addr)?;
            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    pub fn protect_region(
        &mut self,
        vaddr: M::VirtAddr,
        size: usize,
        flags: PagingFlags,
    ) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let p_size = match self.protect(v_addr, flags) {
                Ok(s) => s,
                Err(PtError::NotMapped) => PageSize::Size4K,
                Err(e) => return Err(e),
            };
            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    #[cfg(feature = "copy-from")]
    pub fn copy_from(&mut self, other: &PageTable64<M, PTE, H>, start: M::VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let src_table = self.table_of(other.root_paddr);
        let dst_table = self.table_of_mut(self.root_paddr);
        let index_fn = if M::LEVELS == 3 {
            p3_idx
        } else if M::LEVELS == 4 {
            p4_idx
        } else {
            unreachable!()
        };
        let start_idx = index_fn(start.into());
        let end_idx = index_fn(start.into() + size - 1) + 1;
        for i in start_idx..end_idx {
            let entry = &mut dst_table[i];
            if !self.inner.borrowed_entries.set(i, true) && self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
            *entry = src_table[i];
        }
    }

    pub fn finish(&mut self) {
        #[cfg(not(docsrs))]
        match &self.flush {
            ToFlush::None => {}
            ToFlush::Addresses(addrs) => {
                for vaddr in addrs.iter() {
                    M::flush_tlb(Some(*vaddr));
                }
            }
            ToFlush::Full => {
                M::flush_tlb(None);
            }
        }
        self.flush = ToFlush::None;
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop
    for PageTableMut<'_, M, PTE, H>
{
    fn drop(&mut self) {
        self.finish();
    }
}
