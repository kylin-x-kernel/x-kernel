// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interval-timer ABI types.

use strum::FromRepr;

/// The POSIX/Linux interval timer kind used by `getitimer(2)` / `setitimer(2)`.
#[repr(i32)]
#[derive(Eq, PartialEq, Debug, Clone, Copy, FromRepr)]
pub enum ITimerType {
    /// Wall-clock interval timer.
    Real    = 0,
    /// Per-thread/process user CPU time interval timer.
    Virtual = 1,
    /// User + kernel CPU time profiling timer.
    Prof    = 2,
}

unittest::path_mod_test!(tests, "../../ut-tests/time_itimer.rs");
