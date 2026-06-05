// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process runtime and init-process orchestration.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod init_process;
mod runtime;
pub use init_process::run_init_process;
pub use runtime::{check_signals, do_exit, new_user_task, raise_signal_fatal};
