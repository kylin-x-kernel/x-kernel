// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

#[macro_use]
extern crate log;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcpu_id_map::RawCpuId;
use kspin::{IrqSave, SpinNoIrq};
use lazyinit::LazyInit;
use memaddr::{PhysAddr, pa};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicTriggerMode {
    Edge,
    Level,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicPolarity {
    High,
    Low,
}

const MSIX_VECTOR_COUNT: usize = (APIC_TIMER_VECTOR as usize) - (MSIX_VECTOR_BASE as usize);
const MSIX_VECTOR_WORD_BITS: usize = u64::BITS as usize;
const MSIX_VECTOR_WORDS: usize = MSIX_VECTOR_COUNT.div_ceil(MSIX_VECTOR_WORD_BITS);

#[percpu::def_percpu]
static LOCAL_APIC_PTR: usize = 0;

static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();
static MSIX_VECTOR_ALLOCATOR: SpinNoIrq<MsixVectorAllocator> =
    SpinNoIrq::new(MsixVectorAllocator::new());

struct MsixVectorAllocator {
    allocated: [u64; MSIX_VECTOR_WORDS],
}

impl MsixVectorAllocator {
    const fn new() -> Self {
        Self {
            allocated: [0; MSIX_VECTOR_WORDS],
        }
    }

    fn alloc(&mut self) -> Option<u8> {
        for index in 0..MSIX_VECTOR_COUNT {
            let word = index / MSIX_VECTOR_WORD_BITS;
            let bit = 1u64 << (index % MSIX_VECTOR_WORD_BITS);
            if self.allocated[word] & bit == 0 {
                self.allocated[word] |= bit;
                return Some(MSIX_VECTOR_BASE + index as u8);
            }
        }

        None
    }

    fn free(&mut self, vector: u8) -> bool {
        if !(MSIX_VECTOR_BASE..APIC_TIMER_VECTOR).contains(&vector) {
            return false;
        }

        let index = usize::from(vector - MSIX_VECTOR_BASE);
        let word = index / MSIX_VECTOR_WORD_BITS;
        let bit = 1u64 << (index % MSIX_VECTOR_WORD_BITS);
        let was_allocated = self.allocated[word] & bit != 0;
        self.allocated[word] &= !bit;
        was_allocated
    }
}

struct X86ApicIfImpl;

#[crate_interface::impl_interface]
impl khal::irq::X86ApicIf for X86ApicIfImpl {
    fn alloc_msix_vector() -> Option<u8> {
        MSIX_VECTOR_ALLOCATOR.lock().alloc()
    }

    fn free_msix_vector(vector: u8) -> bool {
        MSIX_VECTOR_ALLOCATOR.lock().free(vector)
    }

    fn current_apic_id() -> u8 {
        raw_cpuid::CpuId::new()
            .get_feature_info()
            .map_or(0, |f| f.initial_local_apic_id())
    }
}

pub fn set_irq_enabled(irq: usize, enabled: bool) {
    if irq >= MSIX_VECTOR_BASE as usize {
        return;
    }

    let vector = IO_APIC_VECTOR_BASE + irq;
    if vector < APIC_TIMER_VECTOR as usize {
        // SAFETY: `IO_APIC` is initialized during APIC bring-up before runtime IRQ
        // masking is used, and the spin lock serializes MMIO access to the device.
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

pub fn configure_irq(irq: usize, trigger: IoApicTriggerMode, polarity: IoApicPolarity) {
    if irq >= MSIX_VECTOR_BASE as usize {
        return;
    }

    let vector = IO_APIC_VECTOR_BASE + irq;
    if vector < APIC_TIMER_VECTOR as usize {
        // SAFETY: `IO_APIC` is initialized before IRQ descriptors are configured.
        unsafe {
            let mut io_apic = IO_APIC.lock();
            if irq <= io_apic.max_table_entry() as usize {
                let mut entry = io_apic.table_entry(irq as u8);
                let mut flags = entry.flags();
                if trigger == IoApicTriggerMode::Level {
                    flags.insert(IrqFlags::LEVEL_TRIGGERED);
                } else {
                    flags.remove(IrqFlags::LEVEL_TRIGGERED);
                }
                if polarity == IoApicPolarity::Low {
                    flags.insert(IrqFlags::LOW_ACTIVE);
                } else {
                    flags.remove(IrqFlags::LOW_ACTIVE);
                }
                entry.set_flags(flags);
                io_apic.set_table_entry(irq as u8, entry);
            }
        }
    }
}

pub fn irq_trigger_mode(irq: usize) -> Option<IoApicTriggerMode> {
    if irq >= MSIX_VECTOR_BASE as usize {
        return None;
    }

    let vector = IO_APIC_VECTOR_BASE + irq;
    if vector >= APIC_TIMER_VECTOR as usize {
        return None;
    }

    // SAFETY: `IO_APIC` is initialized before IO-APIC interrupts can be dispatched.
    unsafe {
        let mut io_apic = IO_APIC.lock();
        if irq > io_apic.max_table_entry() as usize {
            return None;
        }
        let flags = io_apic.table_entry(irq as u8).flags();
        Some(if flags.contains(IrqFlags::LEVEL_TRIGGERED) {
            IoApicTriggerMode::Level
        } else {
            IoApicTriggerMode::Edge
        })
    }
}

pub fn with_local_apic<R>(f: impl FnOnce(&mut LocalApic) -> R) -> R {
    let _irq_guard = IrqSave::new();
    let lapic = local_apic_ptr();
    assert!(
        !lapic.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: each CPU installs exactly one leaked `LocalApic` handle into its
    // own per-CPU slot during `init_primary`/`init_secondary`. Local IRQs stay
    // masked for the duration of this borrow, so the current CPU cannot re-enter
    // LAPIC access through an interrupt handler and create overlapping mutable
    // borrows to the same object.
    unsafe { f(&mut *lapic) }
}

pub fn end_of_interrupt() {
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before IRQ
        // dispatch, and EOI targets only that CPU's local APIC registers.
        unsafe { lapic.end_of_interrupt() };
    });
}

pub fn raw_apic_id(id_u8: u8) -> u32 {
    if IS_X2APIC.load(Ordering::Relaxed) {
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
    // SAFETY: these are the legacy PIC data ports; masking them here is part of
    // x86 APIC bring-up before interrupts are routed through the IO-APIC/LAPIC.
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
        IS_X2APIC.store(true, Ordering::Relaxed);
    } else {
        info!("Using xAPIC.");
        // SAFETY: on the xAPIC path, `xapic_base` reads the architectural LAPIC
        // base address, which is then mapped before the builder uses it.
        let base_vaddr = iomap_device(pa!(unsafe { xapic_base() } as usize), 0x1000, "lapic")
            .unwrap_or_else(|err| panic!("failed to iomap LAPIC: {err:?}"));
        XAPIC_BASE.store(base_vaddr.as_usize() as u64, Ordering::Relaxed);
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    let mut lapic = builder.build().unwrap();
    // SAFETY: the LAPIC instance was built from either x2APIC CPU state or a
    // freshly mapped xAPIC MMIO base, so enabling it is valid during boot init.
    unsafe {
        lapic.enable();
    }
    install_local_apic(lapic);

    let io_apic_base = iomap_device(io_apic_paddr, 0x1000, "ioapic")
        .unwrap_or_else(|err| panic!("failed to iomap IOAPIC: {err:?}"));
    // SAFETY: `io_apic_base` is the MMIO mapping for the platform IO-APIC and is
    // kept alive for the duration of the constructed controller object.
    let mut io_apic = unsafe { IoApic::new(io_apic_base.as_usize() as u64) };

    // SAFETY: `io_apic` was just created from a valid IO-APIC mapping and is still
    // exclusively owned here while the redirection table is initialized.
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

pub fn init_secondary() {
    let mut lapic = build_local_apic();
    // SAFETY: secondary CPUs build their own LAPIC handle from the APIC mode
    // discovered during primary initialization, then enable the local APIC for
    // the current CPU before publishing that per-CPU handle.
    unsafe { lapic.enable() };
    install_local_apic(lapic);
}

pub fn send_ipi_self(interrupt_id: usize) {
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before self-IPIs
        // are used, and the x2apic crate requires `&mut self` only to model the
        // CPU-local register programming performed here.
        unsafe { lapic.send_ipi_self(interrupt_id as _) };
    });
}

pub fn send_ipi_raw(interrupt_id: usize, target_raw_apic_id: RawCpuId) {
    let apic_id = raw_apic_id(target_raw_apic_id.as_usize() as u8);
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before cross-CPU
        // IPIs are used, and programming the destination APIC ID touches only
        // this CPU's local APIC command register.
        unsafe { lapic.send_ipi(interrupt_id as _, apic_id as _) };
    });
}

