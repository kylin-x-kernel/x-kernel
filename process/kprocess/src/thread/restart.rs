// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-thread restart-block slot for `ERESTART_RESTARTBLOCK`.
//!
//! Linux stores one continuation on `task_struct`. This crate owns only the
//! slot lifetime: it starts empty, clone does not copy it, and `exec` clears
//! it. The syscall adapter that arms the slot owns the payload type.
//! [`CurrentThread`] is the only handle that can arm or take that payload, so
//! a sibling [`Thread`] reference cannot read another thread's timed wait.

use core::{
    any::TypeId,
    mem::{align_of, size_of},
};

use super::{CurrentThread, Thread};

/// Fixed storage for one syscall-adapter payload.
///
/// `bytes` is first so a 16-aligned `RestartSlot` keeps the payload 16-aligned.
/// The tag is a [`TypeId`]; this crate never interprets the bytes.
#[repr(C, align(16))]
pub(super) struct RestartSlot {
    bytes: [u8; RestartSlot::CAPACITY],
    tag: Option<TypeId>,
}

impl RestartSlot {
    const ALIGN: usize = 16;
    pub(super) const CAPACITY: usize = 64;

    pub(super) const fn empty() -> Self {
        Self {
            bytes: [0; Self::CAPACITY],
            tag: None,
        }
    }

    fn store<T: Copy + 'static>(&mut self, payload: T) {
        const {
            assert!(size_of::<T>() <= RestartSlot::CAPACITY);
            assert!(align_of::<T>() <= RestartSlot::ALIGN);
        }
        self.tag = Some(TypeId::of::<T>());
        self.bytes = [0; Self::CAPACITY];
        // SAFETY: `bytes` is the first field of an align(16) slot, so its
        // address is 16-aligned. The asserts above require `T` to fit in that
        // storage and to need no stricter alignment. `T: Copy` makes a bitwise
        // write a valid `T`, and this method has exclusive access to `self`.
        unsafe {
            self.bytes.as_mut_ptr().cast::<T>().write(payload);
        }
    }

    fn read<T: Copy + 'static>(&mut self) -> Option<T> {
        if self.tag != Some(TypeId::of::<T>()) {
            return None;
        }
        self.tag = None;
        // SAFETY: `store` wrote a `T` under this `TypeId` into `bytes`, which
        // is still 16-aligned and exclusively borrowed. `T: Copy`, so the read
        // does not invalidate another owner. Clearing `tag` first makes a
        // second take observe an empty slot even if this read were to panic.
        Some(unsafe { self.bytes.as_ptr().cast::<T>().read() })
    }
}

impl Thread {
    pub(super) fn clear_restart_block(&self) {
        *self.restart_block.lock() = RestartSlot::empty();
    }
}

impl CurrentThread {
    /// Arms `payload` as this thread's restart-block, replacing any previous one.
    ///
    /// `kprocess` does not interpret `payload`. A sibling thread handle cannot
    /// call this. `T` must fit the fixed slot (64 bytes, 16-byte alignment);
    /// a larger type fails to compile.
    pub fn arm_restart_block<T: Copy + Send + 'static>(&self, payload: T) {
        self.restart_block.lock().store(payload);
    }

    /// Removes the restart-block when it was armed as `T`.
    ///
    /// An empty slot, or a payload of another type, returns `None` and leaves
    /// a different payload in place. `restart_syscall` tries each adapter type
    /// and then clears anything it does not recognize.
    pub fn take_restart_block<T: Copy + Send + 'static>(&self) -> Option<T> {
        self.restart_block.lock().read()
    }

    /// Drops the restart-block without interpreting it.
    ///
    /// The user-return path calls this when a handler turns
    /// `ERESTART_RESTARTBLOCK` into `EINTR`, so a later `restart_syscall`
    /// cannot resume a wait userspace already observed as interrupted.
    /// `exec` clears the same slot because the payload addresses the previous
    /// image.
    pub fn clear_restart_block(&self) {
        Thread::clear_restart_block(self);
    }
}
