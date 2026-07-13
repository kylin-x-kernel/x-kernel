// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::arch::naked_asm;

use boot_info::{BootInfo, BootProtocol, HardwareDescriptionRoot, MemoryDescriptionRoot};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::{BOOT_CONSOLE_ADDR, BOOT_STACK_SIZE};
use kcpu_id_map::RawCpuId;

use super::{BOOT_DMW_BASE, BOOT_DMW_UNCACHED_BASE};

const DIRECT_BOOT_LOAD_OFFSET: usize = 0x0020_0000;

#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

static mut LOONGARCH_BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::Uefi);

#[unsafe(link_section = ".text.boot")]
fn enable_fp_simd() {
    #[cfg(feature = "fp-simd")]
    {
        karch::enable_fp();
        karch::enable_lsx();
    }
}

/// # Safety
///
/// This is the raw firmware entry point for the boot CPU. The caller must
/// provide the architecture-defined boot register state and execute this entry
/// exactly once for the primary CPU before normal Rust invariants exist.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
        .word   0x5a4d
        .word   0
        .dword  {kernel_entry}
        .dword  _ekernel - _skernel
        .dword  {kernel_load_offset}
        .dword  0
        .dword  0
        .dword  0
        .word   0x818223cd
        .word   0x0
        move        $s0, $a1
        move        $s1, $a2
        li.d        $t0, {dmw_uncached_base} | 0x1
        csrwr       $t0, 0x180
        li.d        $t0, {phys_boot_offset} | 0x11
        csrwr       $t0, 0x181
        la.local    $t0, 1f
        li.d        $t1, {phys_boot_offset}
        or          $t0, $t0, $t1
        jirl        $zero, $t0, 0
    1:
        la.local    $sp, {boot_stack}
        li.d        $t0, {boot_stack_size}
        add.d       $sp, $sp, $t0
        la.local    $s2, {kernel_start}
        li.d        $t0, {phys_boot_offset}
        sub.d       $s2, $s2, $t0
        move        $a0, $s2
        bl          {boot_stage_banner}
        bl          {enable_fp_simd}
        move        $a0, $s2
        move        $a1, $s0
        move        $a2, $s1
        bl          {create_boot_page_tables}
        li.w        $a0, 1
        bl          {boot_stage_checkpoint}
        bl          {init_mmu}
        li.w        $a0, 2
        bl          {boot_stage_checkpoint}
        li.d        $s3, {kimage_vaddr}
        sub.d       $s3, $s3, $s2
        li.d        $t0, {phys_boot_offset}
        sub.d       $t0, $s3, $t0
        add.d       $sp, $sp, $t0
        li.w        $a0, 3
        bl          {boot_stage_checkpoint}
        csrrd       $a0, 0x20
        move        $a1, $s0
        move        $a2, $s1
        move        $a3, $s3
        la.abs      $t0, {entry}
        li.d        $ra, 0
        jirl        $zero, $t0, 0",
        kernel_entry = const DIRECT_BOOT_LOAD_OFFSET + 0x40,
        kernel_load_offset = const DIRECT_BOOT_LOAD_OFFSET,
        dmw_uncached_base = const BOOT_DMW_UNCACHED_BASE,
        phys_boot_offset = const BOOT_DMW_BASE,
        boot_stack = sym BOOT_STACK,
        boot_stack_size = const BOOT_STACK_SIZE,
        kernel_start = sym _start,
        boot_stage_banner = sym super::serial::boot_stage_banner,
        boot_stage_checkpoint = sym super::serial::boot_stage_checkpoint,
        enable_fp_simd = sym enable_fp_simd,
        create_boot_page_tables = sym super::mmu::create_boot_page_tables,
        init_mmu = sym super::mmu::init_mmu,
        kimage_vaddr = const KIMAGE_VADDR,
        entry = sym __primary_switched,
    )
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
/// # Safety
///
/// This is the raw secondary-CPU entry point reached directly from firmware/boot
/// handoff before normal Rust invariants exist. Callers must enter with the
/// architecture-defined secondary boot state and transfer control exactly once.
pub unsafe extern "C" fn _start_secondary() -> ! {
    naked_asm!(
        "
        li.d        $t0, {dmw_uncached_base} | 0x1
        csrwr       $t0, 0x180
        li.d        $t0, {phys_boot_offset} | 0x11
        csrwr       $t0, 0x181
        li.w        $t0,  0x1028
        iocsrrd.d   $sp,  $t0
        bl          {enable_fp_simd}
        bl          {init_mmu}
        la.local    $s0, {kernel_start}
        li.d        $t0, {phys_boot_offset}
        sub.d       $s0, $s0, $t0
        li.d        $t0, {kimage_vaddr}
        sub.d       $s0, $t0, $s0
        li.d        $t0, {phys_boot_offset}
        sub.d       $t0, $s0, $t0
        add.d       $sp, $sp, $t0
        csrrd       $a0, 0x20
        la.abs      $t0, {entry}
        jirl        $zero, $t0, 0",
        dmw_uncached_base = const BOOT_DMW_UNCACHED_BASE,
        phys_boot_offset = const BOOT_DMW_BASE,
        enable_fp_simd = sym enable_fp_simd,
        init_mmu = sym super::mmu::init_mmu,
        kernel_start = sym _start,
        kimage_vaddr = const KIMAGE_VADDR,
        entry = sym __secondary_switched,
    )
}

