// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Intel VMX constants, VMCS field encodings, and inline helpers.

/// VMX MSR addresses.
pub const MSR_IA32_FEATURE_CONTROL: u32 = 0x0000_003a;
pub const MSR_IA32_VMX_BASIC: u32 = 0x0000_0480;
pub const MSR_IA32_VMX_PINBASED_CTLS: u32 = 0x0000_0481;
pub const MSR_IA32_VMX_PROCBASED_CTLS: u32 = 0x0000_0482;
pub const MSR_IA32_VMX_EXIT_CTLS: u32 = 0x0000_0483;
pub const MSR_IA32_VMX_ENTRY_CTLS: u32 = 0x0000_0484;
pub const MSR_IA32_VMX_CR0_FIXED0: u32 = 0x0000_0486;
pub const MSR_IA32_VMX_CR0_FIXED1: u32 = 0x0000_0487;
pub const MSR_IA32_VMX_CR4_FIXED0: u32 = 0x0000_0488;
pub const MSR_IA32_VMX_CR4_FIXED1: u32 = 0x0000_0489;
pub const MSR_IA32_VMX_TRUE_PIN: u32 = 0x0000_048d;
pub const MSR_IA32_VMX_TRUE_PROC: u32 = 0x0000_048e;
pub const MSR_IA32_VMX_TRUE_EXIT: u32 = 0x0000_048f;
pub const MSR_IA32_VMX_TRUE_ENTRY: u32 = 0x0000_0490;
pub const MSR_IA32_VMX_PROCBASED_CTLS2: u32 = 0x0000_048b;
pub const MSR_EFER: u32 = 0xc000_0080;

/// MSR addresses for FS/GS base.
const MSR_FS_BASE: u32 = 0xC000_0100;
pub const MSR_GS_BASE: u32 = 0xC000_0101;

/// CR4 bits.
pub const X86_CR4_VMXE: u64 = 0x0000_2000;

/// RFLAGS bits.
pub const X86_EFLAGS_CF: u64 = 0x0000_0001;
pub const X86_EFLAGS_ZF: u64 = 0x0000_0040;

/// Pin-based control bits.
pub const PIN_EXTERNAL_INT_EXIT: u32 = 1 << 0;
pub const PIN_NMI: u32 = 1 << 3;
pub const PIN_VIRT_NMI: u32 = 1 << 5;

/// Primary CPU-based control bits.
pub const CPU_HLT: u32 = 1 << 7;
pub const CPU_ACTIVATE_SECONDARY: u32 = 1 << 31;

/// VM-Exit control bits.
pub const EXI_HOST_64: u32 = 1 << 9;
pub const EXI_SAVE_EFER: u32 = 1 << 20;
pub const EXI_LOAD_EFER: u32 = 1 << 21;

/// VM-Entry control bits.
pub const ENT_GUEST_64: u32 = 1 << 9;
pub const ENT_LOAD_EFER: u32 = 1 << 15;

/// Secondary CPU-based control bits.
pub const CPU2_ENABLE_EPT: u32 = 1 << 1;

/// VMX exit reasons.
pub const VMX_REASON_EXC_NMI: u32 = 0;
pub const VMX_REASON_EXT_INT: u32 = 1;
pub const VMX_REASON_CPUID: u32 = 10;
pub const VMX_REASON_HLT: u32 = 12;
pub const VMX_REASON_VMCALL: u32 = 18;
pub const VMX_REASON_EPT_VIOLATION: u32 = 48;

/// Hypercall numbers.
pub const VMX_HYPERCALL_DONE: u64 = 0;
pub const VMX_HYPERCALL_PRINT: u64 = 1;

/// Guest activity state.
pub const ACTV_ACTIVE: u64 = 0;

/// GDT selectors (must match x-kernel's gdt.rs layout).
pub const X86_SEL_CODE64: u64 = 0x08; // GDT index 1
pub const X86_SEL_DATA: u64 = 0x10; // GDT index 2
pub const X86_SEL_TSS: u64 = 0x28; // GDT index 5 (16-byte descriptor)

