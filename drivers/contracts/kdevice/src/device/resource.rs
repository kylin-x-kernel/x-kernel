// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified resource descriptors for device discovery.
//!
//! The descriptor types themselves are OS-agnostic and live in [`device_res`].
//! This module re-exports them for the shared device model.

pub use device_res::{
    DmaSpec, IoPortRange, IrqResource, IrqTrigger, MmioRegion, ResourceDesc, ResourceSet,
};
