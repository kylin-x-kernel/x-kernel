// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related syscall adapters.

mod itimer;
mod posix_timer;
mod queries;
mod sleep;
mod timerfd;

pub use self::{itimer::*, posix_timer::*, queries::*, sleep::*, timerfd::*};
