// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 position-independent boot entry.
//!
//! Boot flow
//! ---------
//! ```text
//! _start  (.head.text)
//!   └─ primary_entry  (.idmap.text)
//!        ├─ preserve_boot_args()   – save x0-x3 (DTB, …) via adrp
//!        ├─ switch_to_el1()        – EL3/EL2 → EL1 transition
//!        ├─ enable_fp()            – enable FP/SIMD
//!        ├─ create_boot_page_tables() – build idmap + kernel high map
//!        ├─ init_mmu()             – set MAIR/TCR/TTBR, enable MMU
//!        └─ __primary_switched()  (virtual address)
//!             ├─ zero BSS
//!             └─ kplat::entry(cpu_id, boot_info)
//! ```

use core::arch::naked_asm;

use boot_info::{BootInfo, BootProtocol, HardwareDescriptionRoot, MemoryDescriptionRoot};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::BOOT_STACK_SIZE;
use kcpu_id_map::{LogicalCpuId, RawCpuId, raw_cpu_id};

use super::{el, mmu, serial};

// Linux ARM64 Boot Protocol image flags.
const FLAG_LE: usize = 0b0;
const FLAG_PAGE_SIZE_4K: usize = 0b10;
const FLAG_ANY_MEM: usize = 0b1000;

/// Boot stack for the primary CPU.
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

/// Storage for the boot arguments passed in x0-x3 by firmware/bootloader.
#[unsafe(link_section = ".data")]
pub(super) static mut SAVED_BOOT_ARGS: [u64; 4] = [0; 4];

/// Unified boot info passed from the AArch64 boot entry into the kernel.
static mut AARCH64_BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::DeviceTree);

#[unsafe(link_section = ".data")]
static mut SECONDARY_BOOT_CONTEXT: SecondaryBootContext = SecondaryBootContext::new();

#[derive(Clone, Copy)]
#[repr(C)]
struct SecondaryBootContext {
    target_raw_cpu_id: usize,
    stack_top_paddr: usize,
}

impl SecondaryBootContext {
    const INVALID_RAW_CPU_ID: usize = usize::MAX;

    const fn new() -> Self {
        Self {
            target_raw_cpu_id: Self::INVALID_RAW_CPU_ID,
            stack_top_paddr: 0,
        }
    }
}

/// Records the target raw CPU id and physical boot stack for the next
/// secondary AArch64 CPU release.
pub fn set_secondary_boot_context(logical_cpu_id: LogicalCpuId, stack_top_paddr: usize) {
    let target_raw_cpu_id = raw_cpu_id(logical_cpu_id).unwrap_or_else(|| {
        panic!(
            "missing raw CPU id mapping for logical CPU {}",
            logical_cpu_id.as_usize()
        )
    });
    let ctx = SecondaryBootContext {
        target_raw_cpu_id: target_raw_cpu_id.as_usize(),
        stack_top_paddr,
    };

    // SAFETY: Secondary CPU bring-up is serialized by `start_secondary_cpus`,
    // so the boot CPU is the only writer. The secondary entry reads this
    // context only after the boot CPU fully populates it and issues `CPU_ON`.
    unsafe {
        SECONDARY_BOOT_CONTEXT = ctx;
    }
    karch::clean_dcache_range_to_poc(
        memaddr::VirtAddr::from(core::ptr::addr_of!(SECONDARY_BOOT_CONTEXT) as usize),
        core::mem::size_of::<SecondaryBootContext>(),
    );
}

/// Linux ARM64 Boot Protocol header followed by a branch to `primary_entry`.
///
/// # Safety
///
/// This is the raw firmware entry point. The caller must enter with the Linux
/// ARM64 boot protocol register state, a valid early execution context, and
/// control must transfer here exactly once for the boot CPU.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".head.text")]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "add     x13, x18, #0x16",         // "MZ" magic (valid ARM64 no-op instruction)
        "bl      {entry}",                 // branch to kernel start
        ".quad   0",                       // image load offset from RAM base (little-endian)
        ".quad   _ekernel - _start",       // effective image size
        ".quad   {flags}",                 // kernel flags
        ".quad   0",                       // reserved
        ".quad   0",                       // reserved
        ".quad   0",                       // reserved
        ".ascii  \"ARM\\x64\"",            // magic number
        ".long   0",                       // reserved (PE COFF offset)
        flags = const FLAG_LE | FLAG_PAGE_SIZE_4K | FLAG_ANY_MEM,
        entry = sym primary_entry,
    )
}

