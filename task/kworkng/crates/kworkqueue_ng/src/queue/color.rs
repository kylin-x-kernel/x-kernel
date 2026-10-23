// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable receipt for one sealed color occurrence, not a reusable counter.

use alloc::{sync::Arc, vec::Vec};

use ktask::completion::CompletionWait;

use crate::work::observers::{CompletionObserver, PreparedObserver};

/// Independent completion sources; no Queue, Work or reusable slot back-reference.
pub(super) struct ColorCompletion {
    locals: Vec<Arc<CompletionObserver>>,
}

impl ColorCompletion {
    /// Prepares one fresh observation per partition before publishing a cutoff.
    pub(super) fn prepare(
        partitions: usize,
        progress: &Arc<kcompletion::WaitQueue>,
    ) -> (Arc<Self>, Vec<PreparedObserver>) {
        let mut locals = Vec::with_capacity(partitions);
        let mut tokens = Vec::with_capacity(partitions);
        for _ in 0..partitions {
            let (receipt, token) =
                CompletionObserver::prepare_with_progress(Some(progress.clone()));
            locals.push(receipt);
            tokens.push(token);
        }
        (Arc::new(Self { locals }), tokens)
    }

    pub(super) fn is_completed(&self) -> bool {
        self.locals.iter().all(|local| local.is_completed())
    }

    pub(super) fn wait(&self, task_wait: &CompletionWait) {
        for local in &self.locals {
            local.wait(task_wait);
        }
    }
}
