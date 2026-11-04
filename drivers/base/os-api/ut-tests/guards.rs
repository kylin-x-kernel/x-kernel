// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{DevIrq, Virq};

struct GuardProvider {
    releases: AtomicUsize,
    fail_request: bool,
}

fn guard_resource() -> crate::IrqResource {
    crate::IrqResource::mapped(
        Virq::new(903),
        crate::IrqTrigger::LevelHigh,
        crate::IrqController::Gic,
        Some(crate::IrqDomainId::new(0x7319)),
        37,
    )
}

impl crate::IrqOp for GuardProvider {
    fn request_irq(
        &self,
        resource: crate::IrqResource,
        handler: Arc<dyn crate::IrqHandler>,
    ) -> crate::ResResult<crate::IrqHandlerToken> {
        core::assert_eq!(resource, guard_resource());
        if self.fail_request {
            return Err(crate::ResError::Busy);
        }
        core::assert!(handler.handle(resource.virq().raw()).handled());
        Ok(crate::IrqHandlerToken::SharedAction(79))
    }

    fn release_irq(&self, resource: crate::IrqResource, token: crate::IrqHandlerToken) {
        core::assert_eq!(resource, guard_resource());
        core::assert_eq!(token, crate::IrqHandlerToken::SharedAction(79));
        self.releases.fetch_add(1, Ordering::Relaxed);
    }

    fn set_irq_enabled(&self, _: crate::IrqResource, _: bool) {}
}

#[def_test]
fn irq_guard_preserves_exact_registration_until_release() {
    static PROVIDER: GuardProvider = GuardProvider {
        releases: AtomicUsize::new(0),
        fail_request: false,
    };
    let initial = PROVIDER.releases.load(Ordering::Relaxed);
    let guard = DevIrq::request_with(
        &PROVIDER,
        guard_resource(),
        Arc::new(|virq| {
            core::assert_eq!(virq, 903);
            crate::IrqEvent::HANDLED
        }),
    )
    .unwrap();
    assert_eq!(guard.virq(), Virq::new(903));
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial);
    drop(guard);
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), initial + 1);
}

#[def_test]
fn rejected_irq_request_has_no_release_obligation_or_retained_callback() {
    static PROVIDER: GuardProvider = GuardProvider {
        releases: AtomicUsize::new(0),
        fail_request: true,
    };
    let handler: Arc<dyn crate::IrqHandler> = Arc::new(|_| crate::IrqEvent::HANDLED);
    let result = DevIrq::request_with(&PROVIDER, guard_resource(), handler.clone());
    assert_eq!(result.unwrap_err(), crate::ResError::Busy);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(PROVIDER.releases.load(Ordering::Relaxed), 0);
}
