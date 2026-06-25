// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX memory management syscall implementations.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod brk;
mod memfd;
mod mincore;
mod mmap;

pub use self::{brk::*, memfd::*, mincore::*, mmap::*};

#[cfg(unittest)]
mod tests {
    use khal::paging::{MappingFlags, PageSize};
    use linux_raw_sys::general::*;
    use memaddr::PAGE_SIZE_4K;
    use unittest::def_test;

    use crate::mmap::{MadviseRequest, MmapRequest, MprotectRequest, MsyncRequest, MunmapRequest};

    #[def_test]
    fn mmap_request_rejects_unknown_flags() {
        let unknown_flag = 1_u32 << 31;

        assert!(
            MmapRequest::from_raw(
                0,
                PAGE_SIZE_4K,
                PROT_READ,
                MAP_PRIVATE | MAP_ANONYMOUS | unknown_flag,
                -1,
                0,
            )
            .is_err()
        );
    }

    #[def_test]
    fn mmap_request_rejects_unknown_prot_bits() {
        let unknown_prot = 1_u32 << 31;

        assert!(
            MmapRequest::from_raw(
                0,
                PAGE_SIZE_4K,
                PROT_READ | unknown_prot,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
            .is_err()
        );
    }

    #[def_test]
    fn mmap_request_derives_current_and_may_permissions() {
        let request = MmapRequest::from_raw(
            0,
            PAGE_SIZE_4K,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
        .unwrap();

        assert!(request.permissions.current.contains(MappingFlags::USER));
        assert!(request.permissions.current.contains(MappingFlags::READ));
        assert!(request.permissions.current.contains(MappingFlags::WRITE));
        assert!(!request.permissions.current.contains(MappingFlags::EXECUTE));
        assert!(request.permissions.maximum.contains(MappingFlags::READ));
        assert!(request.permissions.maximum.contains(MappingFlags::WRITE));
        assert!(request.permissions.maximum.contains(MappingFlags::EXECUTE));
        assert_eq!(request.resolved_page_size(), PageSize::Size4K);
    }

    #[def_test]
    fn mmap_prot_none_keeps_mprotect_raise_possible() {
        let request = MmapRequest::from_raw(
            0,
            PAGE_SIZE_4K,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
        .unwrap();

        assert!(request.permissions.current.contains(MappingFlags::USER));
        assert!(!request.permissions.current.contains(MappingFlags::READ));
        assert!(!request.permissions.current.contains(MappingFlags::WRITE));
        assert!(request.permissions.maximum.contains(MappingFlags::READ));
        assert!(request.permissions.maximum.contains(MappingFlags::WRITE));
    }

    #[def_test]
    fn mmap_request_accepts_known_ignored_flags() {
        assert!(
            MmapRequest::from_raw(
                0,
                PAGE_SIZE_4K,
                PROT_READ,
                MAP_PRIVATE
                    | MAP_ANONYMOUS
                    | MAP_GROWSDOWN
                    | MAP_EXECUTABLE
                    | MAP_LOCKED
                    | MAP_NONBLOCK,
                -1,
                0,
            )
            .is_ok()
        );
    }

    #[def_test]
    fn mmap_request_rejects_shared_validate_unsupported_flags() {
        assert!(
            MmapRequest::from_raw(
                0,
                PAGE_SIZE_4K,
                PROT_READ,
                MAP_SHARED_VALIDATE | MAP_LOCKED,
                0,
                0,
            )
            .is_err()
        );
    }

    #[def_test]
    fn mmap_request_rejects_file_hugetlb() {
        assert!(
            MmapRequest::from_raw(0, PAGE_SIZE_4K, PROT_READ, MAP_PRIVATE | MAP_HUGETLB, 0, 0,)
                .is_err()
        );
    }

    #[def_test]
    fn mmap_request_rejects_unaligned_fixed_addr() {
        assert!(
            MmapRequest::from_raw(
                0x1234,
                PAGE_SIZE_4K,
                PROT_READ,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                -1,
                0,
            )
            .is_err()
        );
    }

    #[def_test]
    fn mmap_request_rejects_unaligned_fixed_noreplace_addr() {
        assert!(
            MmapRequest::from_raw(
                0x1234,
                PAGE_SIZE_4K,
                PROT_READ,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
            .is_err()
        );
    }

    #[def_test]
    fn munmap_request_rejects_zero_length() {
        assert!(MunmapRequest::from_raw(0x1000, 0).is_err());
    }

    #[def_test]
    fn mprotect_request_rejects_conflicting_grow_flags() {
        assert!(
            MprotectRequest::from_raw(
                0x1000,
                PAGE_SIZE_4K,
                PROT_READ | PROT_GROWSDOWN | PROT_GROWSUP,
            )
            .is_err()
        );
    }

    #[def_test]
    fn mprotect_request_rejects_single_grow_flag_as_unsupported() {
        assert!(
            MprotectRequest::from_raw(0x1000, PAGE_SIZE_4K, PROT_READ | PROT_GROWSDOWN).is_err()
        );
    }

    #[def_test]
    fn madvise_request_rejects_unsupported_advice() {
        assert!(MadviseRequest::dontneed_from_raw(0x1000, PAGE_SIZE_4K, 999).is_err());
    }

    #[def_test]
    fn msync_request_rejects_unknown_flags() {
        let unknown_flag = 1_u32 << 31;

        assert!(MsyncRequest::from_raw(0x1000, PAGE_SIZE_4K, unknown_flag).is_err());
    }

    #[def_test]
    fn msync_request_rejects_conflicting_sync_modes() {
        assert!(MsyncRequest::from_raw(0x1000, PAGE_SIZE_4K, MS_ASYNC | MS_SYNC).is_err());
    }

    #[def_test]
    fn msync_request_accepts_zero_length() {
        let request = MsyncRequest::from_raw(0x1000, 0, MS_SYNC).unwrap();
        assert!(request.is_empty());
    }

    #[def_test]
    fn msync_request_rejects_overflowing_range() {
        let addr = usize::MAX & !(PAGE_SIZE_4K - 1);

        assert!(MsyncRequest::from_raw(addr, PAGE_SIZE_4K, MS_SYNC).is_err());
    }

    #[def_test]
    fn msync_request_preserves_invalidate_policy() {
        let request = MsyncRequest::from_raw(0x1000, PAGE_SIZE_4K, MS_INVALIDATE).unwrap();

        assert!(request.policy().unwrap().has_invalidate());
    }
}
