// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal GICv2 vGIC — injects virtual interrupts via the GICH list registers.
//!
//! Bring-up sandbox for the kvmm interrupt path. Pending and enabled bitmaps
//! for the first 64 virtual IRQs are reconciled into the per-physical-CPU GICH
//! list registers on guest entry and read back on exit, all inside the
//! IRQ-masked world-switch window (via [`VgicHook`]).
//!
//! The guest's GIC **CPU interface** (GICC) is the hardware **GICV** mapped in
//! Stage-2, so ack/EOI are hardware-assisted; only the distributor (GICD) is
//! software-emulated (see [`super::vgicd`]).

use alloc::sync::{Arc, Weak};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU16, AtomicU64, Ordering},
};

use crate::{
    arch::aarch64::Aarch64Vhe,
    vcpu::{MAX_VCPUS, Vcpu},
    vdev::{IrqController, IrqSender, VcpuHook, VcpuHookFactory, VcpuWaker},
};

// GICH (hypervisor control) register offsets.
const GICH_HCR: usize = 0x000;
const GICH_VMCR: usize = 0x008;
const GICH_ELSR0: usize = 0x030;
const GICH_APR: usize = 0x0f0;
const GICH_LR0: usize = 0x100;

const MAX_LRS: usize = 4;
const HCR_EN: u32 = 1;
const SGI_MASK: u64 = 0xffff;
const FIRST_HOST_BACKED_GUEST_IRQ: u32 = 17;

// GICH_LR (GICv2, 32-bit): [31]=HW, [29:28]=State, [27:23]=Priority,
// [19:10]=PhysicalID when HW=1, [9:0]=VirtualID.
const LR_HW: u32 = 1 << 31;
const LR_STATE_PENDING: u32 = 1 << 28;
const LR_PRIORITY: u32 = 0x14 << 23;
const LR_STATE_MASK: u32 = 0x3 << 28;
const LR_PHYSID_SHIFT: u32 = 10;
const LR_SGI_SOURCE_SHIFT: u32 = 10;
const LR_VINTID_MASK: u32 = 0x3ff;

/// Highest virtual IRQ this minimal vGIC tracks (SGI/PPI + first SPIs).
const MAX_IRQS: u32 = 64;

/// Per-vCPU GICH hardware state, saved on exit and reloaded on entry so the
/// guest's virtual CPU-interface control (VMCR: virtual PMR/enable/EOImode),
/// active priorities (APR), and list registers survive VM exits and pCPU
/// migration.
#[derive(Clone, Copy)]
struct Hw {
    vmcr: u32,
    apr: u32,
    lr: [u32; MAX_LRS],
}

struct Core {
    /// Pending virtual IRQs (bit N = IRQ N). Set from any CPU; drained on entry.
    pending: AtomicU64,
    /// Per-SGI source CPU pending masks. GICv2 tracks SGI pending state per
    /// source CPU, and exposes the source in LR[12:10]/IAR[12:10].
    sgi_sources: [AtomicU16; 16],
    /// Banked SGI/PPI enable state for this vCPU (IRQs 0..31).
    local_enabled: AtomicU64,
    /// Cached GICH state, owned by the running vCPU thread.
    hw: UnsafeCell<Hw>,
}

/// Per-VM virtual GIC (GICH list-register injector).
pub struct Vgic {
    gich_va: usize,
    nr_vcpus: usize,
    cores: [Core; MAX_VCPUS],
    waker: Weak<dyn VcpuWaker>,
    /// Shared SPI enable state (IRQs 32..63 in this bring-up model).
    shared_enabled: AtomicU64,
}

// SAFETY: `hw` is only accessed by the owning vCPU thread inside the IRQ-masked
// world-switch window (`VgicHook::on_entry`/`on_exit`), so there is no
// concurrent access to the `UnsafeCell`. Pending and enable state are atomics.
unsafe impl Sync for Vgic {}
// SAFETY: see the `Sync` justification above — non-atomic interior mutability
// is confined to the owning vCPU thread's world-switch window.
unsafe impl Send for Vgic {}

