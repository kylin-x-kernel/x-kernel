// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address types and iterators with units.
mod addr;
mod iter;
mod range;

#[cfg(feature = "memaddr_test")]
pub use self::iter::tests_iter;
#[cfg(feature = "memaddr_test")]
pub use self::range::tests_range;
pub use self::{
    addr::{AddrOps, MemoryAddr, PhysAddr, VirtAddr},
    iter::{DynPageIter, PageIter},
    range::{AddrRange, PhysAddrRange, VirtAddrRange},
};

#[cfg(feature = "memaddr_test")]
#[allow(missing_docs)]
pub mod tests_units {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::{AddrRange, PhysAddr, VirtAddr};

    test_fn! {
        using TestResult;

        fn test_virt_phys_addr_from_usize() {
            let va = VirtAddr::from(0x1234usize);
            let pa = PhysAddr::from(0x5678usize);
            assert_eq!(va.as_usize(), 0x1234);
            assert_eq!(pa.as_usize(), 0x5678);
        }
    }

    test_fn! {
        using TestResult;

        fn test_addr_range_default_empty() {
            let range: AddrRange<VirtAddr> = AddrRange::default();
            assert!(range.is_empty());
        }
    }

    test_fn! {
        using TestResult;

        fn test_addr_range_contains() {
            let range = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
            assert!(range.contains(VirtAddr::from(0x1000usize)));
            assert!(!range.contains(VirtAddr::from(0x2000usize)));
        }
    }

    tests_name! {
        TEST_UNITS;
        test_virt_phys_addr_from_usize,
        test_addr_range_default_empty,
        test_addr_range_contains,
    }
}
