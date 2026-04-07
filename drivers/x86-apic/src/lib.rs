// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(target_arch = "x86_64")]
#![no_std]
#[macro_use]
extern crate log;

use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use kplat::{
    interrupts::{HandlerTable, TargetCpu},
    memory::PhysAddr,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memspace::iomap_device;
use x2apic::{
    ioapic::{IoApic, IrqFlags},
    lapic::{LocalApic, LocalApicBuilder, xapic_base},
};
use x86_64::instructions::port::Port;

pub const APIC_TIMER_VECTOR: u8 = 0xf0;
pub const APIC_SPURIOUS_VECTOR: u8 = 0xf1;
pub const APIC_ERROR_VECTOR: u8 = 0xf2;
pub const MSIX_VECTOR_BASE: u8 = 0x40;
pub const IO_APIC_VECTOR_BASE: usize = 0x20;

const MAX_IRQ_COUNT: usize = 256;

static mut LOCAL_APIC: MaybeUninit<LocalApic> = MaybeUninit::uninit();
static mut IS_X2APIC: bool = false;
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();
static MSIX_VECTOR_COUNTER: AtomicU8 = AtomicU8::new(MSIX_VECTOR_BASE);

pub static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();

#[unsafe(export_name = "__kplat_alloc_msix_vector")]
pub fn alloc_msix_vector() -> Option<u8> {
    loop {
        let current = MSIX_VECTOR_COUNTER.load(Ordering::Relaxed);
        if current >= APIC_TIMER_VECTOR {
            return None;
        }
        match MSIX_VECTOR_COUNTER.compare_exchange(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(current),
            Err(_) => continue,
        }
    }
}

#[unsafe(export_name = "__kplat_current_apic_id")]
pub fn current_apic_id() -> u8 {
    raw_cpuid::CpuId::new()
        .get_feature_info()
        .map_or(0, |f| f.initial_local_apic_id())
}

pub fn enable(irq: usize, enabled: bool) {
    if irq >= MSIX_VECTOR_BASE as usize {
        return;
    }

    let vector = IO_APIC_VECTOR_BASE + irq;
    if vector < APIC_TIMER_VECTOR as usize {
        unsafe {
            let mut io_apic = IO_APIC.lock();
            if irq <= io_apic.max_table_entry() as usize {
                if enabled {
                    io_apic.enable_irq(irq as u8);
                } else {
                    io_apic.disable_irq(irq as u8);
                }
            }
        }
    }
}

#[allow(static_mut_refs)]
pub fn local_apic<'a>() -> &'a mut LocalApic {
    unsafe { LOCAL_APIC.assume_init_mut() }
}

pub fn raw_apic_id(id_u8: u8) -> u32 {
    if unsafe { IS_X2APIC } {
        id_u8 as u32
    } else {
        (id_u8 as u32) << 24
    }
}

fn cpu_has_x2apic() -> bool {
    match raw_cpuid::CpuId::new().get_feature_info() {
        Some(finfo) => finfo.has_x2apic(),
        None => false,
    }
}

pub fn init_primary(io_apic_paddr: PhysAddr) {
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
        let base_vaddr = iomap_device(
            kplat::memory::pa!(unsafe { xapic_base() } as usize),
            0x1000,
            "lapic",
        )
        .unwrap_or_else(|err| panic!("failed to iomap LAPIC: {err:?}"));
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    let mut lapic = builder.build().unwrap();
    unsafe {
        lapic.enable();
        #[allow(static_mut_refs)]
        LOCAL_APIC.write(lapic);
    }

    let io_apic_base = iomap_device(io_apic_paddr, 0x1000, "ioapic")
        .unwrap_or_else(|err| panic!("failed to iomap IOAPIC: {err:?}"));
    let mut io_apic = unsafe { IoApic::new(io_apic_base.as_usize() as u64) };

    unsafe {
        use x2apic::ioapic::{IrqMode, RedirectionTableEntry};

        let max_entry = io_apic.max_table_entry();
        info!(
            "  IO-APIC supports {} IRQ inputs (0-{})",
            max_entry + 1,
            max_entry
        );

        for irq in 0..=max_entry {
            let mut entry = RedirectionTableEntry::default();
            entry.set_vector((IO_APIC_VECTOR_BASE as u8) + irq);
            entry.set_dest(0);
            entry.set_mode(IrqMode::Fixed);
            if irq >= 10 {
                entry
                    .set_flags(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE | IrqFlags::MASKED);
            } else {
                entry.set_flags(IrqFlags::MASKED);
            }
            io_apic.set_table_entry(irq, entry);
        }
        info!("IO-APIC initialized and masked");
    }

    IO_APIC.init_once(SpinNoIrq::new(io_apic));
}

pub fn init_from_firmware() {
    let io_apic_paddr = ::acpi::find_io_apic_from_init()
        .map(|entry| kplat::memory::pa!(entry.address as usize))
        .unwrap_or_else(|| {
            warn!("ACPI MADT IOAPIC not found, fallback to static IOAPIC base");
            kplat::memory::pa!(0xFEC0_0000)
        });
    init_primary(io_apic_paddr);
}

pub fn init_secondary() {
    unsafe { local_apic().enable() };
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Self_ => unsafe {
            local_apic().send_ipi_self(interrupt_id as _);
        },
        TargetCpu::Specific(cpu_id) => {
            let apic_id = raw_apic_id(cpu_id as u8);
            unsafe {
                local_apic().send_ipi(interrupt_id as _, apic_id as _);
            };
        }
        TargetCpu::AllButSelf { me: _, total: _ } => {
            use x2apic::lapic::IpiAllShorthand;
            unsafe {
                local_apic().send_ipi_all(interrupt_id as _, IpiAllShorthand::AllExcludingSelf);
            };
        }
    }
}

pub fn dispatch_irq(vector: usize) -> Option<usize> {
    let irq = if vector >= APIC_TIMER_VECTOR as usize {
        trace!("LAPIC IRQ {}", vector);
        IRQ_HANDLER_TABLE.handle(vector);
        unsafe { local_apic().end_of_interrupt() };
        return Some(vector);
    } else if vector >= MSIX_VECTOR_BASE as usize {
        let irq = vector;
        trace!("MSI-X IRQ {}", irq);
        IRQ_HANDLER_TABLE.handle(irq);
        unsafe { local_apic().end_of_interrupt() };
        return Some(irq);
    } else if vector >= IO_APIC_VECTOR_BASE {
        vector - IO_APIC_VECTOR_BASE
    } else {
        return None;
    };

    trace!("IRQ {}", irq);
    if !IRQ_HANDLER_TABLE.handle(irq) {
        enable(irq, false);
    }
    unsafe { local_apic().end_of_interrupt() };
    Some(irq)
}

#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! irq_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::interrupts::IntrManager for $name {
            fn enable(irq: usize, enabled: bool) {
                $crate::enable(irq, enabled);
            }

            fn reg_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
                if $crate::IRQ_HANDLER_TABLE.register_handler(irq, handler) {
                    Self::enable(irq, true);
                    return true;
                }
                warn!("reg_handler handler for IRQ {} failed", irq);
                false
            }

            fn unreg_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
                Self::enable(irq, false);
                $crate::IRQ_HANDLER_TABLE.unregister_handler(irq)
            }

            fn dispatch_irq(vector: usize) -> Option<usize> {
                $crate::dispatch_irq(vector)
            }

            fn notify_cpu(interrupt_id: usize, target: kplat::interrupts::TargetCpu) {
                $crate::notify_cpu(interrupt_id, target);
            }

            fn set_prio(_irq: usize, _priority: u8) {
                todo!()
            }
        }
    };
}
