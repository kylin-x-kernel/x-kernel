// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX-facing state owned by a process runtime.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, Ordering};

use ksync::RwLock;

/// Executable metadata that must be observed as one snapshot.
#[derive(Clone)]
pub(super) struct ExecMetadata {
    exe_path: String,
    cmdline: Arc<Vec<String>>,
    /// Opened backing executable file, followed by `/proc/<pid>/exe`.
    exe_file: Option<Arc<kvfs::VfsFile>>,
}

impl ExecMetadata {
    /// Creates a new executable metadata snapshot.
    pub(super) fn new(
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        exe_file: Option<Arc<kvfs::VfsFile>>,
    ) -> Self {
        Self {
            exe_path,
            cmdline,
            exe_file,
        }
    }

    /// Returns the executable path.
    pub(super) fn exe_path(&self) -> &str {
        &self.exe_path
    }

    /// Returns the command-line arguments.
    pub(super) fn cmdline(&self) -> &Arc<Vec<String>> {
        &self.cmdline
    }

    /// Returns the opened backing executable file, if any.
    pub(super) fn exe_file(&self) -> Option<Arc<kvfs::VfsFile>> {
        self.exe_file.clone()
    }
}

/// POSIX-facing state shared by all threads in a process.
pub(super) struct ProcessPosixState {
    exec_metadata: RwLock<ExecMetadata>,
    oom_score_adj: AtomicI32,
}

impl ProcessPosixState {
    /// Creates a new [`ProcessPosixState`].
    pub(super) fn new(exe_path: String, cmdline: Arc<Vec<String>>) -> Self {
        Self {
            exec_metadata: RwLock::new(ExecMetadata::new(exe_path, cmdline, None)),
            oom_score_adj: AtomicI32::new(0),
        }
    }

    /// Returns the executable metadata snapshot.
    pub(super) fn exec_metadata(&self) -> ExecMetadata {
        self.exec_metadata.read().clone()
    }

    /// Updates executable metadata after a successful exec.
    pub(super) fn set_exec_metadata(
        &self,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        exe_file: Option<Arc<kvfs::VfsFile>>,
    ) {
        // Replace first and drop the old snapshot outside the write lock: the
        // replaced `ExecMetadata` owns an `Arc<VfsFile>` whose release path can
        // take inode locks, and dropping it while the metadata lock is held
        // would invert the lock order against readers of `/proc/<pid>/exe`.
        let replaced = core::mem::replace(
            &mut *self.exec_metadata.write(),
            ExecMetadata::new(exe_path, cmdline, exe_file),
        );
        drop(replaced);
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
