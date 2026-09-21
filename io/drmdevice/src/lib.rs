// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM (Direct Rendering Manager) device subsystem.
//!
//! Provides a minimal resource-backed DRM/KMS driver (`DrmCard`) bound to one
//! registered display scanout backend. Covers legacy libdrm and atomic-KMS
//! paths used by modern compositors.
//!
//! # Integration example
//!
//! devfs constructs one card from the exact display class handle assigned to a
//! `/dev/dri/cardN` node and retains a weak handle for removal notification:
//!
//! ```ignore
//! use alloc::sync::Arc;
//! use drmdevice::DrmCard;
//!
//! let display = kclass::display_devices()
//!     .into_iter()
//!     .next()
//!     .expect("display discovery must run before devfs registration");
//! let card = DrmCard::new(display);
//! let removal_card = Arc::downgrade(&card);
//! // Install `card` as the node's `DeviceFileOps` implementation.
//! if let Some(card) = removal_card.upgrade() {
//!     card.notify_removed();
//! }
//! ```

#![no_std]

extern crate alloc;
mod card0;
mod consts;
mod drm;

pub use card0::DrmCard;