/// Primary CPU early boot entry (runs before MMU is enabled).
///
/// All code here is position-independent – only PC-relative addressing is
/// used for data, except for `ldr x8, =sym` literal-pool loads which
/// intentionally load the *linked* virtual address so that the `br x8`
/// after MMU-enable jumps to the correct high-virtual-address symbol.
///
/// # Safety
///
/// This function is entered directly from the raw boot header before normal
/// Rust invariants exist. The caller must provide the architecture-defined
/// boot register state, and the code it calls must remain valid until the MMU
/// handoff completes.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn primary_entry() -> ! {
    naked_asm!(
        // Capture the current CPU MPIDR affinity value and the DTB pointer
        // before any call clobbers them. The logical runtime CPU id is derived
        // later from the DT CPU order rather than assuming MPIDR == cpu_id.
        "mrs     x19, mpidr_el1",
        "and     x21, x19, #0xffffff",   // Aff2|Aff1|Aff0
        "ubfx    x19, x19, #32, #8",     // Aff3
        "orr     x19, x21, x19, lsl #32",
        "mov     x20, x0",               // save DTB physical address

        // Save firmware boot arguments (x0-x3) to SAVED_BOOT_ARGS via adrp.
        "bl      {preserve_boot_args}",

        // Set up the early boot stack using PC-relative addressing.
        "adrp    x8, {boot_stack}",
        "add     x8, x8, :lo12:{boot_stack}",
        "add     x8, x8, {boot_stack_size}",
        "mov     sp, x8",

        // EL initialization: VHE (stay in EL2) or drop to EL1.
        "bl      {el_init}",

        // Enable FP/SIMD so that Rust code can use float registers.
        "bl      {enable_fp}",

        // Build the two-level boot page tables (idmap + kernel high map).
        "bl      {create_boot_page_tables}",

        // Program MAIR/TCR/TTBR and enable the MMU.
        "bl      {init_mmu}",

        // Switch the stack pointer to its high virtual address at KIMAGE_VADDR.
        // The boot stack lives inside the kernel image, so its virtual address is:
        //   SP_virt = SP_phys + (KIMAGE_VADDR - PA(_start))
        // Compute PA(_start) via adrp and derive the adjustment.
        "adrp    x8, {kernel_start}",          // x8 = PA(_start), page-aligned
        "ldr     x9, ={kimage_vaddr}",          // x9 = KIMAGE_VADDR (compile-time const)
        "sub     x8, x9, x8",                  // x8 = KIMAGE_VADDR - PA(_start) = kimage_voffset
        "add     sp, sp, x8",

        // Restore raw CPU id, DTB, and pass kimage_voffset for __primary_switched.
        "mov     x0, x19",
        "mov     x1, x20",
        "mov     x2, x8",                      // x2 = kimage_voffset

        // Jump to the virtual address of __primary_switched.
        // `ldr x8, =sym` loads the *linked* VMA from the literal pool so
        // that the branch targets the high-VA mapping set up above.
        "ldr     x8, ={primary_switched}",
        "blr     x8",
        "b .",

        preserve_boot_args      = sym preserve_boot_args,
        boot_stack              = sym BOOT_STACK,
        boot_stack_size         = const BOOT_STACK_SIZE,
        el_init                 = sym el_init,
        enable_fp               = sym enable_fp,
        create_boot_page_tables = sym mmu::create_boot_page_tables,
        init_mmu                = sym mmu::init_mmu,
        kernel_start            = sym _start,
        kimage_vaddr            = const KIMAGE_VADDR,
        primary_switched        = sym __primary_switched,
    )
}