/// VMCS field encodings.
#[repr(u64)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum VmcsField {
    // 16-bit guest
    GuestSelEs       = 0x0800,
    GuestSelCs       = 0x0802,
    GuestSelSs       = 0x0804,
    GuestSelDs       = 0x0806,
    GuestSelFs       = 0x0808,
    GuestSelGs       = 0x080a,
    GuestSelLdtr     = 0x080c,
    GuestSelTr       = 0x080e,
    // 16-bit host
    HostSelCs        = 0x0c02,
    HostSelSs        = 0x0c04,
    HostSelDs        = 0x0c06,
    HostSelEs        = 0x0c00,
    HostSelFs        = 0x0c08,
    HostSelGs        = 0x0c0a,
    HostSelTr        = 0x0c0c,
    // 64-bit control
    EptPointer       = 0x201a,
    // 64-bit read-only
    GuestPhysAddr    = 0x2400,
    // 64-bit guest
    VmcsLinkPtr      = 0x2800,
    // 64-bit guest
    GuestDebugctl    = 0x2802,
    GuestEfer        = 0x2806,
    // 64-bit host
    HostEfer         = 0x2c02,
    // 32-bit control
    PinControls      = 0x4000,
    CpuExecCtrl0     = 0x4002,
    ExcBitmap        = 0x4004,
    PfErrorMask      = 0x4006,
    PfErrorMatch     = 0x4008,
    Cr3TargetCount   = 0x400a,
    ExiControls      = 0x400c,
    EntControls      = 0x4012,
    CpuExecCtrl2     = 0x401e,
    // 32-bit read-only
    VmInstrError     = 0x4400,
    ExiReason        = 0x4402,
    ExiIntrInfo      = 0x4404,
    ExiInstLen       = 0x440c,
    // 32-bit guest
    GuestLimitEs     = 0x4800,
    GuestLimitCs     = 0x4802,
    GuestLimitSs     = 0x4804,
    GuestLimitDs     = 0x4806,
    GuestLimitFs     = 0x4808,
    GuestLimitGs     = 0x480a,
    GuestLimitLdtr   = 0x480c,
    GuestLimitTr     = 0x480e,
    GuestLimitGdtr   = 0x4810,
    GuestLimitIdtr   = 0x4812,
    GuestArEs        = 0x4814,
    GuestArCs        = 0x4816,
    GuestArSs        = 0x4818,
    GuestArDs        = 0x481a,
    GuestArFs        = 0x481c,
    GuestArGs        = 0x481e,
    GuestArLdtr      = 0x4820,
    GuestArTr        = 0x4822,
    GuestIntrState   = 0x4824,
    GuestActvState   = 0x4826,
    GuestSysenterCs  = 0x482a,
    // 32-bit host
    HostSysenterCs   = 0x4c00,
    // natural-width read-only
    ExiQualification = 0x6400,
    // natural-width guest
    GuestCr0         = 0x6800,
    GuestCr3         = 0x6802,
    GuestCr4         = 0x6804,
    GuestBaseEs      = 0x6806,
    GuestBaseCs      = 0x6808,
    GuestBaseSs      = 0x680a,
    GuestBaseDs      = 0x680c,
    GuestBaseFs      = 0x680e,
    GuestBaseGs      = 0x6810,
    GuestBaseLdtr    = 0x6812,
    GuestBaseTr      = 0x6814,
    GuestBaseGdtr    = 0x6816,
    GuestBaseIdtr    = 0x6818,
    GuestDr7         = 0x681a,
    GuestRsp         = 0x681c,
    GuestRip         = 0x681e,
    GuestRflags      = 0x6820,
    GuestSysenterEsp = 0x6824,
    GuestSysenterEip = 0x6826,
    // natural-width host
    HostCr0          = 0x6c00,
    HostCr3          = 0x6c02,
    HostCr4          = 0x6c04,
    HostBaseFs       = 0x6c06,
    HostBaseGs       = 0x6c08,
    HostBaseTr       = 0x6c0a,
    HostBaseGdtr     = 0x6c0c,
    HostBaseIdtr     = 0x6c0e,
    HostSysenterEsp  = 0x6c10,
    HostSysenterEip  = 0x6c12,
    HostRsp          = 0x6c14,
    HostRip          = 0x6c16,
}

/// VMX Basic MSR layout.
#[derive(Clone, Copy)]
pub struct VmxBasic {
    pub revision: u32,
    pub ctrl: bool,
}

impl VmxBasic {
    pub fn from_msr(val: u64) -> Self {
        Self {
            revision: val as u32 & 0x7FFF_FFFF,
            ctrl: (val >> 55) & 1 != 0,
        }
    }
}

/// VMX capability MSR (allowed-0 / allowed-1 bits).
#[derive(Clone, Copy)]
pub struct VmxCtrlMsr {
    pub set: u32,
    pub clr: u32,
}

