// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal APIs for processes and threads.
mod dequeue_observer;
mod process;
mod thread;

pub(crate) use dequeue_observer::notify_signal_dequeued;
pub use dequeue_observer::{
    SignalDequeueAction, register_signal_observer, unregister_signal_observer,
};
pub use process::*;
pub use thread::*;