/// Save x0-x3 (firmware boot arguments) to [`SAVED_BOOT_ARGS`].
///
/// Uses PC-relative addressing so this can run before the MMU is on.
///
/// # Safety
///
/// The caller must invoke this only during early boot while `x0..x3` still
/// contain the original firmware handoff arguments for the current CPU.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn preserve_boot_args() {
    naked_asm!(
        // Get the physical address of SAVED_BOOT_ARGS via adrp/add.
        "adrp    x8, {saved_args}",
        "add     x8, x8, :lo12:{saved_args}",
        // Store x0..x3.
        "stp     x0, x1, [x8]",
        "stp     x2, x3, [x8, #16]",
        // Full system barrier so the stores complete before the MMU is enabled.
        "dmb     sy",
        "ret",
        saved_args = sym SAVED_BOOT_ARGS,
    )
}

/// Secondary CPU boot entry.
///
/// Called from firmware on a secondary CPU.
///
/// # Safety
///
/// Must only be called from secondary CPUs.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn _start_secondary() -> ! {
    naked_asm!(
        "mrs     x19, mpidr_el1",
        "and     x20, x19, #0xffffff",   // Aff2|Aff1|Aff0
        "ubfx    x19, x19, #32, #8",     // Aff3
        "orr     x19, x20, x19, lsl #32",
        // The boot CPU prepares a single shared release context before each
        // serialized PSCI `CPU_ON`. The released secondary CPU validates that
        // the context targets its raw MPIDR and then takes the prepared stack.
        "adrp    x22, {secondary_boot_context}",
        "add     x22, x22, :lo12:{secondary_boot_context}",
        "ldp     x23, x24, [x22]",
        "cmp     x23, x19",
        "b.ne    2f",
        "cbz     x24, 2f",
        "mov     sp, x24",
        "bl      {el_init}",
        "bl      {enable_fp}",
        "bl      {init_mmu}",
        // Adjust SP to KIMAGE_VADDR range (same as primary_entry):
        //   kimage_voffset = KIMAGE_VADDR - PA(_start)
        // Secondary boot stacks are in .bss.stack (kernel image, mapped at KIMAGE_VADDR).
        // The runtime page table maps kernel image PAs only at KIMAGE_VADDR, not at
        // PAGE_OFFSET+PA, so we must use kimage_voffset here to keep SP valid
        // after init_memory_management_secondary() switches to the runtime page table.
        "adrp    x8, {kernel_start}",
        "ldr     x9, ={kimage_vaddr}",
        "sub     x8, x9, x8",           // x8 = kimage_voffset
        "add     sp, sp, x8",
        "mov     x0, x19",               // raw cpu id
        "ldr     x8, ={entry_secondary}",
        "br      x8",
        "2:",
        "wfe",
        "b       2b",
        secondary_boot_context = sym SECONDARY_BOOT_CONTEXT,
        el_init          = sym el_init,
        enable_fp        = sym enable_fp,
        init_mmu         = sym mmu::init_mmu,
        kernel_start     = sym _start,
        kimage_vaddr     = const KIMAGE_VADDR,
        entry_secondary  = sym __secondary_switched,
    )
}

/// # Safety
///
/// Must only be entered from [`_start_secondary`] after the secondary CPU has
/// switched to the boot page tables and established a valid virtual stack.
pub unsafe extern "C" fn __secondary_switched(raw_cpu_id: RawCpuId) {
    let logical_cpu_id = kcpu_id_map::logical_cpu_id(raw_cpu_id).unwrap_or_else(|| {
        panic!(
            "missing logical cpu id mapping for raw cpu id {:#x}",
            raw_cpu_id.as_usize()
        )
    });
    call_kernel_entry!(SECOND_KERNEL_ENTRY, logical_cpu_id)
}

