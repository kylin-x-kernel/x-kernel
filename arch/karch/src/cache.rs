// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compile-time cache-line isolation for independently updated kernel objects.
//!
//! This layout policy is not a hardware cache-line query, DMA alignment contract,
//! or cache-maintenance instruction stride. Each architecture selects the layout
//! in its own `cache_layout.rs`; there is no generic alignment fallback.
//! It remains enabled in UP builds so layout is independent of the `smp` feature.
//! Prefer [`CacheAligned`] over per-caller alignment literals.

use core::ops::{Deref, DerefMut};

/// Minimum alignment used to separate independently updated SMP state.
///
/// Derived from the wrapper so consumers and the representation cannot disagree.
/// This is a compile-time layout policy, not the detected L1 cache-line size.
pub const SMP_CACHE_BYTES: usize = align_of::<CacheAligned<()>>();

// Rust repr alignment needs a literal. The architecture supplies that literal
// once, generating the type from which SMP_CACHE_BYTES is derived.
macro_rules! define_cache_alignment {
    ($bytes:literal) => {
        /// Aligns an owned value and pads its array stride to the cache-isolation policy.
        ///
        /// Wrap the entire synchronization unit, such as a lock and its protected data.
        /// Alignment does not synchronize access, pin the value, or isolate subfields.
        /// Values with stronger natural alignment retain it. Zero-sized values remain
        /// zero-sized and need not occupy distinct addresses.
        ///
        /// # Examples
        ///
        /// ```
        /// use core::sync::atomic::{AtomicUsize, Ordering};
        ///
        /// use karch::cache::{CacheAligned, SMP_CACHE_BYTES};
        ///
        /// let counters = [const { CacheAligned::new(AtomicUsize::new(0)) }; 2];
        /// counters[0].fetch_add(1, Ordering::Relaxed);
        /// assert_eq!(counters[0].load(Ordering::Relaxed), 1);
        /// assert!(size_of_val(&counters[0]).is_multiple_of(SMP_CACHE_BYTES));
        /// ```
        #[repr(align($bytes))]
        pub struct CacheAligned<T>(pub(super) T);
    };
}

#[cfg_attr(target_arch = "aarch64", path = "aarch64/cache_layout.rs")]
#[cfg_attr(target_arch = "x86_64", path = "x86_64/cache_layout.rs")]
#[cfg_attr(
    any(target_arch = "riscv32", target_arch = "riscv64"),
    path = "riscv/cache_layout.rs"
)]
#[cfg_attr(target_arch = "loongarch64", path = "loongarch64/cache_layout.rs")]
#[cfg_attr(target_arch = "arm", path = "arm/cache_layout.rs")]
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "arm"
))]
mod layout;

#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "arm"
)))]
compile_error!("karch cache alignment needs an explicit architecture policy");

pub use layout::CacheAligned;

impl<T> CacheAligned<T> {
    /// Wraps `value` without allocation or any execution-context requirement.
    pub const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> Deref for CacheAligned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for CacheAligned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonempty_array_elements_are_isolated() {
        let values = [
            CacheAligned::new([0u8; SMP_CACHE_BYTES + 1]),
            CacheAligned::new([0u8; SMP_CACHE_BYTES + 1]),
        ];
        let first = core::ptr::from_ref(&values[0]).addr();
        let second = core::ptr::from_ref(&values[1]).addr();
        assert_eq!(first % SMP_CACHE_BYTES, 0);
        assert_eq!(second % SMP_CACHE_BYTES, 0);
        assert_eq!(second - first, size_of_val(&values[0]));
        assert!((first + size_of_val(&values[0]) - 1) / SMP_CACHE_BYTES < second / SMP_CACHE_BYTES);
    }

    #[test]
    fn stronger_alignment_and_value_access_are_preserved() {
        #[repr(align(256))]
        struct Strong(u32);
        let mut value = CacheAligned::new(Strong(7));
        assert_eq!(align_of_val(&value), 256.max(SMP_CACHE_BYTES));
        value.deref_mut().0 += 1;
        assert_eq!(value.deref().0, 8);
    }
}
