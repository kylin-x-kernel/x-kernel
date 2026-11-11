// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::num::NonZeroUsize;

use driver_os_api::{MmioClaim, MmioClaimOp, MmioClaimToken, MmioRegion, ResError};
use unittest::{assert, def_test};

use super::XKernelResourceProvider;

const OWNER_BASE: usize = 0x6c00_0000;
const INVALID_BASE: usize = 0x6c10_0000;
const SIZE: usize = 0x1000;

static PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

#[def_test(serial)]
fn claims_are_exclusive_until_the_owner_drops() {
    let owner = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE, SIZE),
        "claim-test-owner",
    )
    .unwrap();

    // Identical, left-overlap, right-overlap, subset and superset all conflict.
    for region in [
        MmioRegion::new(OWNER_BASE, SIZE),
        MmioRegion::new(OWNER_BASE - 0x10, 0x20),
        MmioRegion::new(OWNER_BASE + SIZE - 0x10, 0x20),
        MmioRegion::new(OWNER_BASE + 0x10, 0x10),
        MmioRegion::new(OWNER_BASE - SIZE, SIZE * 3),
    ] {
        assert!(matches!(
            MmioClaim::claim_with(&PROVIDER, region, "claim-test-conflict"),
            Err(ResError::Busy)
        ));
    }

    // Adjacent ranges do not overlap and may be claimed concurrently.
    let after = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE + SIZE, SIZE),
        "claim-test-after",
    )
    .unwrap();
    let before = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE - SIZE, SIZE),
        "claim-test-before",
    )
    .unwrap();

    drop(owner);
    let reacquired = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE, SIZE),
        "claim-test-reacquired",
    )
    .unwrap();

    drop(reacquired);
    drop(after);
    drop(before);
}

#[def_test(serial)]
fn invalid_ranges_transfer_no_claim() {
    assert!(matches!(
        MmioClaim::claim_with(
            &PROVIDER,
            MmioRegion::new(INVALID_BASE, 0),
            "claim-test-empty"
        ),
        Err(ResError::InvalidResource)
    ));
    assert!(matches!(
        MmioClaim::claim_with(
            &PROVIDER,
            MmioRegion::new(usize::MAX, SIZE),
            "claim-test-overflow"
        ),
        Err(ResError::InvalidResource)
    ));

    // Failed acquisitions inserted nothing.
    let claim = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(INVALID_BASE, SIZE),
        "claim-test-valid",
    )
    .unwrap();
    drop(claim);
}

#[def_test(serial)]
fn unknown_token_does_not_release_the_owner() {
    let owner = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE, SIZE),
        "claim-test-unknown-owner",
    )
    .unwrap();
    let unknown = MmioClaimToken::new(
        MmioRegion::new(OWNER_BASE, SIZE),
        NonZeroUsize::new(usize::MAX).unwrap(),
    );

    // SAFETY: deliberately violates the token precondition to exercise the
    // fail-closed defense for a token this provider never issued. Release must
    // only warn, not panic or remove the live owner.
    unsafe { PROVIDER.release_mmio(unknown) };

    assert!(matches!(
        MmioClaim::claim_with(
            &PROVIDER,
            MmioRegion::new(OWNER_BASE, SIZE),
            "claim-test-after-unknown"
        ),
        Err(ResError::Busy)
    ));

    drop(owner);
    let reacquired = MmioClaim::claim_with(
        &PROVIDER,
        MmioRegion::new(OWNER_BASE, SIZE),
        "claim-test-after-drop",
    )
    .unwrap();
    drop(reacquired);
}
