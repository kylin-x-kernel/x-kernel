// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sealed raw ownership conversions; pointers never encode tag bits.

use alloc::{boxed::Box, sync::Arc};

/// Supported ownership carriers: Box and Arc of a sized, shareable value.
///
/// This sealed trait describes representation, not destructor safety. Inserting
/// a value separately requires certification of its complete release chain.
pub trait EntryOwner: private::RawOwner + Send + Sync {
    /// Value borrowed by an indexed lookup.
    type Target: Send + Sync;
}

pub(crate) mod private {
    pub trait RawOwner {
        fn into_raw(self) -> *mut ();

        /// # Safety
        /// `raw` must retain exactly one owner produced by this implementation;
        /// all borrows protected by that owner must have ended before release.
        unsafe fn drop_raw(raw: *mut ());
    }
}

impl<T: Send + Sync> EntryOwner for Box<T> {
    type Target = T;
}

impl<T> private::RawOwner for Box<T> {
    fn into_raw(self) -> *mut () {
        Box::into_raw(self).cast()
    }

    unsafe fn drop_raw(raw: *mut ()) {
        // SAFETY: the caller returns this implementation's unique raw Box only
        // after its borrowers have ended, preserving allocation provenance.
        drop(unsafe { Box::from_raw(raw.cast::<T>()) });
    }
}

impl<T: Send + Sync> EntryOwner for Arc<T> {
    type Target = T;
}

impl<T> private::RawOwner for Arc<T> {
    fn into_raw(self) -> *mut () {
        Arc::into_raw(self).cast_mut().cast()
    }

    unsafe fn drop_raw(raw: *mut ()) {
        // SAFETY: the caller returns exactly the strong reference transferred
        // by into_raw, after all borrows protected by that reference have ended.
        drop(unsafe { Arc::from_raw(raw.cast::<T>()) });
    }
}