impl Vgic {
    /// Create a vGIC. `gich_va` is the kernel VA of the mapped GICH MMIO base.
    pub fn new(nr_vcpus: usize, gich_va: usize, waker: Weak<dyn VcpuWaker>) -> Arc<Self> {
        Arc::new(Self {
            gich_va,
            nr_vcpus,
            cores: core::array::from_fn(|_| Core {
                pending: AtomicU64::new(0),
                sgi_sources: [const { AtomicU16::new(0) }; 16],
                local_enabled: AtomicU64::new(SGI_MASK),
                hw: UnsafeCell::new(Hw {
                    vmcr: 0,
                    apr: 0,
                    lr: [0; MAX_LRS],
                }),
            }),
            waker,
            shared_enabled: AtomicU64::new(0),
        })
    }

    /// Mark `irq` pending for `vcpu`.
    ///
    /// Delivery to GICH LRs happens only when the guest-visible enable bit is
    /// set for the target vCPU.
    pub fn set_pending(&self, vcpu: u32, irq: u32) {
        self.set_pending_from(vcpu, irq, 0);
    }

    /// Mark `irq` pending for `vcpu`, preserving SGI source CPU ID when known.
    pub fn set_pending_from(&self, vcpu: u32, irq: u32, source_vcpu: u32) {
        if (vcpu as usize) < self.nr_vcpus && irq < MAX_IRQS {
            if irq < 16 {
                // let n = SGI_PENDING_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                // if n < 128 || n.is_power_of_two() {
                //     log::info!(
                //         "[vgic] SGI pending#{n}: src_vcpu={} target_vcpu={} irq={}",
                //         source_vcpu,
                //         vcpu,
                //         irq,
                //     );
                // }
                self.cores[vcpu as usize].sgi_sources[irq as usize]
                    .fetch_or(1u16 << source_vcpu.min(15), Ordering::Release);
            }
            self.cores[vcpu as usize]
                .pending
                .fetch_or(1u64 << irq, Ordering::Release);
            if let Some(waker) = self.waker.upgrade() {
                waker.wake_vcpu(vcpu);
            }
        }
    }

    /// Enable or disable `irq` for `vcpu` according to GIC distributor state.
    pub fn set_enabled(&self, vcpu: u32, irq: u32, enabled: bool) {
        if irq >= MAX_IRQS {
            return;
        }
        if irq < 16 {
            return;
        }

        let mask = 1u64 << irq;
        let state = if irq < 32 {
            if (vcpu as usize) >= self.nr_vcpus {
                return;
            }
            &self.cores[vcpu as usize].local_enabled
        } else {
            &self.shared_enabled
        };

        if enabled {
            state.fetch_or(mask, Ordering::Release);
        } else {
            state.fetch_and(!mask, Ordering::Release);
        }
    }

    /// Return the guest-visible enable state for `irq` on `vcpu`.
    pub fn is_enabled(&self, vcpu: u32, irq: u32) -> bool {
        if irq >= MAX_IRQS {
            return false;
        }

        let enabled = if irq < 32 {
            if (vcpu as usize) >= self.nr_vcpus {
                return false;
            }
            self.cores[vcpu as usize]
                .local_enabled
                .load(Ordering::Acquire)
                | SGI_MASK
        } else {
            self.shared_enabled.load(Ordering::Acquire)
        };
        enabled & (1u64 << irq) != 0
    }

    /// Return a GICD_ISENABLER/ICENABLER word for `vcpu`.
    pub fn enabled_word(&self, vcpu: u32, word: usize) -> u32 {
        match word {
            0 if (vcpu as usize) < self.nr_vcpus => {
                (self.cores[vcpu as usize]
                    .local_enabled
                    .load(Ordering::Acquire)
                    | SGI_MASK) as u32
            }
            1 => (self.shared_enabled.load(Ordering::Acquire) >> 32) as u32,
            _ => 0,
        }
    }

    #[inline]
    fn gich_write(&self, off: usize, val: u32) {
        // SAFETY: `gich_va + off` is within the mapped GICH MMIO page.
        unsafe { ((self.gich_va + off) as *mut u32).write_volatile(val) };
    }

