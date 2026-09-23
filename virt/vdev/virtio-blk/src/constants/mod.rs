// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO Block Device Constants
//!
//! This module contains block device specific constants and re-exports
//! common VirtIO constants from axvirtio-common.

pub mod block;

// Re-export common VirtIO constants from axvirtio-common
// Re-export block device specific constants
pub use block::*;
pub use vdev_virtio::constants::*;
