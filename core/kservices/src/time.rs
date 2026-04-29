// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time interrupt accounting.
//!
//! [`TimeValueLike`] has been moved to the `posix-types` crate.

use core::sync::atomic::{AtomicUsize, Ordering};

static IRQ_CNT: AtomicUsize = AtomicUsize::new(0);

/// Increment the interrupt count.
pub fn inc_irq_cnt() {
    IRQ_CNT.fetch_add(1, Ordering::Relaxed);
}

/// Get the current interrupt count.
pub fn irq_cnt() -> usize {
    IRQ_CNT.load(Ordering::Relaxed)
}
