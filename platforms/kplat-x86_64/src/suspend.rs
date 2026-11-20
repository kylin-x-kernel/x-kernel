// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 S3 execution-context preservation and firmware wake trampoline.

use core::arch::global_asm;

#[repr(C, align(16))]
struct FxSaveArea([u8; 512]);

/// Storage shared with the reset trampoline. Assembly offsets follow this layout.
#[repr(C, align(16))]
pub(crate) struct SavedCpu {
    rsp: u64,
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    cr3: u64,
    cr0: u64,
    cr4: u64,
    efer: u64,
    gs_base: u64,
    kernel_gs_base: u64,
    fs_base: u64,
    star: u64,
    lstar: u64,
    fmask: u64,
    apic_base: u64,
    gdtr: [u64; 2],
    idtr: [u64; 2],
    tr: u64,
    cr2: u64,
    cr8: u64,
    fxsave: FxSaveArea,
}
global_asm!(
    include_str!("suspend.S"),
    ctx_rsp = const core::mem::offset_of!(SavedCpu, rsp),
    ctx_rbx = const core::mem::offset_of!(SavedCpu, rbx),
    ctx_rbp = const core::mem::offset_of!(SavedCpu, rbp),
    ctx_r12 = const core::mem::offset_of!(SavedCpu, r12),
    ctx_r13 = const core::mem::offset_of!(SavedCpu, r13),
    ctx_r14 = const core::mem::offset_of!(SavedCpu, r14),
    ctx_r15 = const core::mem::offset_of!(SavedCpu, r15),
    ctx_cr3 = const core::mem::offset_of!(SavedCpu, cr3),
    ctx_cr0 = const core::mem::offset_of!(SavedCpu, cr0),
    ctx_cr4 = const core::mem::offset_of!(SavedCpu, cr4),
    ctx_efer = const core::mem::offset_of!(SavedCpu, efer),
    ctx_gs_base = const core::mem::offset_of!(SavedCpu, gs_base),
    ctx_kernel_gs_base = const core::mem::offset_of!(SavedCpu, kernel_gs_base),
    ctx_fs_base = const core::mem::offset_of!(SavedCpu, fs_base),
    ctx_star = const core::mem::offset_of!(SavedCpu, star),
    ctx_lstar = const core::mem::offset_of!(SavedCpu, lstar),
    ctx_fmask = const core::mem::offset_of!(SavedCpu, fmask),
    ctx_apic_base = const core::mem::offset_of!(SavedCpu, apic_base),
    ctx_gdtr = const core::mem::offset_of!(SavedCpu, gdtr),
    ctx_idtr = const core::mem::offset_of!(SavedCpu, idtr),
    ctx_tr = const core::mem::offset_of!(SavedCpu, tr),
    ctx_cr2 = const core::mem::offset_of!(SavedCpu, cr2),
    ctx_cr8 = const core::mem::offset_of!(SavedCpu, cr8),
    ctx_fxsave = const core::mem::offset_of!(SavedCpu, fxsave),
);

impl SavedCpu {
    pub(crate) const fn new() -> Self {
        Self {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            cr3: 0,
            cr0: 0,
            cr4: 0,
            efer: 0,
            gs_base: 0,
            kernel_gs_base: 0,
            fs_base: 0,
            star: 0,
            lstar: 0,
            fmask: 0,
            apic_base: 0,
            gdtr: [0; 2],
            idtr: [0; 2],
            tr: 0,
            cr2: 0,
            cr8: 0,
            fxsave: FxSaveArea([0; 512]),
        }
    }

    /// Saves this CPU and invokes `enter`, returning after abort or restoration.
    ///
    /// # Safety
    /// IRQs and migration must remain disabled. `self` must stay at its address
    /// through resume. The caller must arrange the boot mapping, wake vector and
    /// hardware restoration before permitting any ordinary execution. `enter`
    /// must not unwind, change CPU ownership or release the saved stack.
    pub(crate) unsafe fn save_and_call(
        &mut self,
        enter: extern "C" fn(*mut ()),
        argument: *mut (),
    ) {
        debug_assert!(!karch::local_irq_enabled());
        // No independent migration query is available here; the caller retains
        // its CPU-pinning guard across the assembly handoff.
        unsafe extern "C" {
            fn s3_save_and_call(
                context: *mut SavedCpu,
                enter: extern "C" fn(*mut ()),
                argument: *mut (),
            );
        }
        // SAFETY: the caller supplies the exclusive CPU and pinned storage,
        // saved-stack lifetime and non-unwinding callback required by assembly.
        unsafe { s3_save_and_call(self, enter, argument) };
    }
}

