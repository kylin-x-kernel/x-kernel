// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

struct DmaPageTableImpl;

#[crate_interface::impl_interface]
impl kdma::DmaPageTableIf for DmaPageTableImpl {
    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        memspace::kernel_layout().lock().protect(vaddr, size, flags)
    }
}
