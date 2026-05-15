// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File system related syscalls.
//!
//! This module keeps fd-backed syscalls whose primary ABI semantics are not
//! filesystem operations, while re-exporting filesystem ABI implementations
//! from `posix_fs`.

mod event;
mod pidfd;
mod timerfd;

pub use posix_fs::*;

pub use self::{event::*, pidfd::*, timerfd::*};