    #[inline]
    fn gich_read(&self, off: usize) -> u32 {
        // SAFETY: `gich_va + off` is within the mapped GICH MMIO page.
        unsafe { ((self.gich_va + off) as *const u32).read_volatile() }
    }

    fn pending_lr_value(irq: u32, source_vcpu: u32) -> u32 {
        let mut lr = LR_PRIORITY | LR_STATE_PENDING | irq;
        if irq < FIRST_HOST_BACKED_GUEST_IRQ {
            if irq < 16 {
                lr |= source_vcpu.min(7) << LR_SGI_SOURCE_SHIFT;
            }
            return lr;
        }
        if let Some(host_hwirq) = host_hwirq_for_guest_irq(irq) {
            lr |= LR_HW | (host_hwirq << LR_PHYSID_SHIFT);
        }
        lr
    }
}

fn host_hwirq_for_guest_irq(guest_irq: u32) -> Option<u32> {
    super::irq_route::host_hwirq_for_guest_irq(guest_irq)
}

impl IrqSender for Vgic {
    fn inject(&self, vcpu: u32, irq: u32) {
        self.set_pending(vcpu, irq);
    }
}

impl IrqController for Vgic {
    fn inject_irq(&self, vcpu_id: u32, irq: u32) {
        self.set_pending(vcpu_id, irq);
    }
}

/// Hook factory for a VM's vGIC instance.
pub struct VgicHookFactory {
    vgic: Arc<Vgic>,
}

impl VgicHookFactory {
    pub fn new(vgic: Arc<Vgic>) -> Self {
        Self { vgic }
    }
}

impl VcpuHookFactory<Aarch64Vhe> for VgicHookFactory {
    fn make_vcpu_hook(&self, _vcpu_id: u32) -> Option<alloc::boxed::Box<dyn VcpuHook<Aarch64Vhe>>> {
        Some(alloc::boxed::Box::new(VgicHook::new(self.vgic.clone())))
    }
}

/// Per-vCPU world-switch hook that syncs the vGIC pending set with the GICH
/// list registers around guest entry/exit.
pub struct VgicHook {
    vgic: Arc<Vgic>,
}

impl VgicHook {
    pub fn new(vgic: Arc<Vgic>) -> Self {
        Self { vgic }
    }
}

