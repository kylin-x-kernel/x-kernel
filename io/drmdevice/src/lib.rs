// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM (Direct Rendering Manager) device subsystem.
//!
//! Provides a minimal resource-backed DRM/KMS driver (`Card0`) over the
//! registered display scanout backend. Covers legacy libdrm and atomic-KMS
//! paths used by modern compositors.

#![no_std]

extern crate alloc;
mod card0;
mod consts;
mod drm;

pub use card0::Card0;

/// Returns whether a display device is available for DRM to use.
pub fn available() -> bool {
    card0::scanout_available()
}
