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
    ) -> kerrno::KResult<Option<kiface_mm_api::DmaMappingCompletion>> {
        // Detach the remote invalidation from the kernel address-space lock;
        // the caller completes it after releasing its own locks. A CPU waiting
        // for either lock with interrupts disabled cannot answer the shootdown.
        let deferred = {
            let mut layout = crate::kernel_layout().lock();
            layout.protect_kernel_deferred(vaddr, size_bytes, mapping_flags(mode))?
        };
        Ok(deferred.map(|flush| {
            kiface_mm_api::DmaMappingCompletion::new(move || flush.complete().is_ok())
        }))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/dma_mapping.rs");