unittest::path_mod_test!(tests, "../ut-tests/suspend.rs");

use core::ptr;

use kerrno::{KError, KResult};
use khal::mem::{PAGE_SIZE_4K, p2v, pa, v2p};

// ACPI Firmware ACPI Control Structure (FACS), byte offsets from its signature.
const FACS_FIRMWARE_WAKING_VECTOR: usize = 12;
const FACS_X_FIRMWARE_WAKING_VECTOR: usize = 24;
const FACS_OSPM_FLAGS: usize = 36;
const FACS_64BIT_WAKE_F: u32 = 1;

/// Exclusive temporary ownership of the reserved AP page and firmware vector.
/// Caller serialization must exclude boot/hotplug and every other PM attempt.
pub(crate) struct WakePage {
    page: *mut u8,
    facs: *mut u8,
    original: [u8; PAGE_SIZE_4K],
    old_vector: u32,
    old_x_vector: u64,
    old_flags: u32,
}

impl WakePage {
    /// Installs the real-mode entry using pinned context storage.
    ///
    /// # Safety
    /// The caller must exclusively own firmware wake state and the reserved AP
    /// page. No AP may execute the page until setup completes. `context` must
    /// stay pinned until no processor can execute this wake entry.
    pub(crate) unsafe fn install(context: *mut SavedCpu) -> KResult<Self> {
        let address = usize::try_from(
            acpi::find_facs_address_from_init().ok_or(KError::OperationNotSupported)?,
        )
        .map_err(|_| KError::OperationNotSupported)?;
        if address & 63 != 0 {
            return Err(KError::InvalidInput);
        }
        let facs = p2v(pa!(address)).as_mut_ptr();
        // SAFETY: FADT supplied the aligned firmware FACS physical address;
        // boot keeps ACPI reserved memory mapped throughout platform operation.
        let header = unsafe { core::slice::from_raw_parts(facs, 8) };
        if &header[..4] != b"FACS"
            || u32::from_le_bytes([header[4], header[5], header[6], header[7]]) < 64
        {
            return Err(KError::InvalidInput);
        }
        let page_address = crate::peripherals::bootmem::ap_trampoline_page_paddr();
        let page = p2v(pa!(page_address)).as_mut_ptr();
        unsafe extern "C" {
            static s3_wake_start: u8;
            static s3_wake_end: u8;
            static s3_wake_pm: u8;
            static s3_wake_lm: u8;
            static s3_wake_pm_patch: u8;
            static s3_wake_lm_patch: u8;
            static s3_wake_gdt: u8;
            static s3_wake_gdt_patch: u8;
            static s3_wake_cr3: u8;
            static s3_wake_context: u8;
            static s3_wake_target: u8;
            static x86_ap_boot_pml4: u8;
            fn s3_restore_context();
        }
        let start = ptr::addr_of!(s3_wake_start) as usize;
        let length = ptr::addr_of!(s3_wake_end) as usize - start;
        if length > PAGE_SIZE_4K {
            return Err(KError::InvalidInput);
        }
        let mut saved = Self {
            page,
            facs,
            original: [0; PAGE_SIZE_4K],
            old_vector: 0,
            old_x_vector: 0,
            old_flags: 0,
        };
        // SAFETY: boot reserves the entire trampoline page in direct-mapped RAM.
        // The caller excludes AP startup and other writers while we read it.
        // The writable backup array has PAGE_SIZE_4K bytes in separate Rust
        // storage, so the two byte ranges cannot overlap.
        unsafe { ptr::copy_nonoverlapping(page, saved.original.as_mut_ptr(), PAGE_SIZE_4K) };
        // SAFETY: validated FACS extent includes its aligned waking vector.
        saved.old_vector = unsafe {
            facs.wrapping_add(FACS_FIRMWARE_WAKING_VECTOR)
                .cast::<u32>()
                .read_volatile()
        };
        // SAFETY: validated FACS extent includes its aligned extended vector.
        saved.old_x_vector = unsafe {
            facs.wrapping_add(FACS_X_FIRMWARE_WAKING_VECTOR)
                .cast::<u64>()
                .read_volatile()
        };
        // SAFETY: validated FACS extent includes the aligned OS flags.
        saved.old_flags = unsafe {
            facs.wrapping_add(FACS_OSPM_FLAGS)
                .cast::<u32>()
                .read_volatile()
        };
        // SAFETY: the linker labels bound readable bytes in kernel text. The
        // length check above bounds the write to the reserved trampoline page.
        // That writable low-memory mapping is disjoint from the kernel image,
        // and the caller excludes AP execution and writes until installation ends.
        unsafe { ptr::copy_nonoverlapping(start as *const u8, page, length) };
        for (patch, target) in [
            (ptr::addr_of!(s3_wake_pm_patch), ptr::addr_of!(s3_wake_pm)),
            (ptr::addr_of!(s3_wake_lm_patch), ptr::addr_of!(s3_wake_lm)),
            (ptr::addr_of!(s3_wake_gdt_patch), ptr::addr_of!(s3_wake_gdt)),
        ] {
            saved.patch(
                patch as usize - start,
                (page_address + target as usize - start) as u32,
            );
        }
        saved.patch(
            ptr::addr_of!(s3_wake_cr3) as usize - start,
            v2p((ptr::addr_of!(x86_ap_boot_pml4) as usize).into()).as_usize() as u64,
        );
        saved.patch(
            ptr::addr_of!(s3_wake_context) as usize - start,
            context as u64,
        );
        saved.patch(
            ptr::addr_of!(s3_wake_target) as usize - start,
            s3_restore_context as *const () as u64,
        );
        // SAFETY: the validated aligned FACS field is owned by this transaction.
        unsafe {
            facs.wrapping_add(FACS_X_FIRMWARE_WAKING_VECTOR)
                .cast::<u64>()
                .write_volatile(0)
        };
        // SAFETY: the same ownership excludes other firmware-vector writers.
        unsafe {
            facs.wrapping_add(FACS_OSPM_FLAGS)
                .cast::<u32>()
                .write_volatile(saved.old_flags & !FACS_64BIT_WAKE_F)
        };
        // SAFETY: publish the real-mode vector after the complete wake image.
        unsafe {
            facs.wrapping_add(FACS_FIRMWARE_WAKING_VECTOR)
                .cast::<u32>()
                .write_volatile(page_address as u32)
        };
        Ok(saved)
    }
}

