// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Local APIC and IO APIC setup for x86 platforms.

use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use kplat::{
    interrupts::{HandlerTable, TargetCpu},
    memory::{PhysAddr, p2v},
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use x2apic::{
    ioapic::{IoApic, IrqFlags},
    lapic::{LocalApic, LocalApicBuilder, xapic_base},
};
use x86_64::instructions::port::Port;

/// APIC vector assignments.
pub const APIC_TIMER_VECTOR: u8 = 0xf0;
pub const APIC_SPURIOUS_VECTOR: u8 = 0xf1;
pub const APIC_ERROR_VECTOR: u8 = 0xf2;
/// First CPU vector reserved for MSI-X. Vectors 0x40–0xEF are available
/// for MSI-X (above the IO-APIC range 0x20–0x3F, below APIC_TIMER_VECTOR).
pub const MSIX_VECTOR_BASE: u8 = 0x40;
/// Base vector number used by IO-APIC entries (IRQ 0 → vector 0x20).
pub const IO_APIC_VECTOR_BASE: usize = 0x20;

const MAX_IRQ_COUNT: usize = 256;

static mut LOCAL_APIC: MaybeUninit<LocalApic> = MaybeUninit::uninit();
static mut IS_X2APIC: bool = false;
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();

/// Counter used to dynamically allocate MSI-X CPU vectors.
/// Starts at MSIX_VECTOR_BASE and increments on each allocation.
static MSIX_VECTOR_COUNTER: AtomicU8 = AtomicU8::new(MSIX_VECTOR_BASE);

/// IRQ handler table shared by the platform's `IntrManager` implementation.
pub static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();

/// Allocates the next available MSI-X CPU vector.
///
/// Returns `None` when all vectors in the range
/// `[MSIX_VECTOR_BASE, APIC_TIMER_VECTOR)` are exhausted.
#[unsafe(export_name = "__kplat_alloc_msix_vector")]
pub fn alloc_msix_vector() -> Option<u8> {
    // Use a compare-exchange loop to atomically check-and-increment,
    // avoiding any risk of the counter wrapping past APIC_TIMER_VECTOR when
    // called concurrently (e.g. from multiple CPUs during boot).
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

/// Returns the APIC ID of the current logical CPU.
#[unsafe(export_name = "__kplat_current_apic_id")]
pub fn current_apic_id() -> u8 {
    raw_cpuid::CpuId::new()
        .get_feature_info()
        .map_or(0, |f| f.initial_local_apic_id())
}

/// Enables or disables the IO APIC line for the given IRQ number.
///
/// MSI-X vectors (>= MSIX_VECTOR_BASE) bypass the IO-APIC entirely and are
/// delivered directly by the Local APIC, so they are ignored here.
pub fn enable(irq: usize, enabled: bool) {
    // MSI-X vectors are not routed through the IO-APIC.
    if irq >= MSIX_VECTOR_BASE as usize {
        return;
    }

    let vector = IO_APIC_VECTOR_BASE + irq;

    if vector < APIC_TIMER_VECTOR as usize {
        unsafe {
            let mut io_apic = IO_APIC.lock();

            if irq <= io_apic.max_table_entry() as usize {
                // RTE was configured in init_primary() with vector, dest, mode,
                // trigger, etc. Here we only need to toggle the mask bit.
                if enabled {
                    io_apic.enable_irq(irq as u8);
                } else {
                    io_apic.disable_irq(irq as u8);
                }
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
///
/// `io_apic_paddr` is the physical base address of the IO APIC MMIO region.
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
        let base_vaddr = p2v(kplat::memory::pa!(unsafe { xapic_base() } as usize));
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    let mut lapic = builder.build().unwrap();
    unsafe {
        lapic.enable();
        #[allow(static_mut_refs)]
        LOCAL_APIC.write(lapic);
    }

    let mut io_apic = unsafe { IoApic::new(p2v(io_apic_paddr).as_usize() as u64) };

    unsafe {
        use x2apic::ioapic::{IrqMode, RedirectionTableEntry};

        let max_entry = io_apic.max_table_entry();
        info!(
            "  IO-APIC supports {} IRQ inputs (0-{})",
            max_entry + 1,
            max_entry
        );

        // Set default RTE for all IRQ lines (masked state).
        // ISA IRQ 0-9 use edge-triggered, high-active (PC/AT convention).
        // IRQ 10 and above may be used by PCI INTx. The PCI spec mandates
        // INTx as level-triggered, low-active; configure as such here so
        // that the legacy fallback path (device without MSI-X) works correctly.
        for irq in 0..=max_entry {
            let mut entry = RedirectionTableEntry::default();
            entry.set_vector((IO_APIC_VECTOR_BASE as u8) + irq);
            entry.set_dest(0);
            entry.set_mode(IrqMode::Fixed);
            if irq >= 10 {
                // PCI INTx: level-triggered, low-active, masked
                entry
                    .set_flags(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE | IrqFlags::MASKED);
            } else {
                // ISA: edge-triggered, high-active, masked
                entry.set_flags(IrqFlags::MASKED);
            }
            io_apic.set_table_entry(irq, entry);
        }
        info!("IO-APIC initialized and masked");
    }

    IO_APIC.init_once(SpinNoIrq::new(io_apic));
}

/// Initializes local APIC on a secondary CPU.
pub fn init_secondary() {
    unsafe { local_apic().enable() };
}

/// Sends an IPI to a target CPU.
pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Self_ => {
            unsafe {
                local_apic().send_ipi_self(interrupt_id as _);
            };
        }
        TargetCpu::Specific(cpu_id) => {
            let apic_id = raw_apic_id(cpu_id);
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

/// Dispatches an incoming CPU vector to the registered IRQ handler.
///
/// Handles three ranges:
/// - `[APIC_TIMER_VECTOR, ...)`: Local APIC internal interrupts (timer/spurious/error) — passed through directly.
/// - `[MSIX_VECTOR_BASE, APIC_TIMER_VECTOR)`: MSI-X vectors — edge-triggered, no masking needed.
/// - `[IO_APIC_VECTOR_BASE, MSIX_VECTOR_BASE)`: IO-APIC external IRQs — vector translated back to IRQ number;
///   level-triggered interrupts are masked before EOI if no handler consumed them.
pub fn dispatch_irq(vector: usize) -> Option<usize> {
    let irq = if vector >= APIC_TIMER_VECTOR as usize {
        // Local APIC internal interrupt (Timer/Spurious/Error).
        // These are edge-triggered and do not go through IO-APIC,
        // so just dispatch to handler and send EOI.
        trace!("LAPIC IRQ {}", vector);
        IRQ_HANDLER_TABLE.handle(vector);
        unsafe { local_apic().end_of_interrupt() };
        return Some(vector);
        vector
    } else if vector >= MSIX_VECTOR_BASE as usize {
        // MSI-X vector range: the vector IS the IRQ identifier.
        // MSI-X is edge-triggered, so no masking is needed on dispatch.
        let irq = vector;
        trace!("MSI-X IRQ {}", irq);
        IRQ_HANDLER_TABLE.handle(irq);
        unsafe { local_apic().end_of_interrupt() };
        return Some(irq);
    } else if vector >= IO_APIC_VECTOR_BASE {
        // IO-APIC external interrupt — translate back to IRQ number.
        vector - IO_APIC_VECTOR_BASE
    } else {
        return None;
    };

    // IO-APIC path only (IRQ 0..31)
    trace!("IRQ {}", irq);
    if !IRQ_HANDLER_TABLE.handle(irq) {
        // For level-triggered IO-APIC interrupts (e.g. PCI INTx), the device
        // keeps asserting the interrupt line until the driver explicitly acks.
        // If no handler consumed the interrupt, mask the IRQ line before EOI
        // to prevent an interrupt storm when the device line is still asserted.
        //
        // The async poll mechanism wakes the task via irq_hook; after the task
        // processes the data it calls khal::irq::enable(irq, true) inside
        // register_irq_waker() to re-enable the IRQ line.
        enable(irq, false);
    }
    unsafe { local_apic().end_of_interrupt() };
    Some(irq)
}

/// Implement `kplat::interrupts::IntrManager` using this APIC backend.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! irq_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::interrupts::IntrManager for $name {
            fn enable(irq: usize, enabled: bool) {
                $crate::apic::enable(irq, enabled);
            }

            fn reg_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
                if $crate::apic::IRQ_HANDLER_TABLE.register_handler(irq, handler) {
                    Self::enable(irq, true);
                    return true;
                }
                warn!("reg_handler handler for IRQ {} failed", irq);
                false
            }

            fn unreg_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
                Self::enable(irq, false);
                $crate::apic::IRQ_HANDLER_TABLE.unregister_handler(irq)
            }

            fn dispatch_irq(vector: usize) -> Option<usize> {
                $crate::apic::dispatch_irq(vector)
            }

            fn notify_cpu(interrupt_id: usize, target: kplat::interrupts::TargetCpu) {
                $crate::apic::notify_cpu(interrupt_id, target);
            }

            fn set_prio(_irq: usize, _priority: u8) {
                todo!()
            }
        }
    };
}