impl VmxCtrlMsr {
    pub fn from_msr(val: u64) -> Self {
        Self {
            set: val as u32,
            clr: (val >> 32) as u32,
        }
    }
}

// ── Inline assembly helpers ──

#[inline]
pub fn rdmsr(idx: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: Reading an MSR is safe when the index is valid (caller responsibility).
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") idx, out("eax") lo, out("edx") hi);
    }
    lo as u64 | ((hi as u64) << 32)
}

#[inline]
pub fn wrmsr(idx: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    // SAFETY: Writing an MSR is safe when the index/value are valid (caller responsibility).
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") idx, in("eax") lo, in("edx") hi);
    }
}

#[inline]
pub fn read_cr0() -> u64 {
    let val: u64;
    // SAFETY: Reading CR0 is always safe in ring 0.
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) val) };
    val
}

#[inline]
pub fn read_cr3() -> u64 {
    let val: u64;
    // SAFETY: Reading CR3 is always safe in ring 0.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) val) };
    val
}

#[inline]
pub fn read_cr4() -> u64 {
    let val: u64;
    // SAFETY: Reading CR4 is always safe in ring 0.
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) val) };
    val
}

#[inline]
pub fn write_cr0(val: u64) {
    // SAFETY: CR0 write is safe when the value satisfies VMX fixed-bit constraints.
    unsafe { core::arch::asm!("mov cr0, {}", in(reg) val) };
}

#[inline]
pub fn write_cr4(val: u64) {
    // SAFETY: CR4 write is safe when the value satisfies VMX fixed-bit constraints.
    unsafe { core::arch::asm!("mov cr4, {}", in(reg) val) };
}

#[inline]
pub fn read_rflags() -> u64 {
    let f: u64;
    // SAFETY: pushfq/pop is always safe in ring 0.
    unsafe { core::arch::asm!("pushfq; pop {}", out(reg) f) };
    f
}

#[inline]
pub fn vmcs_read(field: VmcsField) -> u64 {
    let val: u64;
    // SAFETY: vmread is safe when a VMCS is loaded via vmptrld.
    unsafe {
        core::arch::asm!(
            "vmread {val}, {enc}",
            enc = in(reg) field as u64,
            val = out(reg) val,
        );
    }
    val
}

#[inline]
pub fn vmcs_write(field: VmcsField, val: u64) {
    // SAFETY: vmwrite is safe when a VMCS is loaded via vmptrld.
    unsafe {
        core::arch::asm!(
            "vmwrite {enc}, {val}",
            enc = in(reg) field as u64,
            val = in(reg) val,
        );
    }
}

#[inline]
pub fn vmxon(pa: u64) -> bool {
    let mut pa_mem = pa;
    let ret: u8;
    // SAFETY: vmxon is safe when CR4.VMXE=1 and the region is a valid 4K-aligned page.
    unsafe {
        core::arch::asm!(
            "vmxon [{addr}]",
            "setbe {ret}",
            addr = in(reg) &mut pa_mem,
            ret = out(reg_byte) ret,
            options(nostack),
        );
    }
    ret == 0
}

#[inline]
pub fn vmclear(pa: u64) -> bool {
    let mut pa_mem = pa;
    let ret: u8;
    // SAFETY: vmclear is safe when in VMX root mode with a valid VMCS PA.
    unsafe {
        core::arch::asm!(
            "vmclear [{addr}]",
            "setbe {ret}",
            addr = in(reg) &mut pa_mem,
            ret = out(reg_byte) ret,
            options(nostack),
        );
    }
    ret == 0
}

#[inline]
pub fn vmptrld(pa: u64) -> bool {
    let mut pa_mem = pa;
    let ret: u8;
    // SAFETY: vmptrld is safe when in VMX root mode with a valid VMCS PA.
    unsafe {
        core::arch::asm!(
            "vmptrld [{addr}]",
            "setbe {ret}",
            addr = in(reg) &mut pa_mem,
            ret = out(reg_byte) ret,
            options(nostack),
        );
    }
    ret == 0
}

// ── Per-CPU VMXON ──

#[percpu::def_percpu]
static VMXON_DONE: bool = false;

