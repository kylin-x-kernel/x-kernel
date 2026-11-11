// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{MmioClaim, MmioClaimOp, MmioClaimToken, MmioRegion};
use crate::{ResError, ResResult};

const REGION: MmioRegion = MmioRegion::new(0x8000_0000, 0x1000);

/// Single-owner fake provider: a second claim is busy until the first drop.
struct Provider {
    claimed: AtomicBool,
    releases: AtomicUsize,
    next_id: AtomicUsize,
}

impl Provider {
    const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            releases: AtomicUsize::new(0),
            next_id: AtomicUsize::new(1),
        }
    }
}

// SAFETY: the fake provider hands out at most one live claim at a time; the
// test releases it only through the guard that owns the token.
unsafe impl MmioClaimOp for Provider {
    fn claim_mmio(&self, region: MmioRegion, _name: &'static str) -> ResResult<MmioClaimToken> {
        if region.size == 0 {
            return Err(ResError::InvalidResource);
        }
        if self.claimed.swap(true, Ordering::SeqCst) {
            return Err(ResError::Busy);
        }
        let id = NonZeroUsize::new(self.next_id.fetch_add(1, Ordering::SeqCst)).unwrap();
        Ok(MmioClaimToken::new(region, id))
    }

    unsafe fn release_mmio(&self, token: MmioClaimToken) {
        core::assert_eq!(token.region(), REGION);
        core::assert!(self.claimed.swap(false, Ordering::SeqCst));
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[def_test(serial)]
fn claim_guard_is_exclusive_and_releases_once() {
    static PROVIDER: Provider = Provider::new();
    let provider = &PROVIDER;
    let releases = provider.releases.load(Ordering::SeqCst);

    let claim = MmioClaim::claim_with(provider, REGION, "first").unwrap();
    assert_eq!(claim.region(), REGION);
    assert!(matches!(
        MmioClaim::claim_with(provider, REGION, "second"),
        Err(ResError::Busy)
    ));
    assert_eq!(provider.releases.load(Ordering::SeqCst), releases);

    drop(claim);
    assert_eq!(provider.releases.load(Ordering::SeqCst), releases + 1);

    let again = MmioClaim::claim_with(provider, REGION, "again").unwrap();
    drop(again);
    assert_eq!(provider.releases.load(Ordering::SeqCst), releases + 2);
}

#[def_test(serial)]
fn failed_claim_creates_no_release_obligation() {
    static PROVIDER: Provider = Provider::new();
    let provider = &PROVIDER;
    let releases = provider.releases.load(Ordering::SeqCst);

    assert!(matches!(
        MmioClaim::claim_with(provider, MmioRegion::new(REGION.base, 0), "empty"),
        Err(ResError::InvalidResource)
    ));
    assert_eq!(provider.releases.load(Ordering::SeqCst), releases);
    assert!(!provider.claimed.load(Ordering::SeqCst));

    // The failed acquisition did not leave the range claimed.
    let claim = MmioClaim::claim_with(provider, REGION, "valid").unwrap();
    drop(claim);
    assert_eq!(provider.releases.load(Ordering::SeqCst), releases + 1);
}
