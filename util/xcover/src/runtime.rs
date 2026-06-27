// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime state management.
//!
//! Holds the live `ProfileImage` inside a [`crate::sync::Once`], which
//! serializes initialization via CAS. After initialization, all access is
//! through `&ProfileImage`; mutation happens via atomic operations on the
//! image's fields.
//!
//! Thread-safety does not rely on a `Mutex`: the only mutable state lives
//! inside atomic fields (`AtomicCounterStore`, `AtomicBitmapStore`,
//! `AtomicValueProfileStore`). Multiple threads may call any of these
//! functions concurrently without data races.

#[cfg(feature = "alloc")]
use crate::image::ProfileImage;
use crate::sync::Once;

/// Global profile image. Initialized on first access via
/// [`crate::abi::collect::collect_profile_image`].
#[cfg(feature = "alloc")]
static RUNTIME_IMAGE: Once<ProfileImage> = Once::new();

/// Returns `Ok(&ProfileImage)` on success, `Err(ProfileError)` if collection
/// failed. On the fast path (already initialized), this is a single atomic
/// load.
///
/// Concurrent first-callers race via [`Once`]'s CAS — only one actually
/// initializes; others spin-wait on the result.
#[cfg(feature = "alloc")]
pub(crate) fn image_or_init() -> Result<&'static ProfileImage, crate::ProfileError> {
    RUNTIME_IMAGE.get_or_try_init(crate::abi::collect::collect_profile_image)
}

/// Returns `true` iff the profile image has been initialized.
///
/// Single atomic load, no allocation, no contention.
#[cfg(feature = "alloc")]
pub(crate) fn has_image() -> bool {
    RUNTIME_IMAGE.get().is_some()
}

/// Captures a snapshot of the current profile image for serialization.
///
/// Returns `None` if the image has not been initialized or initialization
/// fails on this call.
#[cfg(feature = "alloc")]
pub(crate) fn snapshot() -> Option<crate::image::ProfileSnapshot> {
    image_or_init().ok().map(|img| img.snapshot())
}

/// Records a value-profiling observation at the given flat site index.
///
/// Lock-free on the hot path: scans the site's existing entries with
/// atomic loads; on miss, CAS-claims a new slot. Drops the new value if
/// the site is full (see [`crate::image::AtomicValueSite`] for rationale).
///
/// If the image has not been initialized yet, this is a no-op.
#[cfg(feature = "alloc")]
pub(crate) fn record_value(site_index: usize, value: u64, count: u64) {
    if let Some(img) = RUNTIME_IMAGE.get() {
        img.value_sites.record_value(site_index, value, count);
    }
}

/// Resets counters, bitmap, and value-site counts via atomic stores.
///
/// No-op if the image has not been initialized.
#[cfg(feature = "alloc")]
pub(crate) fn reset() {
    if let Some(img) = RUNTIME_IMAGE.get() {
        img.reset();
    }
    crate::state::set_dumped(false);
}
