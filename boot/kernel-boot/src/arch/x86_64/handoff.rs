// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 stub-to-kernel handoff.
//!
//! The assembly stub finishes the low-level CPU transition and enters these
//! functions once the temporary page tables are active. Keeping this boundary
//! separate makes it easier to replace the current monolithic Multiboot path
//! with a Linux-style low-address boot stub later.

use boot_info::{
    BOOT_INFO_MAGIC, BootInfo, BootProtocol, HardwareDescriptionRoot, MemoryDescriptionRoot,
};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kcpu_id_map::RawCpuId;

use super::protocols::{MULTIBOOT_BOOTLOADER_MAGIC, SEV_CBIT_MASK};

static mut X86_BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::Unknown);

unsafe extern "C" {
    static mut x86_ap_boot_gdt_desc: [u8; 6];
    static x86_ap_boot_gdt: u8;
    static mut x86_ap_boot_pml4: [u64; 512];
    static x86_ap_boot_pdpt_low: u8;
    static x86_ap_boot_pdpt_high: u8;
    static mut x86_ap_boot_pdpt_kimage: [u64; 512];
    static mut x86_ap_boot_pd_kimage: [u64; 512];
    static mut x86_ap_boot_pt_kimage: [u64; 512 * 512];
    static _skernel: u8;
    static _ekernel: u8;
}

unsafe fn init_ap_boot_state() {
    const PAGE_SIZE_4K: usize = 0x1000;
    const PTE_PER_PT: usize = 512;

    // SAFETY: AP bootstrap state is initialized once during early boot before
    // any concurrent access to these static handoff tables exists.
    let gdt_desc = unsafe { &mut *core::ptr::addr_of_mut!(x86_ap_boot_gdt_desc) };
    let gdt_paddr = kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_gdt) as usize) as u32;
    gdt_desc[2..6].copy_from_slice(&gdt_paddr.to_le_bytes());

    // SAFETY: AP bootstrap state is initialized once during early boot before
    // any concurrent access to these static handoff tables exists.
    let pml4 = unsafe { &mut *core::ptr::addr_of_mut!(x86_ap_boot_pml4) };
    let pdpt_low = kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_pdpt_low) as usize) as u64;
    let pdpt_high = kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_pdpt_high) as usize) as u64;
    let pdpt_kimage =
        kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_pdpt_kimage) as usize) as u64;
    pml4[0] = pdpt_low | 0x3;
    pml4[256] = pdpt_high | 0x3;
    pml4[511] = pdpt_kimage | 0x3;

    // SAFETY: AP bootstrap state is initialized once during early boot before
    // any concurrent access to these static handoff tables exists.
    let pdpt_kimage = unsafe { &mut *core::ptr::addr_of_mut!(x86_ap_boot_pdpt_kimage) };
    let pd_kimage = kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_pd_kimage) as usize) as u64;
    pdpt_kimage[0] = pd_kimage | 0x3;

    // SAFETY: AP bootstrap state is initialized once during early boot before
    // any concurrent access to these static handoff tables exists.
    let pd_kimage = unsafe { &mut *core::ptr::addr_of_mut!(x86_ap_boot_pd_kimage) };
    // SAFETY: AP bootstrap state is initialized once during early boot before
    // any concurrent access to these static handoff tables exists.
    let pt_kimage = unsafe { &mut *core::ptr::addr_of_mut!(x86_ap_boot_pt_kimage) };
    pd_kimage.fill(0);
    pt_kimage.fill(0);

    let kernel_start = kaddr_layout::v2p(core::ptr::addr_of!(_skernel) as usize);
    let kernel_end = kaddr_layout::v2p(core::ptr::addr_of!(_ekernel) as usize);
    let kernel_pages = (kernel_end - kernel_start).div_ceil(PAGE_SIZE_4K);
    let pt_count = kernel_pages.div_ceil(PTE_PER_PT);
    assert!(
        pt_count <= pd_kimage.len(),
        "AP bootstrap page table overflow"
    );

    let pt_base = kaddr_layout::v2p(core::ptr::addr_of!(x86_ap_boot_pt_kimage) as usize);
    for (idx, entry) in pd_kimage.iter_mut().take(pt_count).enumerate() {
        *entry = (pt_base + idx * PAGE_SIZE_4K) as u64 | 0x3;
    }

    let mut page_paddr = kernel_start as u64;
    for entry in pt_kimage.iter_mut().take(kernel_pages) {
        *entry = page_paddr | 0x3 | SEV_CBIT_MASK;
        page_paddr += PAGE_SIZE_4K as u64;
    }
}