impl WakePage {
    fn patch<T: Copy>(&mut self, offset: usize, value: T) {
        assert!(offset <= PAGE_SIZE_4K - core::mem::size_of::<T>());
        let address = self.page.wrapping_add(offset).cast::<T>();
        // SAFETY: the checked patch fits the exclusively owned reserved page;
        // unaligned writes support linker-defined instruction operands.
        unsafe { address.write_unaligned(value) };
    }
}

impl Drop for WakePage {
    fn drop(&mut self) {
        // SAFETY: transaction ownership retains this validated FACS vector.
        unsafe {
            self.facs
                .wrapping_add(FACS_FIRMWARE_WAKING_VECTOR)
                .cast::<u32>()
                .write_volatile(self.old_vector)
        };
        // SAFETY: ownership likewise retains the aligned extended field.
        unsafe {
            self.facs
                .wrapping_add(FACS_X_FIRMWARE_WAKING_VECTOR)
                .cast::<u64>()
                .write_volatile(self.old_x_vector)
        };
        // SAFETY: ownership likewise retains the aligned OS flags.
        unsafe {
            self.facs
                .wrapping_add(FACS_OSPM_FLAGS)
                .cast::<u32>()
                .write_volatile(self.old_flags)
        };
        // SAFETY: the backup holds PAGE_SIZE_4K readable bytes in Rust storage,
        // disjoint from the writable reserved page retained since install.
        // All AP callbacks have joined before the saved bytes are restored.
        unsafe { ptr::copy_nonoverlapping(self.original.as_ptr(), self.page, PAGE_SIZE_4K) };
    }
}

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

const CPU_COUNT: usize = kbuild_config::NR_CPUS;
struct CpuStorage(UnsafeCell<[SavedCpu; CPU_COUNT]>);
// SAFETY: one PM transaction owns the array. Each CPU writes only its own
// fixed slot with IRQs masked; the BSP publishes its pointer before SIPI and
// does not inspect/modify a remote slot while that CPU can execute it.
unsafe impl Sync for CpuStorage {}
static CPU_CONTEXTS: CpuStorage =
    CpuStorage(UnsafeCell::new([const { SavedCpu::new() }; CPU_COUNT]));