/// Per-CPU VMX init: CR fixup + VMXON with a dynamically allocated page.
///
/// Must be called on each CPU that will run vCPUs.
pub fn vmx_percpu_init() -> bool {
    // SAFETY: reading per-CPU flag on the pinned CPU.
    if unsafe { VMXON_DONE.read_current_raw() } {
        return true;
    }

    let fix_cr0_set = rdmsr(MSR_IA32_VMX_CR0_FIXED0);
    let fix_cr0_clr = rdmsr(MSR_IA32_VMX_CR0_FIXED1);
    let fix_cr4_set = rdmsr(MSR_IA32_VMX_CR4_FIXED0);
    let fix_cr4_clr = rdmsr(MSR_IA32_VMX_CR4_FIXED1);

    write_cr0((read_cr0() & fix_cr0_clr) | fix_cr0_set);
    write_cr4((read_cr4() & fix_cr4_clr) | fix_cr4_set | X86_CR4_VMXE);

    let basic = VmxBasic::from_msr(rdmsr(MSR_IA32_VMX_BASIC));

    let mut page = match kalloc::GlobalPage::alloc_zero() {
        Ok(p) => p,
        Err(_) => {
            log::error!("[VMX] Failed to allocate VMXON page");
            return false;
        }
    };

    let vmxon_ptr = page.as_mut_ptr();
    let vmxon_pa = kaddr_layout::v2p(vmxon_ptr as usize) as u64;
    // SAFETY: page is a zeroed 4K-aligned allocation; writing revision ID.
    unsafe {
        (vmxon_ptr as *mut u32).write(basic.revision);
    }

    if !vmxon(vmxon_pa) {
        log::error!("[VMX] VMXON failed (pa={:#x})", vmxon_pa);
        return false;
    }

    // Leak the page — VMXON region must remain valid while VMX is active.
    core::mem::forget(page);

    // SAFETY: single-threaded per-CPU init path.
    unsafe { VMXON_DONE.write_current_raw(true) };
    log::info!("[VMX] VMXON success (revision={:#x})", basic.revision);
    true
}

// ── Guest Identity Page Table ──

/// x86 conventional page table entry flags (not EPT).
const PT_PRESENT: u64 = 1 << 0;
const PT_WRITABLE: u64 = 1 << 1;
const PT_LARGE: u64 = 1 << 7; // 2 MiB page at PD level

/// Build a guest identity page table covering [0, 4 GiB) using 2 MiB pages.
///
/// Allocates PML4(1) + PDPT(1) + PD(4) = 6 pages. Returns the PML4 physical
/// address and all page objects for lifetime management.
fn build_guest_identity_pt(vcpu: &mut crate::vcpu::Vcpu<super::X86Vmx>) -> Option<u64> {
    // PML4 page
    let mut pml4 = kalloc::GlobalPage::alloc_zero().ok()?;
    let pml4_va = pml4.as_mut_ptr() as *mut u64;
    let pml4_pa = kaddr_layout::v2p(pml4_va as usize) as u64;

    // PDPT page
    let mut pdpt = kalloc::GlobalPage::alloc_zero().ok()?;
    let pdpt_va = pdpt.as_mut_ptr() as *mut u64;
    let pdpt_pa = kaddr_layout::v2p(pdpt_va as usize) as u64;

    // PML4[0] → PDPT
    // SAFETY: pml4_va is a valid zeroed 4 KiB page.
    unsafe { pml4_va.write(pdpt_pa | PT_PRESENT | PT_WRITABLE) };

    // 4 PD pages, each covers 1 GiB (512 × 2 MiB entries)
    for i in 0..4usize {
        let mut pd = kalloc::GlobalPage::alloc_zero().ok()?;
        let pd_va = pd.as_mut_ptr() as *mut u64;
        let pd_pa = kaddr_layout::v2p(pd_va as usize) as u64;

        // PDPT[i] → PD
        // SAFETY: pdpt_va is valid; i < 512.
        unsafe { pdpt_va.add(i).write(pd_pa | PT_PRESENT | PT_WRITABLE) };

        // Fill PD with 2 MiB identity large pages
        for j in 0..512usize {
            let pa = ((i as u64) << 30) | ((j as u64) << 21);
            // SAFETY: pd_va is valid; j < 512.
            unsafe { pd_va.add(j).write(pa | PT_PRESENT | PT_WRITABLE | PT_LARGE) };
        }
        vcpu.hw_pages.push(pd);
    }

    vcpu.hw_pages.push(pdpt);
    vcpu.hw_pages.push(pml4);

    log::info!("[VMX] guest identity PT: pml4_pa={:#x}", pml4_pa);
    Some(pml4_pa)
}

