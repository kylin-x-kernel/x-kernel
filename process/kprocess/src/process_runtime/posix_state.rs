// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX-facing state owned by a process runtime.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use ksync::RwLock;

/// POSIX-facing state shared by all threads in a process.
pub(super) struct ProcessPosixState {
    exe_path: RwLock<String>,
    cmdline: RwLock<Arc<Vec<String>>>,
    umask: AtomicU32,
}

impl ProcessPosixState {
    /// Creates a new [`ProcessPosixState`].
    pub(super) fn new(exe_path: String, cmdline: Arc<Vec<String>>) -> Self {
        Self {
            exe_path: RwLock::new(exe_path),
            cmdline: RwLock::new(cmdline),
            umask: AtomicU32::new(0o022),
        }
    }

    /// Returns the executable path.
    pub(super) fn exe_path(&self) -> &RwLock<String> {
        &self.exe_path
    }

    /// Returns the command-line arguments.
    pub(super) fn cmdline(&self) -> &RwLock<Arc<Vec<String>>> {
        &self.cmdline
    }

    /// Returns the process umask.
    pub(super) fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Sets the process umask.
    pub(super) fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::SeqCst);
    }

    /// Sets the process umask and returns the old value.
    pub(super) fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
    }
}