/// Post-MMU entry point – runs at the kernel's high virtual address.
///
/// Receives `kimage_voffset = KIMAGE_VADDR - PA(_start)` computed in
/// `primary_entry` and stores it in `kaddr_layout` for later use by the memory
/// subsystem (v2p / p2v for kernel-image symbols).
///
/// Zeroes BSS, then constructs a [`BootInfo`] payload and calls
/// [`kplat::entry`] with the boot CPU id and boot info pointer.
///
/// # Safety
///
/// Must only be called once, from [`primary_entry`], after the MMU has been
/// enabled and the stack pointer adjusted to a virtual address.
pub unsafe extern "C" fn __primary_switched(
    cpu_mpidr: usize,
    dtb_paddr: usize,
    kimage_voffset: usize,
) {
    // Zero BSS before setting any global state, so the AtomicUsize storing
    // kimage_voffset (which lives in .bss) is cleared first and not
    // overwritten by the fill below.
    unsafe extern "C" {
        fn _sbss();
        fn _ebss();
    }
    // SAFETY: `_sbss.._ebss` is the linker-defined BSS range for the current
    // image, and zeroing it here is the first mutable access after early entry.
    unsafe {
        let bss_start = _sbss as *const () as usize;
        let bss_end = _ebss as *const () as usize;
        core::slice::from_raw_parts_mut(bss_start as *mut u8, bss_end - bss_start).fill(0);
    }

    // Store the runtime VA-to-PA offset now that BSS is clean.  All
    // subsequent v2p()/p2v() calls on kernel-image symbols depend on this.
    kaddr_layout::set_kimage_voffset(kimage_voffset);

    kcpu_id_map::init_boot_cpu_id_map(dtb_paddr);

    let logical_cpu_id = kcpu_id_map::logical_cpu_id(RawCpuId::new(cpu_mpidr))
        .unwrap_or_else(|| panic!("missing logical cpu id mapping for raw cpu id {cpu_mpidr:#x}"));

    let kernel_load_paddr = KIMAGE_VADDR - kimage_voffset;
    // SAFETY: primary boot CPU performs one-time initialization of the global
    // boot-info structure before handing control to the generic runtime.
    unsafe {
        AARCH64_BOOT_INFO = BootInfo::new(BootProtocol::DeviceTree)
            .with_memory_description_root(MemoryDescriptionRoot::DeviceTree)
            .with_hardware_description_root(HardwareDescriptionRoot::DeviceTree)
            .with_protocol_info_addr(dtb_paddr)
            .with_kernel_load_paddr(kernel_load_paddr)
            .with_phys_virt_offset(PAGE_OFFSET)
            .with_dtb(dtb_paddr, kaddr_layout::p2v(dtb_paddr))
            .with_boot_console_mmio(
                kbuild_config::BOOT_CONSOLE_ADDR,
                0x1000,
                serial::BOOT_UART_BOOT_VADDR,
            )
            .with_cpu_id(logical_cpu_id)
            .with_cpu_count(kbuild_config::CPU_NUM);
    }
    crate::bootln!(
        "entered primary switched cpu={} mpidr={:#x} dtb={:#x} kimage_voffset={:#x}",
        logical_cpu_id.as_usize(),
        cpu_mpidr,
        dtb_paddr,
        kimage_voffset
    );
    super::mmu::extend_boot_linear_ram_from_dtb(dtb_paddr);
    crate::bootln!("boot linear RAM map extended from DT");
    let boot_info_ptr = core::ptr::addr_of!(AARCH64_BOOT_INFO) as usize;
    crate::bootln!("handoff to kruntime boot_info={boot_info_ptr:#x}");
    call_kernel_entry!(PRIMARY_KERNEL_ENTRY, boot_info_ptr)
}

/// Enable FP/SIMD by clearing traps in `CPACR_EL1`.
#[unsafe(link_section = ".idmap.text")]
fn enable_fp() {
    #[cfg(feature = "fp-simd")]
    karch::enable_fp();
}

/// Exception-level initialization for early boot.
///
/// With the `vmm` feature, stays in EL2 with VHE (E2H=1, TGE=1).
/// Without it, drops from EL2/EL3 to EL1 via `switch_to_el1`.
///
/// # Safety
///
/// Must only be called during early boot with the expected EL2/EL3
/// register state, before normal kernel execution begins.
#[unsafe(link_section = ".idmap.text")]
unsafe fn el_init() {
    #[cfg(feature = "vmm")]
    // SAFETY: caller guarantees early-boot context with EL2 entry state.
    unsafe {
        el::init_el2_vhe();
    }
    #[cfg(not(feature = "vmm"))]
    // SAFETY: caller guarantees early-boot context with EL2/EL3 entry state.
    unsafe {
        el::switch_to_el1();
    }
}
