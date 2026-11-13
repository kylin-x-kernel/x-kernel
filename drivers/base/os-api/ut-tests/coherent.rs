// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{
    CoherentArray, CoherentBox, CoherentMem, DmaAllocation, DmaDirection, DmaOp, DmaSpec, ResError,
    ResResult,
};

struct Provider {
    releases: AtomicUsize,
    released_len: AtomicUsize,
    released_align: AtomicUsize,
    released_direction: AtomicUsize,
}

static PROVIDER: Provider = Provider {
    releases: AtomicUsize::new(0),
    released_len: AtomicUsize::new(0),
    released_align: AtomicUsize::new(0),
    released_direction: AtomicUsize::new(0),
};

impl DmaOp for Provider {
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        std_box_in_kernel_tests,
        reason = "CG-049-no-std-box-in-kernel-tests: the DMA test provider transfers a \
                  GlobalAlloc Box array through cpu_addr and reconstructs that exact allocation \
                  once at free_coherent"
    )]
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        if spec.len > 64 || spec.align > 8 {
            return Err(ResError::NoMemory);
        }
        Ok(DmaAllocation {
            cpu_addr: Box::into_raw(Box::new([0u64; 8])) as usize,
            bus_addr: 0,
            spec,
        })
    }

    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        std_box_in_kernel_tests,
        reason = "CG-049-no-std-box-in-kernel-tests: the DMA test provider transfers a \
                  GlobalAlloc Box array through cpu_addr and reconstructs that exact allocation \
                  once at free_coherent"
    )]
    fn free_coherent(&self, allocation: DmaAllocation) {
        core::assert_eq!(allocation.bus_addr, 0);
        self.released_len
            .store(allocation.spec.len, Ordering::Relaxed);
        self.released_align
            .store(allocation.spec.align, Ordering::Relaxed);
        self.released_direction
            .store(allocation.spec.direction as usize, Ordering::Relaxed);
        // SAFETY: these tests return only the original token through the sole
        // guard owner; allocation retains the exact Box array pointer and type.
        drop(unsafe { Box::from_raw(allocation.cpu_addr as *mut [u64; 8]) });
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn coherent_guard_preserves_original_release_spec_and_zero_bus_address() {
    let initial = PROVIDER.releases.load(Ordering::Relaxed);
    let spec = DmaSpec::new(24, 8).with_direction(DmaDirection::DeviceToDriver);
    let memory = CoherentMem::alloc_with(&PROVIDER, spec).unwrap();
    assert_eq!(memory.len(), 24);
    assert_eq!(memory.bus_addr(), 0);
    assert_eq!((memory.as_ptr() as usize) % 8, 0);
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial);
    drop(memory);
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial + 1);
    assert_eq!(PROVIDER.released_len.load(Ordering::Relaxed), spec.len);
    assert_eq!(PROVIDER.released_align.load(Ordering::Relaxed), spec.align);
    assert_eq!(
        PROVIDER.released_direction.load(Ordering::Relaxed),
        spec.direction as usize
    );
    assert_eq!(
        CoherentMem::alloc_with(&PROVIDER, DmaSpec::new(65, 8)).unwrap_err(),
        ResError::NoMemory
    );
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial + 1);
}

#[def_test(serial)]
fn typed_coherent_views_use_the_same_guard_and_release_once() {
    let initial = PROVIDER.releases.load(Ordering::Relaxed);
    let value = CoherentBox::<u64>::alloc_with(&PROVIDER, 8).unwrap();
    let array = CoherentArray::<u32>::alloc_with(&PROVIDER, 3, 8).unwrap();
    assert_eq!(value.len(), 8);
    assert_eq!(value.bus_addr(), 0);
    assert_eq!(array.count(), 3);
    assert_eq!(array.bus_addr_of(2), 8);
    drop(value);
    drop(array);
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial + 2);
}
