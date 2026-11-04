// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert_eq, def_test};

use crate::{
    IrqEvent, IrqHandler, IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, ResResult, Virq,
};

struct MockIrqProvider;

#[def_test]
fn unsupported_threaded_requests_do_not_invoke_callbacks() {
    static PROVIDER: MockIrqProvider = MockIrqProvider;
    let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let primary_calls = calls.clone();
    let thread_calls = calls.clone();
    let resource = IrqResource::plain(Virq::new(41), crate::IrqTrigger::LevelHigh);
    let result = crate::DevIrq::request_threaded_with(
        &PROVIDER,
        resource,
        Arc::new(move |_| {
            primary_calls.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            IrqEvent::WAKE_THREAD
        }),
        Arc::new(move |_| {
            thread_calls.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            IrqEvent::HANDLED
        }),
        None,
    );
    assert_eq!(result.unwrap_err(), crate::ResError::Unsupported);
    let thread_calls = calls.clone();
    let result = crate::DevIrq::request_threaded_default_with(
        &PROVIDER,
        resource,
        Arc::new(move |_| {
            thread_calls.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            IrqEvent::HANDLED
        }),
        None,
    );
    assert_eq!(result.unwrap_err(), crate::ResError::Unsupported);
    assert_eq!(calls.load(core::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&calls), 1);
}

#[def_test]
fn irq_events_preserve_claim_and_thread_intent() {
    let mut event = IrqEvent::NOT_HANDLED;
    event.merge(IrqEvent::HANDLED);
    event.merge(IrqEvent::WAKE_THREAD);
    assert_eq!(event.handled(), true);
    assert_eq!(event.wake_thread(), true);
    assert_eq!(IrqEvent::default(), IrqEvent::HANDLED);
}

impl IrqOp for MockIrqProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        Ok(IrqHandlerToken::shared_action(
            if handler.handle(irq.virq().raw()).handled() {
                7
            } else {
                8
            },
        ))
    }

    fn release_irq(&self, _irq: IrqResource, _token: IrqHandlerToken) {}

    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {}
}

#[def_test]
fn irq_mapping_retains_shared_vocabulary_and_origin() {
    let trigger: crate::IrqTrigger = irq_types::IrqTrigger::Unknown(0x80);
    let controller: crate::IrqController = irq_types::IrqController::Gic;
    let domain: crate::IrqDomainId = irq_types::IrqDomainId::new(73);
    let route = IrqRouteDesc {
        hwirq: 9,
        trigger,
        controller,
        domain: Some(domain),
    };
    let mapped = MockIrqProvider.map_irq(route);
    assert_eq!(
        mapped,
        Ok(IrqResource::DomainMapped {
            virq: Virq::new(9),
            trigger,
            controller,
            domain: Some(domain),
            hwirq: 9,
        })
    );
    assert_eq!(domain.as_u32(), 73);
}

#[def_test]
fn irq_provider_accepts_model_owned_handler_results() {
    let provider = MockIrqProvider;
    let irq = IrqResource::plain(Virq::new(41), crate::IrqTrigger::LevelHigh);
    for (action, token) in [(IrqEvent::HANDLED, 7), (IrqEvent::NOT_HANDLED, 8)] {
        let handler: Arc<dyn IrqHandler> = Arc::new(move |_| action);
        assert_eq!(
            provider.request_irq(irq, handler),
            Ok(IrqHandlerToken::shared_action(token))
        );
    }
}

#[def_test]
fn mapped_resource_keeps_hardware_and_virtual_irq_distinct() {
    let irq = IrqResource::mapped(
        Virq::new(41),
        crate::IrqTrigger::LevelHigh,
        crate::IrqController::Gic,
        Some(crate::IrqDomainId::new(73)),
        9,
    );
    assert_eq!(irq.virq(), Virq::new(41));
    assert_eq!(
        irq,
        IrqResource::DomainMapped {
            virq: Virq::new(41),
            trigger: crate::IrqTrigger::LevelHigh,
            controller: crate::IrqController::Gic,
            domain: Some(crate::IrqDomainId::new(73)),
            hwirq: 9,
        }
    );
}
