// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process Management

#![no_std]
#![warn(missing_docs)]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod process;
mod process_group;
mod session;

/// A process ID, also used as session ID, process group ID, and thread ID.
pub type Pid = u32;

pub use process::{Process, init_proc};
pub use process_group::ProcessGroup;
pub use session::Session;
