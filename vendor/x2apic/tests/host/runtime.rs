//! x2APIC command checks use the register boundary's mock MSR write recorder.

use super::*;
use crate::lapic::LocalApicMode;

fn runtime() -> RuntimeLocalApic {
    let mode = LocalApicMode::X2Apic;
    RuntimeLocalApic {
        regs: LocalApicRegisters::new(mode),
        mode,
        error_vector: 0xf2,
        ipi_destination_mode: IpiDestMode::Physical,
        _not_send_sync: PhantomData,
    }
}

#[test]
fn x2apic_retains_full_width_destination_in_nmi_and_fixed_commands() {
    let apic = runtime();
    for destination in [0, 0x100, 0x1234_5678, u32::MAX] {
        let nmi = apic
            .encode_command(
                IpiCommand::Nmi,
                IpiTarget::Destination(destination),
            )
            .unwrap();
        assert_eq!((nmi >> 32) as u32, destination);
        assert_eq!(nmi as u32, 0x4400);
        let fixed = apic
            .encode_command(
                IpiCommand::Fixed(0xf3),
                IpiTarget::Destination(destination),
            )
            .unwrap();
        assert_eq!((fixed >> 32) as u32, destination);
        assert_eq!(fixed as u32, 0x40f3);
    }
}

#[test]
fn x2apic_submits_each_command_with_one_complete_msr_write() {
    let apic = runtime();
    use crate::lapic::register_tests::take_msr_writes;
    let _ = take_msr_writes();
    for (command, expected) in [
        (IpiCommand::Nmi, 0x4400),
        (IpiCommand::Init, 0x4500),
        (IpiCommand::Startup(8), 0x4608),
    ] {
        // SAFETY: cfg(test) records MSR writes at the register boundary instead
        // of executing WRMSR. No physical AP/NMI handlers can be invoked.
        let result = unsafe {
            apic.try_send_ipi(command, IpiTarget::Destination(0x1234_5678))
        };
        assert_eq!(result, Ok(()));
        assert_eq!(
            take_msr_writes(),
            [(0x830, 0x1234_5678_0000_0000 | expected)]
        );
    }
    // SAFETY: the same mock intercepts the complete fixed IPI MSR write.
    assert_eq!(
        unsafe {
            apic.send_fixed_ipi(0xf3, IpiTarget::Destination(0x1234_5678))
        },
        Ok(())
    );
    assert_eq!(take_msr_writes(), [(0x830, 0x1234_5678_0000_40f3)]);
}

#[test]
fn nmi_broadcast_encodes_shorthand_without_a_fake_interrupt_vector() {
    let apic = runtime();
    let command = apic
        .encode_command(IpiCommand::Nmi, IpiTarget::AllExcludingSelf)
        .unwrap();
    assert_eq!(command as u32 & 0xff, 0);
    assert_eq!(command as u32 & (7 << 8), 4 << 8);
    assert_eq!(command as u32 & (3 << 18), 3 << 18);
    assert_eq!(command >> 32, 0);
}
