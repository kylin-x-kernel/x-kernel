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

    use super::{AddrRange, PhysAddr, VirtAddr};

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
}
