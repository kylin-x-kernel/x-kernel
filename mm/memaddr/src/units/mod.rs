// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address types and iterators with units.
mod addr;
mod iter;
mod range;

pub use self::{
    addr::{AddrOps, MemoryAddr, PhysAddr, VirtAddr},
    iter::{DynPageIter, PageIter},
    range::{AddrRange, PhysAddrRange, VirtAddrRange},
};

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_units {
    use unittest::def_test;

    use super::{AddrRange, MemoryAddr, PhysAddr, VirtAddr};

    #[def_test]
    fn test_virt_phys_addr_from_usize() {
        let va = VirtAddr::from(0x1234usize);
        let pa = PhysAddr::from(0x5678usize);
        assert_eq!(va.as_usize(), 0x1234);
        assert_eq!(pa.as_usize(), 0x5678);
    }

    #[def_test]
    fn test_addr_range_default_empty() {
        let range: AddrRange<VirtAddr> = AddrRange::default();
        assert!(range.is_empty());
    }

    #[def_test]
    fn test_addr_range_contains() {
        let range = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
        assert!(range.contains(VirtAddr::from(0x1000usize)));
        assert!(!range.contains(VirtAddr::from(0x2000usize)));
    }

    #[def_test]
    fn test_memory_addr_checked_and_overflowing_ops() {
        let addr = VirtAddr::from(0x1000usize);
        assert_eq!(addr.checked_add(0x20), Some(VirtAddr::from(0x1020usize)));
        assert_eq!(addr.checked_sub(0x20), Some(VirtAddr::from(0x0fe0usize)));

        let (wrapped_add, add_overflow) = VirtAddr::from(usize::MAX).overflowing_add(1);
        assert_eq!(wrapped_add, VirtAddr::from(0usize));
        assert!(add_overflow);

        let (wrapped_sub, sub_overflow) = VirtAddr::from(0usize).overflowing_sub(1);
        assert_eq!(wrapped_sub, VirtAddr::from(usize::MAX));
        assert!(sub_overflow);
    }

    #[def_test]
    fn test_memory_addr_wrapping_and_diff_ops() {
        let base = VirtAddr::from(0x1000usize);
        let next = base.add(0x234);
        assert_eq!(next.sub_addr(base), 0x234);
        assert_eq!(next.checked_sub_addr(base), Some(0x234));
        assert_eq!(base.wrapping_add(usize::MAX), VirtAddr::from(0x0fffusize));
        assert_eq!(base.wrapping_offset(-0x10), VirtAddr::from(0x0ff0usize));
        assert_eq!(next.offset_from(base), 0x234);
    }

    #[def_test]
    fn test_memory_addr_alignment_helpers() {
        let addr = VirtAddr::from(0x1234usize);
        assert_eq!(addr.align_down(0x1000usize), VirtAddr::from(0x1000usize));
        assert_eq!(addr.align_up(0x1000usize), VirtAddr::from(0x2000usize));
        assert_eq!(addr.align_offset(0x1000usize), 0x234);
        assert!(!addr.is_aligned(0x1000usize));
        assert!(VirtAddr::from(0x2000usize).is_aligned(0x1000usize));
    }

    #[def_test]
    fn test_memory_addr_checked_failures() {
        let zero = PhysAddr::from(0usize);
        assert_eq!(zero.checked_sub(1), None);
        assert_eq!(VirtAddr::from(usize::MAX).checked_add(1), None);
        assert_eq!(
            VirtAddr::from(0x1000usize).checked_sub_addr(VirtAddr::from(0x2000usize)),
            None
        );
    }
}
