// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![cfg(target_arch = "x86_64")]

#[macro_use]
extern crate log;

use core::mem::MaybeUninit;

use kcpu_id_map::RawCpuId;
use kspin::SpinNoIrq;
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

static mut LOCAL_APIC: MaybeUninit<LocalApic> = MaybeUninit::uninit();
static mut IS_X2APIC: bool = false;
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();
static MSIX_VECTOR_ALLOCATOR: SpinNoIrq<MsixVectorAllocator> =
    SpinNoIrq::new(MsixVectorAllocator::new());

const MSIX_VECTOR_COUNT: usize = (APIC_TIMER_VECTOR as usize) - (MSIX_VECTOR_BASE as usize);
const MSIX_VECTOR_WORD_BITS: usize = u64::BITS as usize;
const MSIX_VECTOR_WORDS: usize = MSIX_VECTOR_COUNT.div_ceil(MSIX_VECTOR_WORD_BITS);

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

pub fn end_of_interrupt() {
    unsafe { local_apic().end_of_interrupt() };
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
        let base_vaddr = iomap_device(pa!(unsafe { xapic_base() } as usize), 0x1000, "lapic")
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

pub fn init_secondary() {
    unsafe { local_apic().enable() };
}

pub fn send_ipi_self(interrupt_id: usize) {
    unsafe {
        local_apic().send_ipi_self(interrupt_id as _);
    }
}

pub fn send_ipi_raw(interrupt_id: usize, target_raw_apic_id: RawCpuId) {
    let apic_id = raw_apic_id(target_raw_apic_id.as_usize() as u8);
    unsafe {
        local_apic().send_ipi(interrupt_id as _, apic_id as _);
    };
}

pub fn send_ipi_all_but_self(interrupt_id: usize) {
    use x2apic::lapic::IpiAllShorthand;
    unsafe {
        local_apic().send_ipi_all(interrupt_id as _, IpiAllShorthand::AllExcludingSelf);
    };
}