static CPU_RESTORED: [AtomicBool; CPU_COUNT] = [const { AtomicBool::new(false) }; CPU_COUNT];
static ACTIVE: AtomicBool = AtomicBool::new(false);
const PM1_SCI_EN: u32 = 1;
const PM1_SLP_TYP_SHIFT: u32 = 10;
const PM1_SLP_EN: u32 = 1 << 13;

static S3_REQUEST: AtomicU32 = AtomicU32::new(0);
struct WakeStorage(UnsafeCell<Option<WakePage>>);
// SAFETY: only the boot CPU owning ACTIVE accesses the installed wake page,
// and it retargets it only after the previous CPU acknowledged leaving it.
unsafe impl Sync for WakeStorage {}
static WAKE: WakeStorage = WakeStorage(UnsafeCell::new(None));
struct IrqStorage(UnsafeCell<Option<x86_apic::SavedIoApics>>);
// SAFETY: the serialized boot-CPU PM owner exclusively accesses this state.
unsafe impl Sync for IrqStorage {}
static IRQS: IrqStorage = IrqStorage(UnsafeCell::new(None));

fn context_address(cpu: usize) -> *mut SavedCpu {
    assert!(cpu < CPU_COUNT);
    // SAFETY: the checked CPU index selects fixed static storage. No reference
    // to the whole array is formed while other CPUs own distinct elements.
    unsafe { ptr::addr_of_mut!((*CPU_CONTEXTS.0.get())[cpu]) }
}

fn restore_local_apic() {
    if khal::percpu::this_cpu_id().as_usize() == 0 {
        // SAFETY: after firmware wake, mask both legacy PICs before re-enabling
        // local IRQs; runtime exclusively uses the restored APIC routing.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0x21u16, in("al") 0xffu8);
        }
        // SAFETY: the slave PIC has the same fixed platform I/O ownership.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0xa1u16, in("al") 0xffu8);
        }
    }
    x86_apic::with_local_apic(|lapic| {
        // SAFETY: assembly restored APIC-base access and this CPU's slot base;
        // IRQs remain masked and the permanent handle belongs to this CPU.
        unsafe { lapic.enable_masked() };
        // SAFETY: the local APIC mapping and configured error vector are live.
        unsafe { lapic.enable_error() };
    });
}

extern "C" fn request_s3(argument: *mut ()) {
    let port = argument as usize as u16;
    let control = ((argument as usize) >> u16::BITS) as u16;
    // SAFETY: prepare selected QEMU's FADT-declared PM1a control block. The
    // system coordinator retained the installed vector and every saved stack.
    unsafe {
        core::arch::asm!("out dx, ax", "2: hlt", "jmp 2b",
            in("dx") port, in("ax") control, options(noreturn));
    }
}

