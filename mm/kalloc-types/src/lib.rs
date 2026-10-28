// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible kernel-owned heap types.
//!
//! Allocation policy describes what the caller permits, not a guarantee of
//! success. The current backend never performs sleeping reclaim. Do not route
//! these types through `GlobalAlloc`, whose kernel OOM path is fatal.

#![no_std]
#![feature(derive_coerce_pointee)]

mod kbox;

pub use kbox::KBox;

bitflags::bitflags! {
    /// Composable permissions for an allocation attempt, not required actions.
    ///
    /// The current backend performs no reclaim, reclaim IO, or filesystem
    /// callbacks. These permissions constrain a future reclaim path; granting
    /// them does not make allocation succeed or enable an unimplemented feature.
    /// They are not Linux GFP bit values and do not select an allocator.
    ///
    /// Without `DIRECT_RECLAIM`, allocation must not sleep even if `IO` or `FS`
    /// is set. With it, the caller must allow sleeping. Reclaim IO and filesystem
    /// callbacks additionally require their respective permissions. Flags apply
    /// only to this allocation, not to nested allocations or task-wide scopes.
    /// The caller declares these permissions; no runtime context detection or
    /// automatic fallback is performed. `NOIO`/`NOFS` are not atomic-context
    /// policies: both retain `DIRECT_RECLAIM` and therefore permit sleeping.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct AllocFlags: u32 {
        /// Permits synchronous reclaim that may sleep.
        const DIRECT_RECLAIM = 1 << 0;
        /// Permits reclaim to initiate IO when direct reclaim is allowed.
        const IO = 1 << 1;
        /// Permits reclaim to enter the filesystem when direct reclaim is allowed.
        const FS = 1 << 2;

        /// Default task-context policy: permits sleeping reclaim, IO and FS entry.
        const KERNEL = Self::DIRECT_RECLAIM.bits() | Self::IO.bits() | Self::FS.bits();
        /// No sleeping reclaim, IO or FS entry; no reserve or latency guarantee.
        const NOWAIT = 0;
        /// Permits sleeping reclaim without IO or filesystem entry.
        const NOIO = Self::DIRECT_RECLAIM.bits();
        /// Permits sleeping reclaim and IO, but not filesystem entry.
        const NOFS = Self::DIRECT_RECLAIM.bits() | Self::IO.bits();
    }
}

impl Default for AllocFlags {
    fn default() -> Self {
        Self::KERNEL
    }
}
