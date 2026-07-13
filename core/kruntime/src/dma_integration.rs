// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[kiface::provide]
impl kdma::DmaPageTableIf {
    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        memspace::kernel_layout().lock().protect(vaddr, size, flags)
    }
}