fn raw_delay(microseconds: u64) {
    // SAFETY: x86_64 QEMU exposes TSC and PM has restored counter access. Use
    // raw ticks because logical MONOTONIC intentionally remains frozen here.
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    let ticks =
        khal::time::span_to_ticks(ktime_types::TimeSpan::from_micros(microseconds)).as_raw();
    // SAFETY: same live, stable TSC as sampled above.
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

#[kiface::provide]
impl kplat::sys::SystemSleepIf {
    fn persistent_time() -> Option<ktime_types::SystemTime> {
        #[cfg(feature = "rtc")]
        {
            rtc_driver::read(rtc_driver::RtcConfig::platform(
                rtc_driver::RtcKind::Cmos,
                rtc_driver::RtcSource::PlatformStatic,
            ))
        }
        #[cfg(not(feature = "rtc"))]
        {
            None
        }
    }

    unsafe fn prepare(sleep_types: [u8; 2]) -> KResult {
        if khal::percpu::this_cpu_id().as_usize() != 0 || !karch::local_irq_enabled() {
            return Err(KError::InvalidInput);
        }
        // This backend implements QEMU/SeaBIOS S3, including its retained RAM
        // and device reset contract. Other firmware requires its own ACPI
        // preparation/wake methods rather than inheriting these assumptions.
        let (_, fadt) = acpi::find_table(*b"FACP").ok_or(KError::OperationNotSupported)?;
        let pm1 = acpi::find_pm1a_control_block_from_init().ok_or(KError::OperationNotSupported)?;
        if fadt.oem_id != *b"BOCHS "
            || &fadt.oem_table_id[..4] != b"BXPC"
            || pm1.length != 2
            || sleep_types[0] > 7
            || sleep_types[0] != sleep_types[1]
        {
            log::warn!(
                "S3 unsupported firmware: OEM {:?}/{:?}, PM1 length {}, types {:?}",
                fadt.oem_id,
                fadt.oem_table_id,
                pm1.length,
                sleep_types
            );
            return Err(KError::OperationNotSupported);
        }
        let mut unsupported_id = false;
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
            unsupported_id |=
                kcpu_id_map::raw_cpu_id(cpu).is_none_or(|raw| raw.as_usize() > u8::MAX as usize);
        });
        if unsupported_id {
            return Err(KError::OperationNotSupported);
        }
        // Acquire takes ownership after finish's Release, observing the prior
        // owner's completed WAKE/IRQS cleanup. This CAS does not publish the
        // new contents: remote callbacks receive those via Stop-work admission.
        ACTIVE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| KError::ResourceBusy)?;
        let control = PM1_SCI_EN | (u32::from(sleep_types[0]) << PM1_SLP_TYP_SHIFT) | PM1_SLP_EN;
        S3_REQUEST.store(
            u32::from(pm1.port) | (control << u16::BITS),
            Ordering::Relaxed,
        );
        // SAFETY: the transaction owns fixed context/wake storage and excludes
        // AP startup until finish. No saved CPU context exists yet.
        let irq_state = match x86_apic::SavedIoApics::prepare() {
            Ok(state) => state,
            Err(error) => {
                // Publish the abandoned claim to the next acquiring owner;
                // no wake/IRQ resource remains installed by this attempt.
                // Publish complete resource cleanup to the next owner's Acquire CAS.
                ACTIVE.store(false, Ordering::Release);
                return Err(error);
            }
        };
        // SAFETY: ACTIVE owns wake resources and the BSP slot until join.
        let result = unsafe { WakePage::install(context_address(0)) };
        match result {
            Ok(wake) => {
                // No AP has been started for this owned transaction. Stop-work
                // publication precedes AP execution; these resets need no release
                // edge. Each AP later Release-publishes its own restored flag.
                for restored in &CPU_RESTORED {
                    restored.store(false, Ordering::Relaxed);
                }
                // SAFETY: only the serialized boot-CPU owner accesses WAKE.
                unsafe { *WAKE.0.get() = Some(wake) };
                // SAFETY: the same serialized boot owner owns IRQ storage.
                unsafe { *IRQS.0.get() = Some(irq_state) };
                Ok(())
            }
            Err(error) => {
                // Publish the abandoned claim to the next acquiring owner;
                // no wake/IRQ resource remains installed by this attempt.
                // Publish complete resource cleanup to the next owner's Acquire CAS.
                ACTIVE.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    unsafe fn park_cpu(wait: extern "C" fn(*mut ()), argument: *mut ()) {
        // Ownership is checked here, not acquired. The BSP initialized its
        // resources before queueing Stop work; that queue publishes AP state.
        assert!(ACTIVE.load(Ordering::Relaxed) && !karch::local_irq_enabled());
        let cpu = khal::percpu::this_cpu_id().as_usize();
        assert!(cpu != 0);
        khal::quiesce_nmi();
        // SAFETY: this pinned Stop task owns its static CPU slot and callback
        // stack. The coordinator retains both until the stopper returns.
        let context = unsafe { &mut *context_address(cpu) };
        // SAFETY: the context is pinned, IRQs are disabled and callback lives
        // until the system coordinator joins this stopper.
        unsafe { context.save_and_call(wait, argument) };
        restore_local_apic();
        // Publish completed high-kernel/context/LAPIC restoration to the BSP's
        // Acquire observer before it may retarget the shared wake page.
        CPU_RESTORED[cpu].store(true, Ordering::Release);
    }

    unsafe fn enter() -> KResult {
        // Ownership is checked here, not acquired. The BSP initialized its
        // resources before queueing Stop work; that queue publishes AP state.
        assert!(ACTIVE.load(Ordering::Relaxed) && !karch::local_irq_enabled());
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), 0);
        // SAFETY: the active boot-CPU transaction exclusively owns IRQ state.
        let state = unsafe { &mut *IRQS.0.get() };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: successful prepare retains IRQ \
                      storage installation"
        )]
        let state = state.as_mut().expect("prepared IRQ state");
        // SAFETY: the coordinator parked remote CPUs and disabled local IRQs.
        unsafe { state.mask_all() };
        // SAFETY: the pinned boot-CPU slot is exclusively owned through join.
        let context = unsafe { &mut *context_address(0) };
        // SAFETY: all producers/events are excluded and the wake vector is live.
        unsafe {
            context.save_and_call(
                request_s3,
                S3_REQUEST.load(Ordering::Relaxed) as usize as *mut (),
            )
        };
        restore_local_apic();
        // Firmware may leave APs executing its final rendezvous instructions.
        // Put every AP into wait-for-SIPI before reusing the shared low page;
        // sequentially resetting only the next AP leaves the others unowned.
        x86_apic::with_local_apic(|lapic| {
            // SAFETY: all remote saved stacks belong to the active PM barrier;
            // no AP has resumed kernel execution or been released yet.
            unsafe { lapic.send_init_ipi_all() };
        });
        raw_delay(10_000);
        log::debug!("S3_RESUME: boot CPU context restored");
        // SAFETY: controller mappings are retained and remote CPUs remain reset.
        unsafe { state.restore_masked() };
        Ok(())
    }

    unsafe fn restore_cpus() -> KResult {
        // Ownership is checked here, not acquired. The BSP initialized its
        // resources before queueing Stop work; that queue publishes AP state.
        assert!(ACTIVE.load(Ordering::Relaxed) && !karch::local_irq_enabled());
        let mut result = Ok(());
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
            if cpu.as_usize() == 0 || result.is_err() {
                return;
            }
            // SAFETY: the previous wake entry has returned into high kernel
            // text. The new AP remains reset until its SIPI below.
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: ACTIVE retains the installed wake \
                          page until every remote callback joins"
            )]
            // SAFETY: ACTIVE retains this boot-owned page until AP join.
            let wake = unsafe { (*WAKE.0.get()).as_mut() }.expect("active S3 wake page");
            unsafe extern "C" {
                static s3_wake_start: u8;
                static s3_wake_context: u8;
            }
            let offset =
                ptr::addr_of!(s3_wake_context) as usize - ptr::addr_of!(s3_wake_start) as usize;
            wake.patch(offset, context_address(cpu.as_usize()) as u64);
            // Publish ordinary wake-page stores before either MMIO xAPIC or
            // x2APIC WRMSR can start a remote instruction fetch. x2APIC ICR
            // writes are not a general memory-serialization instruction.
            core::sync::atomic::fence(Ordering::SeqCst);
            let Some(raw) = kcpu_id_map::raw_cpu_id(cpu) else {
                result = Err(KError::BadState);
                return;
            };
            let id = x86_apic::raw_apic_id(raw.as_usize() as u8);
            x86_apic::with_local_apic(|lapic| {
                // SAFETY: the BSP owns this live APIC; the target is a reset AP.
                unsafe { lapic.send_init_ipi(id) };
            });
            raw_delay(10_000);
            let vector =
                (crate::peripherals::bootmem::ap_trampoline_page_paddr() / PAGE_SIZE_4K) as u8;
            for _ in 0..2 {
                x86_apic::with_local_apic(|lapic| {
                    // SAFETY: the complete wake page was installed before INIT.
                    unsafe { lapic.send_sipi(vector, id) };
                });
                raw_delay(200);
            }
            for _ in 0..10_000 {
                // Pair with the AP's Release after context/LAPIC restoration;
                // the AP no longer reads the shared low-memory wake image.
                if CPU_RESTORED[cpu.as_usize()].load(Ordering::Acquire) {
                    log::debug!("S3_RESUME: CPU {} context restored", cpu.as_usize());
                    return;
                }
                raw_delay(100);
            }
            result = Err(KError::TimedOut);
        });
        result
    }

    unsafe fn finish() {
        assert_eq!(khal::percpu::this_cpu_id().as_usize(), 0);
        // SAFETY: the caller joined all AP callbacks; no CPU can execute or
        // read the temporary wake page again after it restores the vector.
        // SAFETY: caller completed device/time recovery before finish.
        if let Some(mut state) = unsafe { (*IRQS.0.get()).take() } {
            // SAFETY: device and time recovery completed before this join.
            unsafe { state.resume() };
        }
        // SAFETY: the boot owner joined every callback before releasing WAKE.
        let wake = unsafe { (*WAKE.0.get()).take() };
        drop(wake);
        // Publish complete resource cleanup to the next owner's Acquire CAS.
        ACTIVE.store(false, Ordering::Release);
    }
}
