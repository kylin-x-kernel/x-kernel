//! Early boot and entry stubs for the aarch64 crosvm-virt platform.
use aarch64_cpu::registers::*;
use kplat::memory::{PageAligned, pa};
use page_table::{
    PageTableEntry as GenericPTE, PagingFlags as MappingFlags, aarch64::A64PageEntry as A64PTE,
};

use crate::config::plat::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET};
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L0: PageAligned<[A64PTE; 512]> = PageAligned::new([A64PTE::empty(); 512]);
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L1: PageAligned<[A64PTE; 512]> = PageAligned::new([A64PTE::empty(); 512]);
use crate::serial::{boot_print_str, boot_print_usize};
/// Build the minimal page tables used before the full MMU setup.
unsafe fn init_boot_page_table() {
    boot_print_str("[boot] init boot page table\r\n");
    crate::psci::kvm_guard_granule_init();
    boot_print_str("[boot] kvm xmap pci cam\r\n");
    crate::psci::do_xmap_granules(0x7200_0000, 0x100_0000);
    boot_print_str("[boot] kvm xmap pci mem\r\n");
    crate::psci::do_xmap_granules(0x7000_0000, 0x200_0000);
    boot_print_str("[boot] kvm xmap gicv3 mem\r\n");
    crate::psci::do_xmap_granules(0x3ffb_0000, 0x20_0000);
    boot_print_str("[boot] kvm xmap rtc\r\n");
    crate::psci::do_xmap_granules(0x2000, 0x1000);
    unsafe {
        BOOT_PT_L0[0] = A64PTE::new_table(pa!(&raw mut BOOT_PT_L1 as usize));
        BOOT_PT_L1[0] = A64PTE::new_page(
            pa!(0),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
            true,
        );
        BOOT_PT_L1[1] = A64PTE::new_page(
            pa!(0x4000_0000),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
            true,
        );
        BOOT_PT_L1[2] = A64PTE::new_page(
            pa!(0x8000_0000),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            true,
        );
        BOOT_PT_L1[3] = A64PTE::new_page(
            pa!(0xC000_0000),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            true,
        );
    }
}
#[unsafe(no_mangle)]
/// Boot-time smoke test entry for early debugging.
extern "C" fn kernel_main_test() {
    boot_print_str("[boot] kernel main entered cpu id\r\n");
}
/// Enable FP/SIMD usage if supported by build features.
unsafe fn enable_fp() {
    #[cfg(feature = "fp-simd")]
    kcpu::instrs::enable_fp();
}
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
/// First instruction executed by the primary CPU.
unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!("
         bl       {entry}             // Branch to kernel start, magic
        .space 52, 0
        .inst 0x644d5241
        .space 4, 0
    ",
    entry = sym _start_primary,
    )
}
/// Switch the current exception level to EL1.
unsafe fn switch_to_el1() {
    let current_sp = aarch64_cpu::registers::SP.get();
    SPSel.write(SPSel::SP::ELx);
    aarch64_cpu::registers::SP.set(current_sp);
    let current_el = CurrentEL.read(CurrentEL::EL);
    boot_print_str("[boot] Current el ");
    boot_print_usize(current_el as _);
}
#[unsafe(naked)]
/// Primary CPU boot path: set up stack, MMU, and jump to `kplat::entry`.
unsafe extern "C" fn _start_primary() -> ! {
    core::arch::naked_asm!("
        mrs     x19, mpidr_el1
        and     x19, x19, #0xffffff     // get current CPU id
        mov     x20, x0                 // save DTB pointer
        adrp    x8, {boot_stack}        // setup boot stack
        add     x8, x8, {boot_stack_size}
        mov     sp, x8
        bl      {switch_to_el1}         // switch to EL1
        bl      {enable_fp}             // enable fp/neon
        bl      {init_boot_page_table}
        adrp    x0, {boot_pt}
        bl      {init_mmu}              // setup MMU
        mov     x8, {phys_virt_offset}  // set SP to the high address
        add     sp, sp, x8
        mov     x0, x19                 // call_main(cpu_id, dtb)
        mov     x1, x20
        ldr     x8, ={entry}
        blr     x8
        b .
        ",
        switch_to_el1 = sym switch_to_el1,
        init_boot_page_table = sym init_boot_page_table,
        init_mmu = sym kcpu::boot::init_mmu,
        enable_fp = sym enable_fp,
        boot_pt = sym BOOT_PT_L0,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym kplat::entry,
        boot_stack = sym BOOT_STACK,
        boot_stack_size = const BOOT_STACK_SIZE,
    )
}
#[cfg(feature = "smp")]
#[unsafe(naked)]
/// Secondary CPU boot path for SMP bring-up.
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    core::arch::naked_asm!("
        mrs     x19, mpidr_el1
        and     x19, x19, #0xffffff     // get current CPU id
        mov     sp, x0
        bl      {switch_to_el1}
        bl      {enable_fp}
        adrp    x0, {boot_pt}
        bl      {init_mmu}
        mov     x8, {phys_virt_offset}  // set SP to the high address
        add     sp, sp, x8
        mov     x0, x19                 // call_secondary_main(cpu_id)
        ldr     x8, ={entry}
        blr     x8
        b      .",
        switch_to_el1 = sym kcpu::boot::switch_to_el1,
        init_mmu = sym kcpu::boot::init_mmu,
        enable_fp = sym enable_fp,
        boot_pt = sym BOOT_PT_L0,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym kplat::entry_secondary,
    )
}
