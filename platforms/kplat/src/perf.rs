// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform performance monitoring interface.

use kplat_macros::device_interface;

/// Performance event callback type.
pub type PerfCb = fn();

#[device_interface]
pub trait PerfMgr {
    /// Handles a performance counter overflow.
    fn on_overflow() -> bool;
    /// Registers a callback for a counter index.
    fn reg_cb(idx: u32, cb: PerfCb) -> bool;
}
