// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interval-timer ABI types.

use strum::FromRepr;

/// The POSIX/Linux interval timer kind used by `getitimer(2)` / `setitimer(2)`.
#[repr(i32)]
#[allow(non_camel_case_types)]
#[derive(Eq, PartialEq, Debug, Clone, Copy, FromRepr)]
pub enum ITimerType {
    /// Wall-clock interval timer.
    Real    = 0,
    /// Per-thread/process user CPU time interval timer.
    Virtual = 1,
    /// User + kernel CPU time profiling timer.
    Prof    = 2,
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::ITimerType;

    #[def_test]
    fn test_itimer_type_from_repr() {
        assert_eq!(ITimerType::from_repr(0), Some(ITimerType::Real));
        assert_eq!(ITimerType::from_repr(1), Some(ITimerType::Virtual));
        assert_eq!(ITimerType::from_repr(2), Some(ITimerType::Prof));
        assert_eq!(ITimerType::from_repr(3), None);
    }
}
