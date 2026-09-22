// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global Descriptor Table (GDT) and Task State Segment (TSS) setup.

use kcpu_slot::{CpuSlot, cpu_slot_cell};
use x86_64::{
    PrivilegeLevel,
    instructions::tables::load_tss,
    registers::segmentation::{CS, DS, ES, SS, Segment, SegmentSelector},
    structures::gdt::Descriptor,
};

// Raw template so trap-entry assembly can reach TSS fields through
// `gs:[offset __KCPU_SLOT_TSS]` without going through a descriptor.
#[unsafe(no_mangle)]
#[used]
#[unsafe(link_section = ".cpu_slot.template")]
pub static __KCPU_SLOT_TSS: x86_64::structures::tss::TaskStateSegment =
    x86_64::structures::tss::TaskStateSegment::new();

/// Typed descriptor over the raw TSS template above.
#[doc(hidden)]
pub static TSS: CpuSlot<x86_64::structures::tss::TaskStateSegment> =
    // SAFETY: the offset function names the raw template above.
    unsafe {
        CpuSlot::from_offset_fn(|| kcpu_slot::__cpu_slot_symbol_offset!(__KCPU_SLOT_TSS))
    };

cpu_slot_cell! {
    static GDT: x86_64::structures::gdt::GlobalDescriptorTable =
        x86_64::structures::gdt::GlobalDescriptorTable::new();
}

/// Kernel code segment for 64-bit mode.
pub const KCODE64: SegmentSelector = SegmentSelector::new(1, PrivilegeLevel::Ring0);
/// Kernel data segment.
pub const KDATA: SegmentSelector = SegmentSelector::new(2, PrivilegeLevel::Ring0);
/// User data segment.
pub const UDATA: SegmentSelector = SegmentSelector::new(3, PrivilegeLevel::Ring3);
/// User code segment for 64-bit mode.
pub const UCODE64: SegmentSelector = SegmentSelector::new(4, PrivilegeLevel::Ring3);

/// Initializes the per-CPU TSS and GDT structures and loads them into the
/// current CPU.
pub(super) fn init() {
    // SAFETY: called during per-CPU initialization (boot or secondary CPU
    // bring-up), single-threaded for this CPU. `GlobalDescriptorTable::load`
    // requires a `'static` reference, so this uses the self-hosting mutable
    // accessor; it is the one x86 raw boundary.
    let gdt = unsafe { GDT.current_shared_mut() };
    assert_eq!(gdt.append(Descriptor::kernel_code_segment()), KCODE64);
    assert_eq!(gdt.append(Descriptor::kernel_data_segment()), KDATA);
    assert_eq!(gdt.append(Descriptor::user_data_segment()), UDATA);
    assert_eq!(gdt.append(Descriptor::user_code_segment()), UCODE64);
    // The TSS template is permanent and `TaskStateSegment: Sync`, so the
    // guard-free shared accessor yields the `&'static` descriptor needs.
    let tss = gdt.append(Descriptor::tss_segment(TSS.current_shared()));
    gdt.load();
    // SAFETY: Segment selectors are compile-time constants verified by `assert_eq!` above. Loading them after `gdt.load()` is valid because the GDT now contains the referenced descriptors.
    unsafe {
        DS::set_reg(KDATA);
        ES::set_reg(KDATA);
        SS::set_reg(KDATA);
        CS::set_reg(KCODE64);
        load_tss(tss);
    }
}
