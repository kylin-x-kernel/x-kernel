// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private MSI-X ownership tests; configuration accesses use RAM, not hardware.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use driver_os_api::{
    IrqHandler, IrqHandlerToken, IrqResource, IrqTrigger, MsiMessage, ResError, Virq,
};
use unittest::{assert, assert_eq, def_test};

use super::*;

struct ReleaseProvider {
    can_release: AtomicBool,
    attempts: AtomicUsize,
    released: AtomicUsize,
    last_irq: AtomicUsize,
}

impl IrqOp for ReleaseProvider {
    fn request_irq(
        &self,
        _irq: IrqResource,
        _handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        Err(ResError::Unsupported)
    }

    fn release_irq(&self, _irq: IrqResource, _token: IrqHandlerToken) {}

    fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {}

    fn try_free_msix(&self, resource: MsiResource) -> Result<(), (ResError, MsiResource)> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.last_irq
            .store(resource.irq.virq().raw(), Ordering::Relaxed);
        if !self.can_release.load(Ordering::Relaxed) {
            return Err((ResError::Busy, resource));
        }
        self.released.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: portable MSI-X fixture requires static provider \
              and aligned MMIO backing retained through copied accessors; kernel allocator \
              dependencies are forbidden"
)]
fn fixture() -> (
    PciIrqState,
    &'static ReleaseProvider,
    PciConfigAccess,
    DeviceFunction,
) {
    let provider = Box::leak(Box::new(ReleaseProvider {
        can_release: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        released: AtomicUsize::new(0),
        last_irq: AtomicUsize::new(0),
    }));
    let window = Box::leak(Box::new([0u32; 64]));
    // SAFETY: aligned program-lifetime RAM covers BDF 0 and offsets 0..255.
    // Only this fixture's sequential volatile accesses use the window.
    let mut config = unsafe { PciConfigAccess::new(window.as_mut_ptr().cast(), pci::Cam::MmioCam) };
    let bdf = DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    };
    config.write_word(bdf, 0x40, 1 << 31);
    let common = Box::leak(Box::new([0u64; 8]));
    let common_cfg = VirtioPciCommonCfg::new(core::ptr::NonNull::from(common).cast());
    let msix = PciMsixState {
        common_cfg,
        config,
        cap: MsixCapability {
            cap_offset: 0x40,
            table_size: 2,
            enabled: true,
            function_masked: false,
            table_bar: 0,
            table_offset: 0,
            pba_bar: 0,
            pba_offset: 0,
        },
        bdf,
        irq_provider: provider,
        msi: MsiResource::new(
            IrqResource::plain(Virq::new(0x5a31), IrqTrigger::EdgeRising),
            MsiMessage::new(0xfee0_0000, 0x91),
        ),
    };
    (PciIrqState::new(Some(msix)), provider, config, bdf)
}

#[def_test]
fn configuration_failure_retains_allocation_until_teardown() {
    let (mut state, provider, config, bdf) = fixture();
    state.fail();
    state.fail();
    assert!(state.is_failed());
    assert!(state.msix_mut().is_none());
    assert_eq!(provider.attempts.load(Ordering::Relaxed), 0);
    assert_eq!(config.read_word(bdf, 0x40) & (3 << 30), 1 << 30);

    provider.can_release.store(true, Ordering::Relaxed);
    state.release();
    state.release();
    assert_eq!(provider.released.load(Ordering::Relaxed), 1);
    assert_eq!(provider.last_irq.load(Ordering::Relaxed), 0x5a31);
}

#[def_test]
fn rejected_release_retains_disabled_owner_for_retry() {
    let (mut state, provider, config, bdf) = fixture();
    assert_eq!(state.try_release(), Err(ResError::Busy));
    assert!(state.is_failed());
    assert!(state.msix_mut().is_none());
    assert_eq!(provider.released.load(Ordering::Relaxed), 0);
    assert_eq!(config.read_word(bdf, 0x40) & (3 << 30), 1 << 30);
    let PciIrqState::Failed(Some(msix)) = &state else {
        panic!("lost MSI-X owner")
    };
    assert_eq!(msix.msi.irq.virq().raw(), 0x5a31);
    assert_eq!(msix.msi.message, MsiMessage::new(0xfee0_0000, 0x91));

    state.fail();
    assert_eq!(state.try_release(), Err(ResError::Busy));
    provider.can_release.store(true, Ordering::Relaxed);
    assert_eq!(state.try_release(), Ok(()));
    assert!(state.is_failed());
    assert_eq!(state.try_release(), Ok(()));
    state.release();
    assert_eq!(provider.attempts.load(Ordering::Relaxed), 3);
    assert_eq!(provider.released.load(Ordering::Relaxed), 1);
}

#[def_test]
fn legacy_release_does_not_turn_transport_into_failed_msix() {
    let mut state = PciIrqState::new(None);
    assert_eq!(state.try_release(), Ok(()));
    state.fail();
    assert!(!state.is_failed());
}
