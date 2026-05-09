// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX-facing process-shared state.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, Ordering};

use ksignal::Signo;
use ksync::RwLock;

/// POSIX-facing state shared by all threads in a process.
pub struct ProcessPosixState {
    exe_path: RwLock<String>,
    cmdline: RwLock<Arc<Vec<String>>>,
    exit_signal: Option<Signo>,
    umask: AtomicU32,
}

impl ProcessPosixState {
    /// Creates a new [`ProcessPosixState`].
    pub fn new(exe_path: String, cmdline: Arc<Vec<String>>, exit_signal: Option<Signo>) -> Self {
        Self {
            exe_path: RwLock::new(exe_path),
            cmdline: RwLock::new(cmdline),
            exit_signal,
            umask: AtomicU32::new(0o022),
        }
    }

    /// Returns the executable path.
    pub fn exe_path(&self) -> &RwLock<String> {
        &self.exe_path
    }

    /// Returns the command-line arguments.
    pub fn cmdline(&self) -> &RwLock<Arc<Vec<String>>> {
        &self.cmdline
    }

    /// Returns the process exit signal.
    pub fn exit_signal(&self) -> Option<Signo> {
        self.exit_signal
    }

    /// Returns the process umask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Sets the process umask.
    pub fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::SeqCst);
    }

    /// Sets the process umask and returns the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
    }
}