// ── Per-vCPU VMCS Initialization ──

/// Initialize a VMCS for a vCPU. Allocates a VMCS page and programs all fields.
pub fn vmcs_init_vcpu(
    vcpu: &mut crate::vcpu::Vcpu<super::X86Vmx>,
    entry: u64,
    guest_sp: u64,
) -> bool {
    let basic = VmxBasic::from_msr(rdmsr(MSR_IA32_VMX_BASIC));

    let mut page = match kalloc::GlobalPage::alloc_zero() {
        Ok(p) => p,
        Err(_) => {
            log::error!("[VMX] Failed to allocate VMCS page");
            return false;
        }
    };

    let vmcs_ptr = page.as_mut_ptr();
    let vmcs_pa = kaddr_layout::v2p(vmcs_ptr as usize) as u64;
    // SAFETY: page is a zeroed 4K-aligned allocation; writing revision ID.
    unsafe {
        (vmcs_ptr as *mut u32).write(basic.revision);
    }

    vcpu.hw_pages.push(page);
    vcpu.arch.vmcs_pa = vmcs_pa;

    if !vmclear(vmcs_pa) {
        log::error!("[VMX] vmclear failed");
        return false;
    }

    // ── Pre-compute all values before disabling interrupts ──
    let msr_pin = if basic.ctrl {
        MSR_IA32_VMX_TRUE_PIN
    } else {
        MSR_IA32_VMX_PINBASED_CTLS
    };
    let msr_cpu = if basic.ctrl {
        MSR_IA32_VMX_TRUE_PROC
    } else {
        MSR_IA32_VMX_PROCBASED_CTLS
    };
    let msr_exit = if basic.ctrl {
        MSR_IA32_VMX_TRUE_EXIT
    } else {
        MSR_IA32_VMX_EXIT_CTLS
    };
    let msr_ent = if basic.ctrl {
        MSR_IA32_VMX_TRUE_ENTRY
    } else {
        MSR_IA32_VMX_ENTRY_CTLS
    };

    let pin_rev = VmxCtrlMsr::from_msr(rdmsr(msr_pin));
    let cpu_rev = VmxCtrlMsr::from_msr(rdmsr(msr_cpu));
    let exi_rev = VmxCtrlMsr::from_msr(rdmsr(msr_exit));
    let ent_rev = VmxCtrlMsr::from_msr(rdmsr(msr_ent));

    let ctrl_pin = ((PIN_EXTERNAL_INT_EXIT | PIN_NMI | PIN_VIRT_NMI) | pin_rev.set) & pin_rev.clr;
    let ctrl_cpu = (CPU_HLT | cpu_rev.set) & cpu_rev.clr;
    let ctrl_exit = ((EXI_HOST_64 | EXI_LOAD_EFER | EXI_SAVE_EFER) | exi_rev.set) & exi_rev.clr;
    let ctrl_enter = ((ENT_GUEST_64 | ENT_LOAD_EFER) | ent_rev.set) & ent_rev.clr;

    let gdt_base = sgdt_base();
    let idt_base = sidt_base();
    let tss_base = read_tss_base(gdt_base);
    let host_efer = rdmsr(MSR_EFER);
    let host_cr0 = read_cr0();
    let host_cr3 = read_cr3();
    let host_cr4 = read_cr4();
    let host_fs_base = rdmsr(MSR_FS_BASE);
    let host_gs_base = rdmsr(MSR_GS_BASE);
    let guest_efer = rdmsr(MSR_EFER);
    let guest_cr0 = read_cr0();
    let guest_cr4 = read_cr4();
    let gdt_limit = sgdt_limit() as u64;
    let idt_limit = sidt_limit() as u64;

    // Build guest identity page table (allocates pages — must be before cli).
    let guest_cr3 = match build_guest_identity_pt(vcpu) {
        Some(pa) => pa,
        None => {
            log::error!("[VMX] failed to build guest identity page table");
            return false;
        }
    };

    // ── Critical section: vmptrld → vmwrites → vmclear (no allocations) ──
    // SAFETY: disable interrupts to prevent preemption during VMCS writes.
    unsafe {
        core::arch::asm!("cli");
    }

    if !vmptrld(vmcs_pa) {
        // SAFETY: re-enable interrupts on error.
        unsafe {
            core::arch::asm!("sti");
        }
        log::error!("[VMX] vmptrld failed");
        return false;
    }

    vmcs_write(VmcsField::PinControls, ctrl_pin as u64);
    vmcs_write(VmcsField::CpuExecCtrl0, ctrl_cpu as u64);
    vmcs_write(VmcsField::ExiControls, ctrl_exit as u64);
    vmcs_write(VmcsField::EntControls, ctrl_enter as u64);
    vmcs_write(VmcsField::Cr3TargetCount, 0);
    vmcs_write(VmcsField::ExcBitmap, 0);
    vmcs_write(VmcsField::PfErrorMask, 0);
    vmcs_write(VmcsField::PfErrorMatch, 0);

    // ── Host state (using pre-computed values) ──
    vmcs_write(VmcsField::HostEfer, host_efer);
    vmcs_write(VmcsField::HostCr0, host_cr0);
    vmcs_write(VmcsField::HostCr3, host_cr3);
    vmcs_write(VmcsField::HostCr4, host_cr4);

    vmcs_write(VmcsField::HostSelCs, X86_SEL_CODE64);
    vmcs_write(VmcsField::HostSelSs, X86_SEL_DATA);
    vmcs_write(VmcsField::HostSelDs, X86_SEL_DATA);
    vmcs_write(VmcsField::HostSelEs, X86_SEL_DATA);
    vmcs_write(VmcsField::HostSelFs, X86_SEL_DATA);
    vmcs_write(VmcsField::HostSelGs, X86_SEL_DATA);
    vmcs_write(VmcsField::HostSelTr, X86_SEL_TSS);

    vmcs_write(VmcsField::HostBaseTr, tss_base);
    vmcs_write(VmcsField::HostBaseGdtr, gdt_base);
    vmcs_write(VmcsField::HostBaseIdtr, idt_base);
    vmcs_write(VmcsField::HostBaseFs, host_fs_base);
    vmcs_write(VmcsField::HostBaseGs, host_gs_base);

    vmcs_write(VmcsField::HostSysenterCs, 0);
    vmcs_write(VmcsField::HostSysenterEsp, 0);
    vmcs_write(VmcsField::HostSysenterEip, 0);

    vmcs_write(VmcsField::VmcsLinkPtr, !0u64);

    // ── Guest state (identity page table, GVA = GPA) ──
    vmcs_write(VmcsField::GuestCr0, guest_cr0);
    vmcs_write(VmcsField::GuestCr3, guest_cr3);
    vmcs_write(VmcsField::GuestCr4, guest_cr4);
    vmcs_write(VmcsField::GuestEfer, guest_efer);
    vmcs_write(VmcsField::GuestDr7, 0);

    vmcs_write(VmcsField::GuestSelCs, X86_SEL_CODE64);
    vmcs_write(VmcsField::GuestSelSs, X86_SEL_DATA);
    vmcs_write(VmcsField::GuestSelDs, X86_SEL_DATA);
    vmcs_write(VmcsField::GuestSelEs, X86_SEL_DATA);
    vmcs_write(VmcsField::GuestSelFs, X86_SEL_DATA);
    vmcs_write(VmcsField::GuestSelGs, X86_SEL_DATA);
    vmcs_write(VmcsField::GuestSelLdtr, 0);
    vmcs_write(VmcsField::GuestSelTr, X86_SEL_TSS);

    vmcs_write(VmcsField::GuestBaseCs, 0);
    vmcs_write(VmcsField::GuestBaseSs, 0);
    vmcs_write(VmcsField::GuestBaseDs, 0);
    vmcs_write(VmcsField::GuestBaseEs, 0);
    vmcs_write(VmcsField::GuestBaseFs, 0);
    vmcs_write(VmcsField::GuestBaseGs, 0);
    vmcs_write(VmcsField::GuestBaseLdtr, 0);
    vmcs_write(VmcsField::GuestBaseTr, tss_base);
    vmcs_write(VmcsField::GuestBaseGdtr, gdt_base);
    vmcs_write(VmcsField::GuestBaseIdtr, idt_base);

    vmcs_write(VmcsField::GuestLimitCs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitSs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitDs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitEs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitFs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitGs, 0xFFFF_FFFF);
    vmcs_write(VmcsField::GuestLimitLdtr, 0xFFFF);
    vmcs_write(VmcsField::GuestLimitTr, 0x0067);
    vmcs_write(VmcsField::GuestLimitGdtr, gdt_limit);
    vmcs_write(VmcsField::GuestLimitIdtr, idt_limit);

    // Segment access rights.
    vmcs_write(VmcsField::GuestArCs, 0xa09b); // 64-bit code: L=1, G=1, P=1
    vmcs_write(VmcsField::GuestArSs, 0xc093);
    vmcs_write(VmcsField::GuestArDs, 0xc093);
    vmcs_write(VmcsField::GuestArEs, 0xc093);
    vmcs_write(VmcsField::GuestArFs, 0xc093);
    vmcs_write(VmcsField::GuestArGs, 0xc093);
    vmcs_write(VmcsField::GuestArLdtr, 0x0082);
    vmcs_write(VmcsField::GuestArTr, 0x008b); // Busy TSS 64-bit

    vmcs_write(VmcsField::GuestSysenterCs, 0);
    vmcs_write(VmcsField::GuestSysenterEsp, 0);
    vmcs_write(VmcsField::GuestSysenterEip, 0);

    vmcs_write(VmcsField::GuestRip, entry);
    vmcs_write(VmcsField::GuestRsp, guest_sp);
    vmcs_write(VmcsField::GuestRflags, 0x2);

    vmcs_write(VmcsField::GuestActvState, ACTV_ACTIVE);
    vmcs_write(VmcsField::GuestIntrState, 0);
    vmcs_write(VmcsField::GuestDebugctl, 0);

    // Flush VMCS to memory and reset launch state to "clear".
    if !vmclear(vmcs_pa) {
        // SAFETY: re-enable interrupts on error.
        unsafe {
            core::arch::asm!("sti");
        }
        log::error!("[VMX] vmclear(flush) failed");
        return false;
    }

    // SAFETY: re-enable interrupts after VMCS critical section complete.
    unsafe {
        core::arch::asm!("sti");
    }

    log::info!(
        "[VMX] VMCS initialized: entry={:#x} sp={:#x}",
        entry,
        guest_sp
    );
    true
}

