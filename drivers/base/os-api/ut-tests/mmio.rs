// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use super::{IoMap, MmioMapping, MmioOp, MmioRegion};
use crate::{ResError, ResResult};

struct Provider {
    releases: AtomicUsize,
}

// SAFETY: each successful map owns a separate aligned Box allocation. Only the
// matching unmap frees it; release accounting is atomic across callers.
unsafe impl MmioOp for Provider {
    fn map_mmio(&self, region: MmioRegion, _name: &'static str) -> ResResult<MmioMapping> {
        if region.size == 0 || region.size > 64 {
            return Err(ResError::InvalidResource);
        }
        Ok(MmioMapping {
            vaddr: Box::into_raw(Box::new([0u64; 8])) as usize,
            region,
        })
    }

    unsafe fn unmap_mmio(&self, mapping: MmioMapping) {
        // SAFETY: the caller returns this provider's original allocation once,
        // after all accesses have ended, with its exact array type preserved.
        drop(unsafe { Box::from_raw(mapping.vaddr as *mut [u64; 8]) });
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn provider_mapping_preserves_register_access_and_releases_once() {
    static PROVIDER: Provider = Provider {
        releases: AtomicUsize::new(0),
    };
    let provider = &PROVIDER;
    let initial_releases = provider.releases.load(Ordering::Relaxed);
    let region = MmioRegion::new(0x2000, 64);
    let first = IoMap::map_with(provider, region, "first").unwrap();
    let second = IoMap::map_with(provider, region, "second").unwrap();
    first.write8(0, 0x12);
    first.write16(2, 0x3456);
    first.write32(4, 0x789abcde);
    first.write64(8, 0x123456789abcdef0);
    assert_eq!(first.read8(0), 0x12);
    assert_eq!(first.read16(2), 0x3456);
    assert_eq!(first.read32(4), 0x789abcde);
    assert_eq!(first.read64(8), 0x123456789abcdef0);
    assert_eq!(first.region(), region);
    drop(first);
    assert_eq!(
        provider.releases.load(Ordering::Relaxed),
        initial_releases + 1
    );
    second.write32(0, 42);
    assert_eq!(second.read32(0), 42);
    drop(second);
    assert_eq!(
        provider.releases.load(Ordering::Relaxed),
        initial_releases + 2
    );
    assert_eq!(
        IoMap::map_with(provider, MmioRegion::new(0, 0), "invalid").unwrap_err(),
        ResError::InvalidResource
    );
    assert_eq!(
        provider.releases.load(Ordering::Relaxed),
        initial_releases + 2
    );
}
