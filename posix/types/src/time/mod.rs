// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related POSIX/Linux ABI types.

mod abi;
mod itimer;

pub use abi::TimeValueLike;
pub use itimer::ITimerType;
