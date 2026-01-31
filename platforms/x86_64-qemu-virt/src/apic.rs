// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Local APIC and IO APIC setup for x86_64-qemu-virt.

use core::mem::MaybeUninit;

use kplat::memory::{PhysAddr, p2v, pa};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use x2apic::{
    ioapic::IoApic,
    lapic::{LocalApic, LocalApicBuilder, xapic_base},
};
use x86_64::instructions::port::Port;

use self::vectors::*;
/// APIC vector assignments.
pub(super) mod vectors {
    pub const APIC_TIMER_VECTOR: u8 = 0xf0;
    pub const APIC_SPURIOUS_VECTOR: u8 = 0xf1;
    pub const APIC_ERROR_VECTOR: u8 = 0xf2;
}
const IO_APIC_BASE: PhysAddr = pa!(0xFEC0_0000);
static mut LOCAL_APIC: MaybeUninit<LocalApic> = MaybeUninit::uninit();
static mut IS_X2APIC: bool = false;
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();
/// Enables or disables the IO APIC line for the given vector.
pub fn enable(vector: usize, enabled: bool) {
    if vector < APIC_TIMER_VECTOR as _ {
        unsafe {
            if enabled {
                IO_APIC.lock().enable_irq(vector as u8);
            } else {
                IO_APIC.lock().disable_irq(vector as u8);
            }
        }
    }
}
/// Returns a mutable reference to the local APIC.
#[allow(static_mut_refs)]
pub fn local_apic<'a>() -> &'a mut LocalApic {
    unsafe { LOCAL_APIC.assume_init_mut() }
}
/// Converts an APIC ID into a raw APIC register format.
#[cfg(feature = "smp")]
pub fn raw_apic_id(id_u8: u8) -> u32 {
    if unsafe { IS_X2APIC } {
        id_u8 as u32
    } else {
        (id_u8 as u32) << 24
    }
}
/// Detects whether the CPU supports x2APIC.
fn cpu_has_x2apic() -> bool {
    match raw_cpuid::CpuId::new().get_feature_info() {
        Some(finfo) => finfo.has_x2apic(),
        None => false,
    }
}
/// Initializes local and IO APIC on the boot CPU.
pub fn init_primary() {
    info!("Initialize Local APIC...");
    unsafe {
        Port::<u8>::new(0x21).write(0xff);
        Port::<u8>::new(0xA1).write(0xff);
    }
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if cpu_has_x2apic() {
        info!("Using x2APIC.");
        unsafe { IS_X2APIC = true };
    } else {
        info!("Using xAPIC.");
        let base_vaddr = p2v(pa!(unsafe { xapic_base() } as usize));
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    let mut lapic = builder.build().unwrap();
    unsafe {
        lapic.enable();
        #[allow(static_mut_refs)]
        LOCAL_APIC.write(lapic);
    }
    info!("Initialize IO APIC...");
    let io_apic = unsafe { IoApic::new(p2v(IO_APIC_BASE).as_usize() as u64) };
    IO_APIC.init_once(SpinNoIrq::new(io_apic));
}
/// Initializes local APIC on a secondary CPU.
#[cfg(feature = "smp")]
pub fn init_secondary() {
    unsafe { local_apic().enable() };
}
mod irq_impl {
    use kplat::interrupts::{Handler, HandlerTable, IntrManager, TargetCpu};
    const MAX_IRQ_COUNT: usize = 256;
    static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
    struct IntrManagerImpl;
    #[impl_dev_interface]
    impl IntrManager for IntrManagerImpl {
        fn enable(vector: usize, enabled: bool) {
            super::enable(vector, enabled);
        }

        fn reg_handler(vector: usize, handler: Handler) -> bool {
            if IRQ_HANDLER_TABLE.register_handler(vector, handler) {
                Self::enable(vector, true);
                return true;
            }
            warn!("reg_handler handler for IRQ {} failed", vector);
            false
        }

        fn unreg_handler(vector: usize) -> Option<Handler> {
            Self::enable(vector, false);
            IRQ_HANDLER_TABLE.unregister_handler(vector)
        }

        fn dispatch_irq(vector: usize) -> Option<usize> {
            trace!("IRQ {}", vector);
            if !IRQ_HANDLER_TABLE.handle(vector) {
                warn!("Unhandled IRQ {vector}");
            }
            unsafe { super::local_apic().end_of_interrupt() };
            Some(vector)
        }

        fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
            match target {
                TargetCpu::Self_ => {
                    unsafe {
                        super::local_apic().send_ipi_self(interrupt_id as _);
                    };
                }
                TargetCpu::Specific(cpu_id) => {
                    unsafe {
                        super::local_apic().send_ipi(interrupt_id as _, cpu_id as _);
                    };
                }
                TargetCpu::AllButSelf { me: _, total: _ } => {
                    use x2apic::lapic::IpiAllShorthand;
                    unsafe {
                        super::local_apic()
                            .send_ipi_all(interrupt_id as _, IpiAllShorthand::AllExcludingSelf);
                    };
                }
            }
        }

        fn set_prio(_irq: usize, _priority: u8) {
            todo!()
        }

        fn save_disable() -> usize {
            todo!()
        }

        fn restore(_flag: usize) {
            todo!()
        }

        fn enable_local() {
            todo!()
        }

        fn disable_local() {
            todo!()
        }

        fn is_enabled() -> bool {
            todo!()
        }
    }
}
