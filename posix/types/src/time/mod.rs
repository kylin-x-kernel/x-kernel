// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related POSIX/Linux ABI types.

mod abi;
mod clock_ticks;
mod itimer;
mod tms;

#[cfg(target_arch = "x86_64")]
pub use abi::utimbuf;
pub use abi::{SystemTimeLike, TimeSpanLike, try_into_realtime_deadline};
pub use clock_ticks::{PosixClockTicks, USER_HZ};
pub use itimer::ITimerType;
pub use tms::Tms;
