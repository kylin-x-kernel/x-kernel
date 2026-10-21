// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related syscall adapters.

mod clock_id;
mod itimer;
mod posix_timer;
mod queries;
mod sleep;
mod timerfd;

pub(crate) use self::{itimer::*, posix_timer::*, queries::*, sleep::*, timerfd::*};
