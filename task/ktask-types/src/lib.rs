// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Flag-reserved task owner keys for lock words.
//!
//! [`OwnerKey`] is an identity token left-shifted by [`OwnerKey::FLAG_BITS`],
//! so its low bits are zero and a lock word can pack `WAITERS`/`HANDOFF`/
//! `PICKUP`-style flags into them without aliasing another task. The
//! "nonzero, aligned, non-overflowing" invariant is enforced by the
//! constructors rather than left as a caller contract.

#![no_std]
#![deny(unsafe_code)]

use core::num::NonZeroU64;

/// An aligned task identity for use inside a lock owner word.
///
/// The value is the task's dense identity token shifted left by
/// [`OwnerKey::FLAG_BITS`], which keeps the low bits clear for lock-word
/// flags. `0` is not a valid owner and is reserved for an unlocked word.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OwnerKey(NonZeroU64);

impl OwnerKey {
    /// Alignment every owner key keeps: its low [`Self::FLAG_BITS`] bits clear.
    pub const ALIGN: u64 = 1u64 << Self::FLAG_BITS;
    /// Number of low bits reserved for lock-word flags.
    pub const FLAG_BITS: u32 = 3;
    /// Mask of the reserved flag bits: the whole low flag field.
    pub const FLAG_MASK: u64 = (1u64 << Self::FLAG_BITS) - 1;

    /// Projects a dense identity token into its aligned owner key.
    ///
    /// - `token`: the dense, nonzero identity token.
    ///
    /// Returns: `Some(key)` when `token` is nonzero and its aligned projection
    /// does not overflow; `None` for `0` or any token above
    /// `u64::MAX >> FLAG_BITS`.
    pub const fn from_token(token: u64) -> Option<Self> {
        if token > u64::MAX >> Self::FLAG_BITS {
            return None;
        }
        let Some(inner) = NonZeroU64::new(token << Self::FLAG_BITS) else {
            return None;
        };
        Some(Self(inner))
    }

    /// Wraps an already-aligned raw key, rejecting flag bits and zero.
    ///
    /// - `raw`: the aligned key, with the low [`Self::FLAG_BITS`] bits clear.
    ///
    /// Returns: `Some(key)` when `raw` is nonzero and its low flag bits are
    /// clear; `None` otherwise.
    pub const fn from_aligned(raw: u64) -> Option<Self> {
        if raw & Self::FLAG_MASK != 0 {
            return None;
        }
        let Some(inner) = NonZeroU64::new(raw) else {
            return None;
        };
        Some(Self(inner))
    }

    /// Returns the raw aligned key for storage in an owner word.
    pub const fn as_raw(self) -> u64 {
        self.0.get()
    }
}

#[cfg(unittest)]
#[path = "../ut-tests/owner_key.rs"]
mod owner_key_tests;