impl VcpuHook<Aarch64Vhe> for VgicHook {
    fn on_entry(&mut self, vcpu: &mut Vcpu<Aarch64Vhe>) {
        let vcpu_id = vcpu.vcpu_id;
        let vgic = &self.vgic;
        if vcpu_id as usize >= vgic.nr_vcpus {
            return;
        }
        let core = &vgic.cores[vcpu_id as usize];
        // SAFETY: single accessor inside the IRQ-masked world-switch window.
        let hw = unsafe { &mut *core.hw.get() };
        let lr = &mut hw.lr;

        // Drain only enabled pending bits into free LR slots. Disabled pending
        // lines remain latched and become deliverable if the guest enables them.
        let enabled = (core.local_enabled.load(Ordering::Acquire) | SGI_MASK)
            | vgic.shared_enabled.load(Ordering::Acquire);
        let pending_all = core.pending.load(Ordering::Acquire);
        let mut pending = pending_all & enabled;
        while pending != 0 {
            let irq = pending.trailing_zeros();
            pending &= pending - 1;

            if irq < 16 {
                let mut sources = core.sgi_sources[irq as usize].load(Ordering::Acquire);
                while sources != 0 {
                    let source_vcpu = sources.trailing_zeros();
                    sources &= sources - 1;

                    // Skip if this SGI/source pair is already queued in an LR.
                    if lr.iter().any(|&l| {
                        l & LR_VINTID_MASK == irq
                            && ((l >> LR_SGI_SOURCE_SHIFT) & 0x7) == source_vcpu
                            && l & LR_STATE_MASK != 0
                    }) {
                        continue;
                    }

                    match lr.iter().position(|&l| l & LR_STATE_MASK == 0) {
                        Some(slot) => {
                            core.sgi_sources[irq as usize]
                                .fetch_and(!(1u16 << source_vcpu), Ordering::AcqRel);
                            if core.sgi_sources[irq as usize].load(Ordering::Acquire) == 0 {
                                core.pending.fetch_and(!(1u64 << irq), Ordering::AcqRel);
                            }
                            lr[slot] = Vgic::pending_lr_value(irq, source_vcpu);
                            // let n = SGI_LR_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                            // if n < 128 || n.is_power_of_two() {
                            //     log::info!(
                            //         "[vgic] SGI LR#{n}: vcpu={} irq={} src={} slot={} lr={:#x} \
                            //          enabled={:#x} pending_all={:#x}",
                            //         vcpu_id,
                            //         irq,
                            //         source_vcpu,
                            //         slot,
                            //         lr[slot],
                            //         enabled,
                            //         pending_all,
                            //     );
                            // }
                        }
                        None => break,
                    }
                }
                continue;
            }

            // Skip if this IRQ is already queued in an LR.
            if lr
                .iter()
                .any(|&l| l & LR_VINTID_MASK == irq && l & LR_STATE_MASK != 0)
            {
                continue;
            }
            match lr.iter().position(|&l| l & LR_STATE_MASK == 0) {
                Some(slot) => {
                    core.pending.fetch_and(!(1u64 << irq), Ordering::AcqRel);
                    lr[slot] = Vgic::pending_lr_value(irq, 0);
                }
                None => {
                    break;
                }
            }
        }

        // Restore the virtual CPU-interface control (VMCR) and active
        // priorities (APR) before the LRs, so guest PMR/enable/priority state
        // survives exits and pCPU migration.
        vgic.gich_write(GICH_VMCR, hw.vmcr);
        vgic.gich_write(GICH_APR, hw.apr);
        for (i, &l) in lr.iter().enumerate() {
            vgic.gich_write(GICH_LR0 + i * 4, l);
        }
        vgic.gich_write(GICH_HCR, HCR_EN);
    }

    fn on_exit(&mut self, vcpu_id: u32) {
        let vgic = &self.vgic;
        if vcpu_id as usize >= vgic.nr_vcpus {
            return;
        }

        let core = &vgic.cores[vcpu_id as usize];
        // SAFETY: single accessor inside the IRQ-masked world-switch window.
        let hw = unsafe { &mut *core.hw.get() };

        // Save the virtual CPU-interface state (VMCR/APR) and list registers so
        // they can be reloaded on the next entry (possibly on another pCPU).
        hw.vmcr = vgic.gich_read(GICH_VMCR);
        hw.apr = vgic.gich_read(GICH_APR);
        let elsr0 = vgic.gich_read(GICH_ELSR0);
        for (i, l) in hw.lr.iter_mut().enumerate() {
            *l = vgic.gich_read(GICH_LR0 + i * 4);
            let irq = *l & LR_VINTID_MASK;
            if irq < 16 && *l & LR_STATE_MASK != 0 {
                if *l & LR_STATE_PENDING != 0 {
                    let source_vcpu = ((*l >> LR_SGI_SOURCE_SHIFT) & 0x7) as u16;
                    core.sgi_sources[irq as usize].fetch_or(1u16 << source_vcpu, Ordering::Release);
                    core.pending.fetch_or(1u64 << irq, Ordering::Release);
                }
                *l = 0;
                vgic.gich_write(GICH_LR0 + i * 4, 0);
                continue;
            }
            // Fully clear inactive (empty) LR slots — a stale VINTID left in an
            // inactive LR plus a live one with the same VINTID is UNPREDICTABLE
            // in GICv2.
            if elsr0 & (1 << i) != 0 {
                *l = 0;
            }
            vgic.gich_write(GICH_LR0 + i * 4, 0);
        }
        vgic.gich_write(GICH_VMCR, 0);
        vgic.gich_write(GICH_APR, 0);
        vgic.gich_write(GICH_HCR, 0);
    }
}
