// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel IRQ source waiter notification.
//!
//! This crate bridges resource-level IRQ source reports to x-kernel task
//! wakers. It is intentionally separate from `device-res`: the resource model
//! stays OS-agnostic, while this crate owns the `PollSet`-based wait/wake
//! mechanism used by x-kernel drivers and subsystems.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::task::Waker;

use device_res::{IRQ_EVENT_SOURCES, IrqEventSource};
use kpoll::PollSet;
use kspin::SpinNoIrq;

struct IrqSourcePollSet {
    irq: usize,
    source: IrqEventSource,
    poll_set: PollSet,
}

static IRQ_SOURCE_WAITERS: SpinNoIrq<Vec<IrqSourcePollSet>> = SpinNoIrq::new(Vec::new());

/// Register a waker for a specific logical source on an IRQ line.
pub fn register_source_waker(irq: usize, source: IrqEventSource, waker: &Waker) {
    let mut waiters = IRQ_SOURCE_WAITERS.lock();
    if let Some(waiter) = waiters
        .iter()
        .find(|waiter| waiter.irq == irq && waiter.source == source)
    {
        waiter.poll_set.register(waker);
    } else {
        let poll_set = PollSet::new();
        poll_set.register(waker);
        waiters.push(IrqSourcePollSet {
            irq,
            source,
            poll_set,
        });
    }
}

/// Wake waiters whose logical IRQ source bits fired.
pub fn dispatch_sources(irq: usize, sources: u8) {
    let waiters = IRQ_SOURCE_WAITERS.lock();
    for waiter in waiters.iter() {
        if waiter.irq == irq
            && waiter.source < IRQ_EVENT_SOURCES as u8
            && sources & (1 << waiter.source) != 0
        {
            waiter.poll_set.wake();
        }
    }
}
