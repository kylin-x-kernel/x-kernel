// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::paging::{MappingFlags, PageSize};
use memaddr::VirtAddr;
use memspace::VmRuntimeRef;
use unittest::def_test;

use super::*;

#[def_test]
fn address_space_limit_accounts_for_replacement_holes_and_growth() {
    let mut aspace = MmSpace::new_empty_user(VirtAddr::from_usize(0x1000), 0x100000).unwrap();
    for start in [0x1000, 0x3000] {
        let start = VirtAddr::from_usize(start);
        aspace.map(start, 0x1000, MappingFlags::READ | MappingFlags::USER, false,
            VmRuntimeRef::new_anon_private(start, PageSize::Size4K)).unwrap();
    }
    assert_eq!(check_address_space_limit(&aspace, 0x1000, None, 0x3000), Ok(()));
    assert_eq!(check_address_space_limit(&aspace, 0x2000, None, 0x3000), Err(KError::NoMemory));
    let replacement = Some(VirtAddrRange::from_start_size(VirtAddr::from_usize(0x1000), 0x3000));
    assert_eq!(check_address_space_limit(&aspace, 0x3000, replacement, 0x3000), Ok(()));
    assert_eq!(check_address_space_limit(&aspace, 0x3000, replacement, 0x2000), Err(KError::NoMemory));
    assert_eq!(check_address_space_limit(&aspace, 0, None, 0x1000), Err(KError::NoMemory));
    assert_eq!(check_address_space_limit(&aspace, usize::MAX, None, u64::MAX), Ok(()));
}
