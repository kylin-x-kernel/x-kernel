// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{
    IrqEvent, IrqHandler, IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, ResError, ResResult,
    Virq,
};

struct MockIrqProvider;

struct MockMsiProvider {
    is_busy: AtomicBool,
    releases: AtomicUsize,
}

impl IrqOp for MockMsiProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        MockIrqProvider.request_irq(irq, handler)
    }

    fn release_irq(&self, _irq: IrqResource, _token: IrqHandlerToken) {}

    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {}

    fn try_free_msix(
        &self,
        resource: crate::MsiResource,
    ) -> Result<(), (crate::ResError, crate::MsiResource)> {
        // Private sequential fixture: these atomics satisfy the provider's
        // Sync bound, not a cross-thread publication protocol.
        if self.is_busy.load(Ordering::Relaxed) {
            return Err((crate::ResError::Busy, resource));
        }
        self.releases.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn msi_resource() -> crate::MsiResource {
    crate::MsiResource::new(
        IrqResource::plain(Virq::new(41), crate::IrqTrigger::EdgeRising),
        crate::MsiMessage::new(0xfee0_1000, 0x52),
    )
}

#[def_test]
fn default_msi_release_returns_original_resource() {
    let (error, resource) = MockIrqProvider.try_free_msix(msi_resource()).unwrap_err();
    assert_eq!(error, crate::ResError::Unsupported);
    assert_eq!(resource, msi_resource());
    assert_eq!(
        MockIrqProvider.try_free_msix(resource),
        Err((crate::ResError::Unsupported, msi_resource()))
    );
}

#[def_test]
fn rejected_msi_release_retains_ownership_for_explicit_retry() {
    let provider = MockMsiProvider {
        is_busy: AtomicBool::new(true),
        releases: AtomicUsize::new(0),
    };
    let (error, resource) = provider.try_free_msix(msi_resource()).unwrap_err();
    assert_eq!(error, crate::ResError::Busy);
    assert_eq!(resource, msi_resource());
    assert_eq!(provider.releases.load(Ordering::Relaxed), 0);
    provider.is_busy.store(false, Ordering::Relaxed);
    assert_eq!(provider.try_free_msix(resource), Ok(()));
    assert_eq!(provider.releases.load(Ordering::Relaxed), 1);
}

#[def_test]
fn terminal_msi_cleanup_delegates_to_fallible_release() {
    let provider = MockMsiProvider {
        is_busy: AtomicBool::new(false),
        releases: AtomicUsize::new(0),
    };
    provider.free_msix(msi_resource());
    assert_eq!(provider.releases.load(Ordering::Relaxed), 1);
}

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

struct EmptyBackend;

impl fwnode::FwNodeOps<'static> for EmptyBackend {
    fn source(&self) -> fwnode::FwSource {
        fwnode::FwSource::Static
    }

    fn name(&self, _: fwnode::FwNodeId) -> Option<&'static str> {
        None
    }

    fn parent(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn first_child(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn next_sibling(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn is_available(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(true)
    }

    fn is_interrupt_controller(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(false)
    }

    fn firmware_id(&self, _: fwnode::FwNodeId, _: usize) -> Option<&'static str> {
        None
    }

    fn property(
        &self,
        _: fwnode::FwNodeId,
        _: &str,
    ) -> Result<Option<fwnode::FwProperty<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn mmio_resource(
        &self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwMmioResource<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn irq_spec(
        &'static self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwIrqSpec<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn reference(
        &'static self,
        _: fwnode::FwNodeId,
        _: &str,
        _: usize,
        _: usize,
    ) -> Result<Option<fwnode::FwReference<'static>>, fwnode::FwError> {
        Ok(None)
    }
}

static EMPTY_BACKEND: EmptyBackend = EmptyBackend;

#[def_test]
fn providers_without_firmware_identity_reject_firmware_mapping() {
    let spec = fwnode::FwIrqSpec {
        controller: fwnode::FwNode::new(&EMPTY_BACKEND, fwnode::FwNodeId::new(1)),
        params: fwnode::FwParams::new(&[1]).unwrap(),
    };
    // The default must not pretend the raw binding cell is a logical IRQ.
    assert_eq!(
        MockIrqProvider.map_fw_irq(spec),
        Err(ResError::Unsupported)
    );
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
