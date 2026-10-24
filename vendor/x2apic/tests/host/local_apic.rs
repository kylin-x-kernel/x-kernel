extern crate std;

use self::std::{cell::RefCell, vec::Vec};
use super::*;

std::thread_local! {
    static ACCESSES: RefCell<Vec<Access>> = const { RefCell::new(Vec::new()) };
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
