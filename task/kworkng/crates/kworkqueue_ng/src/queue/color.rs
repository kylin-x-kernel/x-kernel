// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable receipt for one sealed color occurrence, not a reusable counter.

use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use kpoll::{PollRegisterError, PollRegistration};

use crate::work::observers::{CompletionObserver, PreparedObserver};

/// Independent completion sources; no Queue, Work or reusable slot back-reference.
pub(super) struct ColorCompletion {
    locals: Vec<Arc<CompletionObserver>>,
}

impl ColorCompletion {
    /// Prepares one fresh observation per partition before publishing a cutoff.
    pub(super) fn prepare(partitions: usize) -> (Arc<Self>, Vec<PreparedObserver>) {
        let mut locals = Vec::with_capacity(partitions);
        let mut tokens = Vec::with_capacity(partitions);
        for _ in 0..partitions {
            let (receipt, token) = CompletionObserver::prepare();
            locals.push(receipt);
            tokens.push(token);
        }
        (Arc::new(Self { locals }), tokens)
    }

    pub(super) fn is_completed(&self) -> bool {
        self.locals.iter().all(|local| local.is_completed())
    }

    /// Registration and rollback happen outside every management/control lock.
    /// Each caller retains its own registrations until its wait ends.
    pub(super) fn register_with(
        &self,
        waker: &Waker,
        register: &mut impl FnMut(
            &CompletionObserver,
            &Waker,
        ) -> Result<PollRegistration, PollRegisterError>,
    ) -> Result<Vec<PollRegistration>, PollRegisterError> {
        self.locals
            .iter()
            .map(|local| register(local, waker))
            .collect()
    }
}