/// # Safety
///
/// Must only be entered from [`_start_secondary`] after the secondary CPU has
/// enabled the boot mappings and adjusted its stack into the kernel virtual
/// address space.
pub unsafe extern "C" fn __secondary_switched(raw_cpu_id: RawCpuId) -> ! {
    let logical_cpu_id = kcpu_id_map::logical_cpu_id(raw_cpu_id).unwrap_or_else(|| {
        panic!(
            "missing logical cpu id mapping for raw cpu id {:#x}",
            raw_cpu_id.as_usize()
        )
    });
    crate::SecondaryKernelEntry::enter(logical_cpu_id)
}

fn cmdline_len(cmdline_paddr: usize) -> usize {
    if cmdline_paddr == 0 {
        return 0;
    }
    let cmdline = kaddr_layout::p2v(cmdline_paddr) as *const u8;
    let mut len = 0usize;
    // SAFETY: `cmdline_paddr` comes from firmware handoff; after `p2v` it
    // points to a readable NUL-terminated command-line buffer during early boot.
    unsafe {
        while len < 4096 {
            if cmdline.add(len).read_volatile() == 0 {
                break;
            }
            len += 1;
        }
    }
    len
}

/// # Safety
///
/// Must only be entered from [`_start`] after the boot mappings are active and
/// `kimage_voffset` matches the current kernel image mapping. The firmware
/// pointers passed in must still refer to the current boot handoff objects.
pub unsafe extern "C" fn __primary_switched(
    raw_cpu_id: RawCpuId,
    cmdline_paddr: usize,
    systemtable_paddr: usize,
    kimage_voffset: usize,
) -> ! {
    unsafe extern "C" {
        fn _sbss();
        fn _ebss();
    }

    // SAFETY: `_sbss.._ebss` is the linker-defined BSS range for this boot
    // image, and early boot is its only writer before runtime init.
    unsafe {
        let bss_start = _sbss as *const () as usize;
        let bss_end = _ebss as *const () as usize;
        core::slice::from_raw_parts_mut(bss_start as *mut u8, bss_end - bss_start).fill(0);
    }

    kaddr_layout::set_kimage_voffset(kimage_voffset);
    super::serial::activate_linear_map();

    let kernel_load_paddr = KIMAGE_VADDR - kimage_voffset;
    let cmdline_len = cmdline_len(cmdline_paddr);
    // SAFETY: boot MMU setup cached the DTB/RSDP firmware table addresses in
    // boot globals that remain valid until kernel handoff.
    let (dtb_paddr, rsdp_paddr) = unsafe { super::mmu::boot_firmware_tables() };
    kcpu_id_map::init_boot_cpu_id_map(dtb_paddr);
    let logical_cpu_id = kcpu_id_map::logical_cpu_id(raw_cpu_id).unwrap_or_else(|| {
        panic!(
            "missing logical cpu id mapping for raw cpu id {:#x}",
            raw_cpu_id.as_usize()
        )
    });
    let cpu_id = logical_cpu_id.as_usize();
    // SAFETY: boot MMU setup cached the EFI memmap physical address in a boot
    // global that remains valid until kernel handoff.
    let uefi_memmap_paddr = unsafe { super::mmu::boot_uefi_memmap_paddr() };
    let uefi_memmap_vaddr = if uefi_memmap_paddr == 0 {
        0
    } else {
        kaddr_layout::p2v(uefi_memmap_paddr)
    };
    let memory_root = if uefi_memmap_paddr != 0 {
        MemoryDescriptionRoot::UefiMemmap
    } else if dtb_paddr != 0 {
        MemoryDescriptionRoot::DeviceTree
    } else {
        panic!("loongarch UEFI handoff missing both EFI memmap and DTB");
    };
    let hardware_root = if dtb_paddr != 0 {
        HardwareDescriptionRoot::DeviceTree
    } else if rsdp_paddr != 0 {
        HardwareDescriptionRoot::Acpi
    } else {
        HardwareDescriptionRoot::None
    };
    // SAFETY: primary boot CPU performs one-time initialization of the global
    // boot-info structure after BSS clear and before kernel entry.
    unsafe {
        LOONGARCH_BOOT_INFO = BootInfo::new(BootProtocol::Uefi)
            .with_memory_description_root(memory_root)
            .with_hardware_description_root(hardware_root)
            .with_protocol_info_addr(systemtable_paddr)
            .with_kernel_load_paddr(kernel_load_paddr)
            .with_phys_virt_offset(PAGE_OFFSET)
            .with_dtb(dtb_paddr, BOOT_DMW_UNCACHED_BASE + dtb_paddr)
            .with_uefi_memmap(uefi_memmap_paddr, uefi_memmap_vaddr)
            .with_rsdp(rsdp_paddr)
            .with_cmdline(cmdline_paddr, cmdline_len)
            .with_boot_console_mmio(
                BOOT_CONSOLE_ADDR,
                0x1000,
                kaddr_layout::p2v(BOOT_CONSOLE_ADDR),
            )
            .with_cpu_id(logical_cpu_id)
            .with_cpu_count(kcpu_id_map::nr_cpus());
    }

    crate::bootln!(
        "loongarch primary switched cpu={} systab={:#x} dtb={:#x} memmap={:#x} rsdp={:#x} \
         kimage_voffset={:#x}",
        cpu_id,
        systemtable_paddr,
        dtb_paddr,
        uefi_memmap_paddr,
        rsdp_paddr,
        kimage_voffset
    );
    let boot_info_ptr = core::ptr::addr_of!(LOONGARCH_BOOT_INFO) as usize;
    crate::PrimaryKernelEntry::enter(boot_info_ptr)
}
