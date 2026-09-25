// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared normal-service notification without an owning reference cycle.

use alloc::{
    sync::{Arc, Weak},
    task::Wake,
};
use core::task::Waker;

use super::NormalService;

struct NormalWake<S>(Weak<NormalService<S>>);

pub(super) fn prepare<S: Send + Sync + 'static>(owner: Weak<NormalService<S>>) -> Arc<Waker> {
    Arc::new(Waker::from(Arc::new(NormalWake(owner))))
}

impl<S: Send + Sync + 'static> Wake for NormalWake<S> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(service) = self.0.upgrade() {
            service.kick();
        }
    }
}
