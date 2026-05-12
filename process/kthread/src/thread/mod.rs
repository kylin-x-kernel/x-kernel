// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod core;
mod current;
mod task_ext;

pub use core::{CurrentThread, Thread};

pub use current::{
    current_fs_context, current_process_fs_context, current_process_state, current_task_name,
    current_thread, with_current_thread,
};
pub use task_ext::AsThread;
