// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related POSIX/Linux ABI types.

mod abi;
mod itimer;
mod tms;

pub use abi::TimeValueLike;
#[cfg(target_arch = "x86_64")]
pub use abi::utimbuf;
pub use itimer::ITimerType;
pub use tms::Tms;
