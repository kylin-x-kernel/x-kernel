extern crate std;

use self::std::{boxed::Box, cell::RefCell, vec::Vec};
use super::*;

std::thread_local! {
    static ACCESSES: RefCell<Vec<Access>> = const { RefCell::new(Vec::new()) };
    static ICR_HIGH_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static MSR_WRITES: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn record_msr_write(msr: u32, value: u64) {
    MSR_WRITES.with(|writes| writes.borrow_mut().push((msr, value)));
}

pub(super) fn take_msr_writes() -> Vec<(u32, u64)> {
    MSR_WRITES.with(|writes| core::mem::take(&mut *writes.borrow_mut()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Read(usize),
    Write(usize, u32),
}

pub(super) fn record_read(address: u64) {
    ACCESSES.with(|accesses| {
        accesses
            .borrow_mut()
            .push(Access::Read(address as usize & 0xfff))
    });
}

pub(super) fn record_write(address: u64, value: u32) {
    ACCESSES.with(|accesses| {
        accesses
            .borrow_mut()
            .push(Access::Write(address as usize & 0xfff, value))
    });
    if address as usize & 0xfff == 0x310 {
        let hook = ICR_HIGH_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[repr(C, align(4096))]
struct RegisterPage([u32; 1024]);

fn mock_apic(page: &mut RegisterPage) -> LocalApic {
    let mode = LocalApicMode::XApic {
        xapic_base: page.0.as_mut_ptr() as u64,
    };
    LocalApic {
        mode,
        timer_vector: 0xf0,
        error_vector: 0xf2,
        spurious_vector: 0xf1,
        timer_mode: TimerMode::Periodic,
        timer_divide: TimerDivide::Div1,
        timer_initial: 10_000_000,
        ipi_destination_mode: IpiDestMode::Physical,
        regs: LocalApicRegisters::new(mode),
        _not_send_sync: PhantomData,
    }
}

#[test]
fn masked_initialization_never_writes_unmasked_timer_or_pins() {
    let mut page = RegisterPage([0; 1024]);
    let lint0 = 0x8700;
    let lint1 = 0x400;
    page.0[LVT_LINT0.1 as usize / 4] = lint0;
    page.0[LVT_LINT1.1 as usize / 4] = lint1;
    page.0[LVT_TIMER.1 as usize / 4] = 2 << 17;
    page.0[TICR.1 as usize / 4] = 123;
    let mut apic = mock_apic(&mut page);
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    // SAFETY: All xAPIC accesses target this live, aligned private mock page;
    // xAPIC mode performs no privileged MSR access in this method.
    unsafe { apic.enable_masked() };
    let writes: Vec<_> = ACCESSES.with(|accesses| {
        accesses
            .borrow()
            .iter()
            .filter_map(|access| match *access {
                Access::Write(offset, value) => Some((offset, value)),
                Access::Read(_) => None,
            })
            .collect()
    });
    assert_eq!(
        writes.first(),
        Some(&(LVT_ERROR.1 as usize, (1 << 16) | 0xf2))
    );
    for offset in [LVT_TIMER.1, LVT_LINT0.1, LVT_LINT1.1, LVT_ERROR.1] {
        let values: Vec<_> = writes
            .iter()
            .filter(|(at, _)| *at == offset as usize)
            .collect();
        assert!(!values.is_empty());
        assert!(values.iter().all(|(_, value)| value & (1 << 16) != 0));
    }
    assert_eq!(page.0[LVT_LINT0.1 as usize / 4], lint0 | (1 << 16));
    assert_eq!(page.0[LVT_LINT1.1 as usize / 4], lint1 | (1 << 16));
    assert_eq!(page.0[LVT_TIMER.1 as usize / 4], (1 << 16) | 0xf0);
    assert_eq!(page.0[TICR.1 as usize / 4], 0);
    assert_eq!(writes.last(), Some(&(SIVR.1 as usize, 0x1f1)));
    assert_eq!(apic.timer_initial, 0);
    // SAFETY: Same live private mock; explicit activation is tested separately.
    unsafe { apic.enable_timer() };
    assert_eq!(page.0[LVT_TIMER.1 as usize / 4], 0xf0);
}

#[test]
fn error_activation_refreshes_esr_without_eoi_or_other_unmasking() {
    for max_lvt in [3, 6] {
        let mut page = RegisterPage([0; 1024]);
        page.0[VERSION.1 as usize / 4] = max_lvt << 16;
        let mut apic = mock_apic(&mut page);
        // SAFETY: Private aligned mock register page, no privileged MSRs.
        unsafe { apic.enable_masked() };
        ACCESSES.with(|accesses| accesses.borrow_mut().clear());
        // SAFETY: Mock masked setup completed; there is no physical delivery.
        unsafe { apic.enable_error() };
        let mut expected = Vec::new();
        expected.push(Access::Read(VERSION.1 as usize));
        if max_lvt > 3 {
            expected.push(Access::Write(ERROR.1 as usize, 0));
        }
        expected.push(Access::Read(ERROR.1 as usize));
        expected.push(Access::Write(LVT_ERROR.1 as usize, 0xf2));
        if max_lvt > 3 {
            expected.push(Access::Write(ERROR.1 as usize, 0));
        }
        expected.push(Access::Read(ERROR.1 as usize));
        ACCESSES.with(|accesses| assert_eq!(*accesses.borrow(), expected));
        for offset in [LVT_TIMER.1, LVT_LINT0.1, LVT_LINT1.1] {
            assert_ne!(page.0[offset as usize / 4] & (1 << 16), 0);
        }
    }
}

#[test]
fn error_refreshes_status_before_read_and_single_eoi() {
    let mut page = RegisterPage([0; 1024]);
    page.0[VERSION.1 as usize / 4] = 6 << 16;
    let mut apic = mock_apic(&mut page);
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    // SAFETY: This test uses a private mock page, not a physical APIC. Its
    // zero status models delivery with no error bits remaining latched.
    let flags = unsafe { apic.acknowledge_error() };
    assert!(flags.is_empty());
    ACCESSES.with(|accesses| {
        assert_eq!(
            &*accesses.borrow(),
            &[
                Access::Read(VERSION.1 as usize),
                Access::Write(ERROR.1 as usize, 0),
                Access::Read(ERROR.1 as usize),
                Access::Write(EOI.1 as usize, 0),
            ]
        )
    });
}

#[test]
fn legacy_error_preserves_status_without_esr_write() {
    let mut page = RegisterPage([0; 1024]);
    page.0[VERSION.1 as usize / 4] = 3 << 16;
    page.0[ERROR.1 as usize / 4] =
        ErrorFlags::SEND_ILLEGAL_VECTOR.bits() as u32;
    let mut apic = mock_apic(&mut page);
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    // SAFETY: All register accesses target this live private mock page.
    let flags = unsafe { apic.acknowledge_error() };
    assert_eq!(flags, ErrorFlags::SEND_ILLEGAL_VECTOR);
    ACCESSES.with(|accesses| {
        assert_eq!(
            &*accesses.borrow(),
            &[
                Access::Read(VERSION.1 as usize),
                Access::Read(ERROR.1 as usize),
                Access::Write(EOI.1 as usize, 0),
            ]
        )
    });
}

#[test]
fn runtime_commands_preserve_startup_encoding_in_both_destination_modes() {
    for destination_mode in [IpiDestMode::Physical, IpiDestMode::Logical] {
        let mut startup_page = RegisterPage([0; 1024]);
        let mut runtime_page = RegisterPage([0; 1024]);
        let mut startup = mock_apic(&mut startup_page);
        startup.ipi_destination_mode = destination_mode;
        let mut prepared = mock_apic(&mut runtime_page);
        prepared.ipi_destination_mode = destination_mode;
        let runtime = prepared.into_runtime();
        for (command, expected) in [
            (IpiCommand::Fixed(0x55), 0x4055),
            (IpiCommand::Nmi, 0x4400),
            (IpiCommand::Init, 0x4500),
            (IpiCommand::Startup(0x8), 0x4608),
        ] {
            // SAFETY: both handles use private aligned mock pages; these
            // commands never reach hardware or start another processor.
            unsafe {
                match command {
                    IpiCommand::Fixed(vector) => {
                        startup.send_ipi(vector, 0x2300_0000)
                    }
                    IpiCommand::Nmi => startup.send_nmi(0x2300_0000),
                    IpiCommand::Init => startup.send_init_ipi(0x2300_0000),
                    IpiCommand::Startup(page) => {
                        startup.send_sipi(page, 0x2300_0000)
                    }
                }
            }
            // SAFETY: same exclusively owned mock hardware, no physical IPIs.
            let result = unsafe {
                runtime.try_send_ipi(command, IpiTarget::Destination(0x23))
            };
            assert_eq!(result, Ok(()));
            let logical_bit = if destination_mode == IpiDestMode::Logical {
                1 << 11
            } else {
                0
            };
            assert_eq!(runtime_page.0[0x300 / 4], expected | logical_bit);
            assert_eq!(runtime_page.0[0x310 / 4], 0x2300_0000);
            for offset in [0x300, 0x310] {
                assert_eq!(
                    runtime_page.0[offset / 4],
                    startup_page.0[offset / 4]
                );
            }
        }
    }
}

#[test]
fn nested_nmi_shorthand_preserves_an_interrupted_xapic_destination() {
    let mut page = RegisterPage([0; 1024]);
    let apic = mock_apic(&mut page).into_runtime();
    let pointer = core::ptr::from_ref(&apic);
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    ICR_HIGH_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            // SAFETY: synchronous hook runs between the outer command's two
            // MMIO writes while apic and its private mapping remain live.
            // Only shared borrows coexist, modeling NMI reentry on this CPU.
            let nested = unsafe { &*pointer };
            // SAFETY: same private mock CPU/registers, no physical delivery.
            let result = unsafe {
                nested
                    .try_send_ipi(IpiCommand::Nmi, IpiTarget::AllExcludingSelf)
            };
            assert_eq!(result, Ok(()));
            // An independent NMI-owned register may still be written while
            // the ordinary sender is interrupted.
            // SAFETY: this test owns the mock performance LVT and its producer.
            unsafe { nested.set_performance_nmi_enabled(false) };
        }));
    });
    // SAFETY: private aligned register page, initialized handle, no CPU migration.
    let result = unsafe {
        apic.try_send_ipi(IpiCommand::Fixed(0x55), IpiTarget::Destination(0x23))
    };
    assert_eq!(result, Ok(()));
    assert_eq!(page.0[0x310 / 4], 0x2300_0000);
    assert_eq!(page.0[0x300 / 4], 0x4055);
    assert_eq!(page.0[0x340 / 4], 0x10400);
    let writes: Vec<_> = ACCESSES.with(|accesses| {
        accesses
            .borrow()
            .iter()
            .filter_map(|access| match access {
                Access::Write(offset, value)
                    if *offset == 0x300 || *offset == 0x310 =>
                {
                    Some((*offset, *value))
                }
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        writes,
        [(0x310, 0x2300_0000), (0x300, 0xc4400), (0x300, 0x4055)]
    );
    // Directed NMI remains available in ordinary context after the send.
    // SAFETY: same private mock and stable handle, no physical NMI delivery.
    assert_eq!(
        unsafe {
            apic.try_send_ipi(IpiCommand::Nmi, IpiTarget::Destination(0x77))
        },
        Ok(())
    );
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    assert_eq!(page.0[0x300 / 4], 0x4400);
}

#[test]
fn fixed_ipi_rejects_invalid_xapic_destination_before_waiting_or_writing() {
    let mut page = RegisterPage([0; 1024]);
    page.0[0x300 / 4] = 1 << 12;
    page.0[0x310 / 4] = 0x7700_0000;
    let apic = mock_apic(&mut page).into_runtime();
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    // SAFETY: private aligned mock register page; no hardware accesses.
    // The invalid target must be rejected before the unbounded busy wait.
    let result = unsafe {
        apic.send_fixed_ipi(0x55, IpiTarget::Destination(0x100))
    };
    assert_eq!(result, Err(IpiSendError::InvalidDestination));
    assert_eq!(page.0[0x300 / 4], 1 << 12);
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    assert!(ACCESSES.with(|accesses| accesses.borrow().is_empty()));
}

#[test]
fn hardware_busy_and_invalid_destination_never_write_icr() {
    let mut page = RegisterPage([0; 1024]);
    page.0[0x300 / 4] = 1 << 12;
    page.0[0x310 / 4] = 0x1200_0000;
    let apic = mock_apic(&mut page).into_runtime();
    ACCESSES.with(|accesses| accesses.borrow_mut().clear());
    // SAFETY: private register mapping and no privileged operations or delivery.
    assert_eq!(
        unsafe {
            apic.try_send_ipi(IpiCommand::Nmi, IpiTarget::Destination(0x44))
        },
        Err(IpiSendError::Busy)
    );
    // SAFETY: same mock; oversized target must be rejected before register writes.
    assert_eq!(
        unsafe {
            apic.try_send_ipi(IpiCommand::Nmi, IpiTarget::Destination(0x100))
        },
        Err(IpiSendError::InvalidDestination)
    );
    assert_eq!(page.0[0x300 / 4], 1 << 12);
    assert_eq!(page.0[0x310 / 4], 0x1200_0000);
    assert!(ACCESSES.with(|accesses| accesses
        .borrow()
        .iter()
        .all(|access| !matches!(access, Access::Write(..)))));
    page.0[0x300 / 4] = 0;
    // SAFETY: same private mock, previous hardware delivery has completed.
    assert_eq!(
        unsafe {
            apic.try_send_ipi(IpiCommand::Nmi, IpiTarget::AllExcludingSelf)
        },
        Ok(())
    );
    assert_eq!(page.0[0x300 / 4], 0xc4400);
    assert_eq!(page.0[0x310 / 4], 0x1200_0000);
}

#[test]
fn xapic_self_and_broadcast_preserve_the_destination_register() {
    for (target, expected) in [
        (IpiTarget::ThisCpu, 0x44400),
        (IpiTarget::AllExcludingSelf, 0xc4400),
    ] {
        let mut page = RegisterPage([0; 1024]);
        page.0[0x310 / 4] = 0x7700_0000;
        let apic = mock_apic(&mut page).into_runtime();
        ACCESSES.with(|accesses| accesses.borrow_mut().clear());
        // SAFETY: private aligned mock page, ready source/target modeled by
        // this test; shorthand may reenter an interrupted ordinary sender.
        assert_eq!(
            unsafe { apic.try_send_ipi(IpiCommand::Nmi, target) },
            Ok(())
        );
        assert_eq!(page.0[0x310 / 4], 0x7700_0000);
        assert_eq!(page.0[0x300 / 4], expected);
        let writes: Vec<_> = ACCESSES.with(|accesses| {
            accesses.borrow().iter()
                .filter_map(|access| match access {
                    Access::Write(offset, value) => Some((*offset, *value)),
                    _ => None,
                }).collect()
        });
        assert_eq!(writes, [(0x300, expected)]);
    }
}

#[test]
fn runtime_error_and_timer_operations_preserve_source_ownership() {
    let mut page = RegisterPage([0; 1024]);
    page.0[VERSION.1 as usize / 4] = 6 << 16;
    let mut startup = mock_apic(&mut page);
    // SAFETY: private mapping, xAPIC setup performs no privileged MSR access.
    unsafe { startup.enable_masked() };
    let runtime = startup.into_runtime();
    // SAFETY: the test owns each mock local source without competing writes.
    unsafe { runtime.enable_error() };
    // SAFETY: same owned timer; configure mode before exposing timer delivery.
    unsafe { runtime.prepare_timer() };
    // SAFETY: mock timer handler is represented by the test itself.
    unsafe { runtime.set_timer_enabled(true) };
    // SAFETY: caller retains exclusive timer programming.
    unsafe { runtime.set_timer_initial(1234) };
    // SAFETY: owns the performance source independently of the timer.
    unsafe { runtime.set_performance_nmi_enabled(true) };
    assert_eq!(page.0[LVT_TIMER.1 as usize / 4], 0xf0);
    assert_eq!(page.0[TICR.1 as usize / 4], 1234);
    assert_eq!(page.0[LVT_PERF.1 as usize / 4], 0x400);
    assert_eq!(page.0[LVT_ERROR.1 as usize / 4], 0xf2);
    // SAFETY: mock in-service error is exclusively owned by this test.
    let _ = unsafe { runtime.acknowledge_error() };
    assert_eq!(page.0[EOI.1 as usize / 4], 0);
    // SAFETY: independent timer source stays programmed through error completion.
    unsafe { runtime.set_timer_initial(0) };
    assert_eq!(page.0[TICR.1 as usize / 4], 0);
}