/// Read the x2APIC ID from CPUID leaf 0x0B (Extended Topology).
/// Falls back to the initial APIC ID from leaf 1 if leaf 0x0B is not supported.
fn get_cpu_id() -> RawCpuId {
    let cpuid = raw_cpuid::CpuId::new();
    if let Some(level) = cpuid
        .get_extended_topology_info()
        .and_then(|mut t| t.next())
    {
        let id = level.x2apic_id();
        if id != 0 {
            return RawCpuId::new(id as usize);
        }
    }
    RawCpuId::new(
        cpuid
            .get_feature_info()
            .map(|f| f.initial_local_apic_id() as usize)
            .unwrap_or(0),
    )
}

#[unsafe(no_mangle)]
/// # Safety
///
/// This is the raw x86_64 handoff entry from the temporary assembly stub. The
/// caller must provide either a valid Multiboot magic and handoff payload or a
/// valid [`BOOT_INFO_MAGIC`] payload, and the temporary boot mappings must keep
/// the referenced data live until control is transferred onward.
pub(super) unsafe extern "C" fn rust_entry(magic: usize, mbi: usize, handoff_arg: usize) {
    if magic == MULTIBOOT_BOOTLOADER_MAGIC {
        let kimage_voffset = handoff_arg;
        kaddr_layout::set_kimage_voffset(kimage_voffset);
        // SAFETY: early boot performs the one-time AP bootstrap table setup here.
        unsafe { init_ap_boot_state() };
        let logical_cpu_id = kcpu_id_map::logical_cpu_id(get_cpu_id())
            .unwrap_or_else(|| panic!("missing logical cpu id mapping for boot cpu"));
        let kernel_load_paddr = KIMAGE_VADDR - kimage_voffset;
        // SAFETY: primary boot CPU performs one-time initialization of the global
        // x86 boot-info structure before entering the generic kernel.
        unsafe {
            X86_BOOT_INFO = BootInfo::new(BootProtocol::Multiboot2)
                .with_memory_description_root(MemoryDescriptionRoot::X86BootProtocol)
                .with_hardware_description_root(HardwareDescriptionRoot::None)
                .with_protocol_info_addr(mbi)
                .with_kernel_load_paddr(kernel_load_paddr)
                .with_phys_virt_offset(PAGE_OFFSET)
                .with_boot_console_ioport(kbuild_config::BOOT_CONSOLE_ADDR as u16)
                .with_cpu_id(logical_cpu_id)
                .with_cpu_count(kcpu_id_map::nr_cpus());
        }
        let boot_info_ptr = core::ptr::addr_of!(X86_BOOT_INFO) as usize;
        crate::PrimaryKernelEntry::enter(boot_info_ptr)
    } else if magic as u64 == BOOT_INFO_MAGIC {
        // SAFETY: `mbi` is the bootloader-provided pointer to a live `BootInfo`
        // structure passed in the boot protocol handoff.
        let boot_info = unsafe { &*(mbi as *const BootInfo) };
        assert!(boot_info.is_valid(), "invalid boot info");
        kaddr_layout::set_kimage_voffset(KIMAGE_VADDR - boot_info.kernel_load_paddr);
        kcpu_id_map::init_boot_cpu_id_map(boot_info.rsdp_addr);
        // SAFETY: early boot performs the one-time AP bootstrap table setup here.
        unsafe { init_ap_boot_state() };
        crate::PrimaryKernelEntry::enter(mbi)
    }
    loop {
        // SAFETY: if boot handoff returns unexpectedly, halting the CPU avoids
        // executing past the bootstrap path.
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) }
    }
}

#[unsafe(no_mangle)]
/// # Safety
///
/// This is the raw secondary-CPU x86_64 handoff entry. The caller must enter
/// with the temporary AP bootstrap environment established and use the
/// supported secondary magic for this path.
pub(super) unsafe extern "C" fn rust_entry_secondary(magic: usize) {
    if magic == MULTIBOOT_BOOTLOADER_MAGIC {
        let raw_cpu_id = get_cpu_id();
        let logical_cpu_id = kcpu_id_map::logical_cpu_id(raw_cpu_id).unwrap_or_else(|| {
            panic!(
                "missing logical cpu id mapping for raw cpu id {}",
                raw_cpu_id.as_usize()
            )
        });
        crate::SecondaryKernelEntry::enter(logical_cpu_id)
    }
    loop {
        // SAFETY: if the secondary-entry handoff returns unexpectedly, halting
        // the CPU avoids executing past the bootstrap path.
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) }
    }
}
