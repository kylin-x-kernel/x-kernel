// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process runtime and init-process orchestration.
//!
//! Boot code starts with [`spawn_init_process`]; clone adapters construct tasks
//! with [`new_user_task`] and publish them through `kprocess`. The trap loop
//! dispatches syscalls and faults, checks signals and orders last-thread cleanup.
//! [`spawn_init_process`] documents the complete image/runtime/publication
//! sequence; its source shows the required provider calls in order.
//!
//! ```no_run
//! # fn publish_prepared(
//! #     entry: ktask::PreparedTaskEntry,
//! #     prepared: kprocess::PreparedUserClone,
//! # ) -> kerrno::KResult<ktask::KtaskRef> {
//! let root = prepared.page_table_root();
//! let (thread, identity) = prepared.into_parts();
//! let mut task = posix_process::new_user_task("child", entry, identity, thread);
//! task.ctx_mut().set_page_table_root(root);
//! // Complete parent-side setup before the published child becomes runnable.
//! kprocess::publish_user_task(task)?.commit(|_| Ok(()))
//! # }
//! ```
//!
//! Prepare the entry with `prepare_user_entry` before creating the clone result.
//! The caller must first prepare the matching image and clone result in an
//! initialized kernel task context; this example only compiles in that target.

#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy posix-process has 11 \
              diagnostic sites, too many to judge individually in this migration; new code \
              remains reviewed"
)]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod init_process;
mod runtime;
pub use init_process::spawn_init_process;
pub use runtime::{
    check_signals, do_exit, do_exit_worker, exec_de_thread, new_user_task, prepare_user_entry,
    raise_current_thread_fault, raise_signal_fatal,
};
#[cfg(unittest)]
pub use runtime::{
    replay_user_return_restart_for_test, replay_user_return_restart_with_gap_for_test,
};