pub fn send_ipi_all_but_self(interrupt_id: usize) {
    use x2apic::lapic::IpiAllShorthand;
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before broadcast
        // IPIs are used, and the shorthand command programs only the local ICR.
        unsafe { lapic.send_ipi_all(interrupt_id as _, IpiAllShorthand::AllExcludingSelf) };
    });
}

fn build_local_apic() -> LocalApic {
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if !IS_X2APIC.load(Ordering::Relaxed) {
        let xapic_base = XAPIC_BASE.load(Ordering::Relaxed);
        assert!(xapic_base != 0, "xAPIC base is not initialized");
        builder.set_xapic_base(xapic_base);
    }
    builder.build().expect("failed to build local APIC")
}

fn install_local_apic(lapic: LocalApic) {
    let ptr = Box::into_raw(Box::new(lapic)) as usize;
    // SAFETY: x86_64 per-CPU slots are written through a single `gs` access, and
    // each CPU installs its LAPIC handle exactly once during local APIC bring-up.
    unsafe {
        assert_eq!(LOCAL_APIC_PTR.read_current_raw(), 0);
        LOCAL_APIC_PTR.write_current_raw(ptr);
    }
}

fn local_apic_ptr() -> *mut LocalApic {
    // SAFETY: x86_64 per-CPU slot reads use a single `gs` access, so the value
    // cannot be torn by preemption. The slot stores either 0 or a pointer
    // previously produced by `Box::into_raw` in `install_local_apic`.
    unsafe { LOCAL_APIC_PTR.read_current_raw() as *mut LocalApic }
}
