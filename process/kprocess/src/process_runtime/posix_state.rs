// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX-facing state owned by a process runtime.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use ksync::RwLock;

/// Executable metadata that must be observed as one snapshot.
#[derive(Clone)]
pub(super) struct ExecMetadata {
    exe_path: String,
    cmdline: Arc<Vec<String>>,
}

impl ExecMetadata {
    /// Creates a new executable metadata snapshot.
    pub(super) fn new(exe_path: String, cmdline: Arc<Vec<String>>) -> Self {
        Self { exe_path, cmdline }
    }

    /// Returns the executable path.
    pub(super) fn exe_path(&self) -> &str {
        &self.exe_path
    }

    /// Returns the command-line arguments.
    pub(super) fn cmdline(&self) -> &Arc<Vec<String>> {
        &self.cmdline
    }
}

/// POSIX-facing state shared by all threads in a process.
pub(super) struct ProcessPosixState {
    exec_metadata: RwLock<ExecMetadata>,
    umask: AtomicU32,
    oom_score_adj: AtomicI32,
}

impl ProcessPosixState {
    /// Creates a new [`ProcessPosixState`].
    pub(super) fn new(exe_path: String, cmdline: Arc<Vec<String>>) -> Self {
        Self {
            exec_metadata: RwLock::new(ExecMetadata::new(exe_path, cmdline)),
            umask: AtomicU32::new(0o022),
            oom_score_adj: AtomicI32::new(0),
        }
    }

    /// Returns the executable metadata snapshot.
    pub(super) fn exec_metadata(&self) -> ExecMetadata {
        self.exec_metadata.read().clone()
    }

    /// Updates executable metadata after a successful exec.
    pub(super) fn set_exec_metadata(&self, exe_path: String, cmdline: Arc<Vec<String>>) {
        *self.exec_metadata.write() = ExecMetadata::new(exe_path, cmdline);
    }

    /// Returns the process umask.
    pub(super) fn umask(&self) -> u32 {
        // `umask` is an independent process attribute and does not publish or
        // order any other state.
        self.umask.load(Ordering::Relaxed)
    }

    /// Sets the process umask.
    pub(super) fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::Relaxed);
    }

    /// Sets the process umask and returns the old value.
    pub(super) fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::Relaxed)
    }

    /// Returns the process OOM score adjustment.
    pub(super) fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj.load(Ordering::Relaxed)
    }

    /// Sets the process OOM score adjustment.
    pub(super) fn set_oom_score_adj(&self, value: i32) {
        self.oom_score_adj.store(value, Ordering::Relaxed);
    }
}
