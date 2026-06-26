// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime state management.
//!
//! Owns `ProfileImage` and `ValueProfileStore` through `lazy_static`.
//! Captures snapshots for serialization without holding locks during I/O.

use core::sync::atomic::Ordering;

use lazy_static::lazy_static;
use portable_atomic::{AtomicBool, AtomicU32};

#[cfg(feature = "alloc")]
use crate::image::{ProfileImage, ProfileSnapshot, ValueProfileStore};
use crate::state;

/// Maximum number of value profiling entries per site.
static VP_MAX_NUM_VALS_PER_SITE: AtomicU32 = AtomicU32::new(24);

lazy_static! {
    static ref RUNTIME: Runtime = Runtime::new();
}

/// Global profile runtime.
///
/// Owns the profile image and value profiling store. All access goes
/// through `Mutex`-protected methods — no raw `static mut` needed.
#[cfg(feature = "alloc")]
pub(crate) struct Runtime {
    image: spin::Mutex<Option<ProfileImage>>,
    value: spin::Mutex<ValueProfileStore>,
    dumped: AtomicBool,
}

#[cfg(feature = "alloc")]
impl Runtime {
    fn new() -> Self {
        let max_vals = VP_MAX_NUM_VALS_PER_SITE.load(Ordering::Relaxed) as usize;
        Self {
            image: spin::Mutex::new(None),
            value: spin::Mutex::new(ValueProfileStore::new(max_vals)),
            dumped: AtomicBool::new(false),
        }
    }

    /// Gets or initializes the profile image, then returns a snapshot.
    pub fn snapshot(&self) -> Option<ProfileSnapshot> {
        let mut guard = self.image.lock();
        if guard.is_none() {
            // We hold the image lock, so no concurrent access.
            match crate::abi::collect::collect_profile_image() {
                Ok(image) => {
                    *guard = Some(image);
                }
                Err(_) => return None,
            }
        }
        guard.as_ref().map(|img| img.snapshot())
    }

    /// Gets mutable access to the profile image for merge/reset.
    pub fn image_mut(&self) -> spin::MutexGuard<'_, Option<ProfileImage>> {
        self.image.lock()
    }

    /// Records a value profiling observation.
    pub fn record_value(&self, site_index: usize, value: u64, count: u64) {
        let mut store = self.value.lock();
        store.record_value(site_index, value, count);
    }

    /// Resets all counters, bitmap, and value counts.
    pub fn reset(&self) {
        let mut guard = self.image.lock();
        if let Some(image) = guard.as_mut() {
            image.counters.reset();
            image.bitmap.clear();
            image.value_sites.clear_counts();
        }
        self.dumped.store(false, Ordering::Release);
        state::set_dumped(false);
    }
}

// === Module-level public functions ===

/// Takes a snapshot of the current profile image.
#[cfg(feature = "alloc")]
pub(crate) fn snapshot() -> Option<ProfileSnapshot> {
    RUNTIME.snapshot()
}

/// Gets mutable access to the profile image.
#[cfg(feature = "alloc")]
pub(crate) fn image_mut() -> spin::MutexGuard<'static, Option<ProfileImage>> {
    RUNTIME.image_mut()
}

/// Records a value profiling observation.
pub(crate) fn record_value(site_index: usize, value: u64, count: u64) {
    #[cfg(feature = "alloc")]
    RUNTIME.record_value(site_index, value, count);
}

/// Resets all profiling counters via the runtime.
#[cfg(feature = "alloc")]
pub(crate) fn reset_via_runtime() {
    RUNTIME.reset();
}

/// Checks if the runtime has been initialized and has an image.
#[cfg(feature = "alloc")]
pub(crate) fn has_image() -> bool {
    let guard = RUNTIME.image.lock();
    guard.is_some()
}