/// Refresh all per-CPU host state fields in the current VMCS.
///
/// After a vCPU thread migrates to a different physical CPU, the host
/// descriptor table bases, segment bases, and CR3 cached in the VMCS are
/// stale. This function re-reads them from the current CPU and writes
/// them into the VMCS so that VM exit restores the correct host context.
pub fn refresh_host_state() {
    let gdt_base = sgdt_base();
    vmcs_write(VmcsField::HostCr3, read_cr3());
    vmcs_write(VmcsField::HostBaseGdtr, gdt_base);
    vmcs_write(VmcsField::HostBaseIdtr, sidt_base());
    vmcs_write(VmcsField::HostBaseTr, read_tss_base(gdt_base));
    vmcs_write(VmcsField::HostBaseFs, rdmsr(MSR_FS_BASE));
    vmcs_write(VmcsField::HostBaseGs, rdmsr(MSR_GS_BASE));
}

/// Enable EPT on an already-initialized VMCS.
///
/// Loads the VMCS, enables secondary proc-based controls with
/// ENABLE_EPT, writes the EPTP, and flushes. Returns `false` if
/// the CPU does not support the required controls.
pub fn vmcs_enable_ept(vmcs_pa: u64, eptp: u64) -> bool {
    // SAFETY: disable interrupts to prevent preemption during VMCS writes.
    unsafe {
        core::arch::asm!("cli");
    }

    if !vmptrld(vmcs_pa) {
        // SAFETY: re-enable interrupts on error.
        unsafe {
            core::arch::asm!("sti");
        }
        log::error!("[VMX] vmcs_enable_ept: vmptrld failed");
        return false;
    }

    let cpu_rev = VmxCtrlMsr::from_msr(rdmsr(
        if VmxBasic::from_msr(rdmsr(MSR_IA32_VMX_BASIC)).ctrl {
            MSR_IA32_VMX_TRUE_PROC
        } else {
            MSR_IA32_VMX_PROCBASED_CTLS
        },
    ));

    if cpu_rev.clr & CPU_ACTIVATE_SECONDARY == 0 {
        log::error!("[VMX] secondary proc-based controls not supported");
        return false;
    }

    let ctrl0 = vmcs_read(VmcsField::CpuExecCtrl0) as u32;
    vmcs_write(
        VmcsField::CpuExecCtrl0,
        (ctrl0 | CPU_ACTIVATE_SECONDARY) as u64,
    );

    let cpu2_rev = VmxCtrlMsr::from_msr(rdmsr(MSR_IA32_VMX_PROCBASED_CTLS2));
    if cpu2_rev.clr & CPU2_ENABLE_EPT == 0 {
        log::error!("[VMX] EPT not supported by CPU");
        return false;
    }

    let ctrl2 = (CPU2_ENABLE_EPT | cpu2_rev.set) & cpu2_rev.clr;
    vmcs_write(VmcsField::CpuExecCtrl2, ctrl2 as u64);
    vmcs_write(VmcsField::EptPointer, eptp);

    if !vmclear(vmcs_pa) {
        // SAFETY: re-enable interrupts on error.
        unsafe {
            core::arch::asm!("sti");
        }
        log::error!("[VMX] vmcs_enable_ept: vmclear failed");
        return false;
    }

    // SAFETY: re-enable interrupts after VMCS critical section.
    unsafe {
        core::arch::asm!("sti");
    }

    log::info!("[VMX] EPT enabled: eptp={:#x}", eptp);
    true
}

