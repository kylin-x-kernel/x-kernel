// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate_interface::{call_interface, def_interface};

#[def_interface]
pub trait RtcIf {
    /// Returns the wall-clock offset in nanoseconds relative to monotonic time.
    fn offset_ns() -> u64;
}

#[inline]
pub fn offset_ns() -> u64 {
    call_interface!(RtcIf::offset_ns)
}
