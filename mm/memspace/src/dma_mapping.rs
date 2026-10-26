// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel mapping attributes for DMA-owned allocations.

use khal::paging::MappingFlags;
use kiface_mm_api::DmaMappingMode;

fn mapping_flags(mode: DmaMappingMode) -> MappingFlags {
    let access = MappingFlags::READ | MappingFlags::WRITE;
    match mode {
        DmaMappingMode::Coherent => access | MappingFlags::UNCACHED | MappingFlags::SHARED,
        DmaMappingMode::Normal => access,
    }
}

#[kiface::provide]
impl kiface_mm_api::DmaMappingIf {
    fn set_mode(
        vaddr: memaddr::VirtAddr,
        size_bytes: usize,
        mode: DmaMappingMode,
    ) -> kerrno::KResult {
        crate::kernel_layout()
            .lock()
            .protect(vaddr, size_bytes, mapping_flags(mode))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/dma_mapping.rs");
