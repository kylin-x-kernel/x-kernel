// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical MMIO range ownership: the `request_mem_region` counterpart.
//!
//! A claim is a range-scoped ownership token recorded by the host provider. It
//! is separate from [`crate::IoMap`]: a mapping reference only keeps a virtual
//! mapping alive, while a claim states that this range has no other claim
//! participant. Callers that need exclusive register access acquire a claim
//! before the first register access and retain it until the last access has
//! ended. The guarantee covers claim participants, not every mapping path in
//! the system.

use core::{fmt, num::NonZeroUsize};

use crate::{MmioRegion, ResResult};

/// An unreleased acquisition returned by [`MmioClaimOp::claim_mmio`].
///
/// A token identifies one claim exactly; it does not transfer ownership when
/// copied. Only the original unreleased token may be passed to
/// [`MmioClaimOp::release_mmio`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioClaimToken {
    region: MmioRegion,
    id: NonZeroUsize,
}

impl MmioClaimToken {
    /// Builds a token for a provider-owned claim record.
    pub const fn new(region: MmioRegion, id: NonZeroUsize) -> Self {
        Self { region, id }
    }

    /// The claimed physical range.
    pub const fn region(self) -> MmioRegion {
        self.region
    }

    /// The provider-assigned claim identifier.
    pub const fn id(self) -> NonZeroUsize {
        self.id
    }
}

/// Physical MMIO range ownership capability.
///
/// A host kernel implements this and passes it to its driver framework.
/// Claim and release run in normal (non-interrupt), sleepable context during
/// device probe and removal; they must not be called with local IRQs disabled
/// or under a spin/preemption guard.
///
/// # Safety
///
/// Successful claims returned by all calls to the same provider must be
/// pairwise non-overlapping. A failed claim transfers no ownership and imposes
/// no release obligation. Implementations may block while claiming.
pub unsafe trait MmioClaimOp: Sync {
    /// Claim `region` for this caller, or fail if any live claim overlaps it.
    ///
    /// `name` is diagnostic only. Overlap must fail with [`crate::ResError::Busy`]
    /// without touching the range.
    fn claim_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioClaimToken>;

    /// Release a claim returned by [`Self::claim_mmio`].
    ///
    /// # Safety
    ///
    /// `token` must be an unreleased acquisition returned by this provider, and
    /// all accesses through the claimed range must have ended.
    unsafe fn release_mmio(&self, token: MmioClaimToken);
}

/// An exclusive claim on a physical MMIO range, released on drop.
///
/// The guard retains the provider and the exact acquisition token, so it can
/// release only the claim it owns, exactly once. Failed acquisitions produce no
/// guard.
///
/// The final drop releases the range through [`MmioClaimOp::release_mmio`].
/// The owner must ensure that drop runs in normal, sleepable context with local
/// IRQs enabled and without a spin or preemption guard, even when the last
/// reference is released outside the remove function (for example from an
/// owned transport's `Drop`).
#[must_use = "dropping the claim immediately releases the range"]
pub struct MmioClaim {
    provider: &'static dyn MmioClaimOp,
    token: MmioClaimToken,
}

impl fmt::Debug for MmioClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MmioClaim")
            .field("region", &self.token.region())
            .finish_non_exhaustive()
    }
}

impl MmioClaim {
    /// Claim a physical range through `provider`.
    pub fn claim_with(
        provider: &'static dyn MmioClaimOp,
        region: MmioRegion,
        name: &'static str,
    ) -> ResResult<Self> {
        let token = provider.claim_mmio(region, name)?;
        Ok(Self { provider, token })
    }

    /// The claimed physical range.
    pub const fn region(&self) -> MmioRegion {
        self.token.region()
    }
}

impl Drop for MmioClaim {
    fn drop(&mut self) {
        // SAFETY: this non-Clone guard retained the only token for its claim,
        // and its owner releases it only after all range accesses have ended.
        unsafe { self.provider.release_mmio(self.token) };
    }
}