// ── Helper functions ──

#[inline]
fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let (eax, ebx, ecx, edx): (u32, u32, u32, u32);
    // SAFETY: cpuid is always safe; push/pop rbx required because LLVM reserves it.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",
            inout("eax") leaf => eax,
            ebx_out = out(reg) ebx,
            inout("ecx") 0u32 => ecx,
            out("edx") edx,
        );
    }
    (eax, ebx, ecx, edx)
}

#[inline]
fn sgdt_base() -> u64 {
    #[repr(C, packed)]
    struct DtReg {
        limit: u16,
        base: u64,
    }
    let mut dt = DtReg { limit: 0, base: 0 };
    // SAFETY: sgdt stores the current GDT descriptor into the provided memory.
    unsafe { core::arch::asm!("sgdt [{}]", in(reg) &mut dt, options(nostack)) };
    dt.base
}

#[inline]
fn sidt_base() -> u64 {
    #[repr(C, packed)]
    struct DtReg {
        limit: u16,
        base: u64,
    }
    let mut dt = DtReg { limit: 0, base: 0 };
    // SAFETY: sidt stores the current IDT descriptor into the provided memory.
    unsafe { core::arch::asm!("sidt [{}]", in(reg) &mut dt, options(nostack)) };
    dt.base
}

#[inline]
fn sgdt_limit() -> u16 {
    #[repr(C, packed)]
    struct DtReg {
        limit: u16,
        base: u64,
    }
    let mut dt = DtReg { limit: 0, base: 0 };
    // SAFETY: sgdt stores the current GDT descriptor into the provided memory.
    unsafe { core::arch::asm!("sgdt [{}]", in(reg) &mut dt, options(nostack)) };
    dt.limit
}

