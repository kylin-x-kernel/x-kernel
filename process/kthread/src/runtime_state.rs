// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime state shared by all threads in a process.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kfs::FsContext;
use ksync::Mutex;
use memspace::AddrSpace;

/// Process runtime state shared by all threads in a process.
pub struct ProcessRuntimeState {
    address_space: Arc<Mutex<AddrSpace>>,
    fs_context: Arc<Mutex<FsContext>>,
    heap_top: AtomicUsize,
}

impl ProcessRuntimeState {
    /// Creates a new [`ProcessRuntimeState`].
    pub fn new(
        address_space: Arc<Mutex<AddrSpace>>,
        fs_context: Arc<Mutex<FsContext>>,
        user_heap_base: usize,
    ) -> Self {
        Self {
            address_space,
            fs_context,
            heap_top: AtomicUsize::new(user_heap_base),
        }
    }

    /// Returns the virtual address space.
    pub fn address_space(&self) -> &Arc<Mutex<AddrSpace>> {
        &self.address_space
    }

    /// Returns the process-owned filesystem context.
    pub fn fs_context(&self) -> &Arc<Mutex<FsContext>> {
        &self.fs_context
    }

    /// Returns the top address of the user heap.
    pub fn heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Sets the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }
}
