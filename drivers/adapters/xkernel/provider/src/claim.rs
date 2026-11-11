// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_os_api::MmioClaimOp`] contract.
//!
//! The provider type stays a stateless dispatcher; the process-wide range
//! arbitration table lives in this adapter, next to the host capabilities it
//! arbitrates.

use alloc::vec::Vec;
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use driver_os_api::{MmioClaimOp, MmioClaimToken, MmioRegion, ResError, ResResult};
use ksync::{Mutex, static_lock};

use crate::XKernelResourceProvider;

struct ClaimEntry {
    region: MmioRegion,
    end: usize,
    token: MmioClaimToken,
    name: &'static str,
}

static_lock! {
    /// Physical MMIO ranges owned through the shared claim contract.
    ///
    /// Only probe and device-removal task paths claim or release ranges; no
    /// IRQ handler or I/O completion path reads this table. A sleeping mutex is
    /// therefore the correct exclusion primitive (CG-014), and the fallible
    /// `Vec::try_reserve` below runs under a sleepable lock (CG-013).
    ///
    /// This is a leaf lock: no kdevice/kclass call may run while it is held.
    static CLAIMS: Mutex<Vec<ClaimEntry>> = Mutex::new(Vec::new());
}

static NEXT_CLAIM_ID: AtomicUsize = AtomicUsize::new(1);

// SAFETY: `claim_mmio` checks every live entry before inserting, so successful
// claims are pairwise non-overlapping. `release_mmio` removes exactly the entry
// matching the supplied token and ignores unknown tokens.
unsafe impl MmioClaimOp for XKernelResourceProvider {
    fn claim_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioClaimToken> {
        if region.size == 0 {
            return Err(ResError::InvalidResource);
        }
        let end = region
            .base
            .checked_add(region.size)
            .ok_or(ResError::InvalidResource)?;
        let mut claims = CLAIMS.lock();
        for entry in &*claims {
            if region.base < entry.end && entry.region.base < end {
                log::debug!(
                    "MMIO claim {name} at {:#x}+{:#x} overlaps {} at {:#x}+{:#x}",
                    region.base,
                    region.size,
                    entry.name,
                    entry.region.base,
                    entry.region.size,
                );
                return Err(ResError::Busy);
            }
        }
        claims.try_reserve(1).map_err(|_| ResError::NoMemory)?;
        let id = NonZeroUsize::new(NEXT_CLAIM_ID.fetch_add(1, Ordering::Relaxed))
            .ok_or(ResError::NoMemory)?;

        let token = MmioClaimToken::new(region, id);
        claims.push(ClaimEntry {
            region,
            end,
            token,
            name,
        });
        Ok(token)
    }

    unsafe fn release_mmio(&self, token: MmioClaimToken) {
        let mut claims = CLAIMS.lock();
        match claims.iter().position(|entry| entry.token == token) {
            Some(index) => {
                claims.swap_remove(index);
            }
            None => log::warn!("released unknown MMIO claim {token:?}"),
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/claim.rs");