#[inline]
fn sidt_limit() -> u16 {
    #[repr(C, packed)]
    struct DtReg {
        limit: u16,
        base: u64,
    }
    let mut dt = DtReg { limit: 0, base: 0 };
    // SAFETY: sidt stores the current IDT descriptor into the provided memory.
    unsafe { core::arch::asm!("sidt [{}]", in(reg) &mut dt, options(nostack)) };
    dt.limit
}

/// Read TSS base from the 16-byte system descriptor at GDT index 5.
fn read_tss_base(gdt_base: u64) -> u64 {
    let gdt = gdt_base as *const u64;
    // SAFETY: GDT base is valid and index 5-6 are within the TSS descriptor range.
    let tss_lo = unsafe { gdt.add(5).read() };
    // SAFETY: GDT[6] is the upper half of the 16-byte TSS system descriptor.
    let tss_hi = unsafe { gdt.add(6).read() };
    ((tss_lo >> 16) & 0xFF_FFFF) | (((tss_lo >> 56) & 0xFF) << 24) | ((tss_hi & 0xFFFF_FFFF) << 32)
}

/// Check VMX hardware support (CPUID + IA32_FEATURE_CONTROL).
pub fn vmx_check_support() -> bool {
    let (_, _, ecx, _) = cpuid(1);
    if ecx & (1 << 5) == 0 {
        log::error!("[VMX] CPU does not support VMX (CPUID.1.ECX[5]=0)");
        return false;
    }

    let fc = rdmsr(MSR_IA32_FEATURE_CONTROL);
    if (fc & 0x5) == 0x5 {
        log::trace!("[VMX] VMX enabled and locked by firmware");
        return true;
    }
    if fc & 0x1 != 0 {
        log::error!("[VMX] VMX locked out by firmware");
        return false;
    }
    wrmsr(MSR_IA32_FEATURE_CONTROL, fc | 0x5);
    true
}
