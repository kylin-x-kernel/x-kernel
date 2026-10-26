// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{marker::PhantomData, ptr::NonNull};

pub use kcpu_id_map::LogicalCpuId as CpuId;

pub(crate) struct LocalIrqGuard {
    saved_state: usize,
    _not_send_sync: PhantomData<*mut ()>,
}

impl LocalIrqGuard {
    pub(crate) fn new() -> Self {
        Self {
            saved_state: kiface_cpu_api::CpuLocalIrqIf::save_and_disable(),
            _not_send_sync: PhantomData,
        }
    }
}

impl Drop for LocalIrqGuard {
    fn drop(&mut self) {
        kiface_cpu_api::CpuLocalIrqIf::restore(self.saved_state);
    }
}

/// Exclusive access to one CPU's slot area during CPU bring-up.
///
/// The token exists after the template has been copied and selected in the
/// architecture base register, but before the area is published for remote
/// access. Callers must explicitly publish it after CPU-local initialization.
pub struct BootCpuToken {
    pub(crate) cpu: CpuId,
    pub(crate) base: NonNull<u8>,
    pub(crate) is_published: bool,
    _not_send_sync: PhantomData<*mut ()>,
}

impl BootCpuToken {
    pub(crate) fn new(cpu: CpuId, base: NonNull<u8>) -> Self {
        Self {
            cpu,
            base,
            is_published: false,
            _not_send_sync: PhantomData,
        }
    }

    /// Returns the logical CPU whose area is being initialized.
    pub fn cpu(&self) -> CpuId {
        self.cpu
    }

    /// Publishes the initialized area for checked remote access.
    pub fn publish(mut self) -> crate::CpuArea {
        crate::layout::publish_cpu_area(&mut self)
    }
}

impl Drop for BootCpuToken {
    fn drop(&mut self) {
        // An interrupted bring-up remains INITIALIZING. Publishing a partly
        // initialized area would let remote observers see invalid state, and
        // retrying after the architecture base changed is not generally safe.
    }
}

// SAFETY: `begin_cpu_init` creates this token only while the calling CPU
// exclusively owns its unpublished area, installs `base` for that CPU, and
// makes the token !Send so the selected CPU cannot change underneath it.
unsafe impl CpuPin for BootCpuToken {
    fn current_cpu(&self) -> CpuId {
        self.cpu
    }

    fn base(&self) -> usize {
        self.base.as_ptr() as usize
    }
}

// SAFETY: CPU bring-up is exclusive and no ordinary IRQ handler may access
// the unpublished area before the token is published.
unsafe impl CpuLocalGuard for BootCpuToken {}

/// Proof that execution is pinned to one logical CPU for the guard's lifetime.
///
/// Implemented by the platform and task layers for the guards that already
/// prevent migration: `kspin::NoPreempt` / `NoPreemptIrqSave`, IRQ and trap
/// frames, and the scheduler's pinned context. Passing one to a slot accessor
/// is the only guarantee those accessors rely on; it does **not** disable
/// preemption or IRQ reentrancy by itself — that remains the caller's job
/// through the existing `kspin` guards.
///
/// # Safety
/// `current_cpu` must report the logical CPU the current execution context is
/// actually pinned to, and that must not change while the guard is alive.
/// The pinned CPU's slot area must be initialized: `begin_cpu_init` must have
/// copied the template into it and installed the architecture base register
/// before any accessor dereferences `base()`. A `base` override must name an
/// initialized slot area for that same CPU and remain valid as long as the
/// guard is alive. The default reads the architecture per-CPU base register at
/// access time, so a guard that does not override it never snapshots a base
/// that could go stale, but it still relies on that register having been
/// installed for the pinned CPU.
pub unsafe trait CpuPin {
    /// The logical CPU this context is pinned to.
    fn current_cpu(&self) -> CpuId;

    /// The per-CPU area base for this pinned context.
    ///
    /// The default reads the architecture base register. Guards that captured
    /// the base when they were created may override this to avoid a redundant
    /// read on every access; that value must refer to the same CPU reported by
    /// [`Self::current_cpu`].
    fn base(&self) -> usize {
        crate::arch::current_base()
    }
}

/// Stronger local-access capability for guards that also exclude IRQ reentry.
///
/// A [`CpuPin`] proves only that the context cannot migrate. A
/// `CpuLocalGuard` additionally proves that no local IRQ handler can run and
/// touch the same slot while the guard is alive, which is what makes the safe
/// `CpuSlot::get` and `CpuSlotCell::read`/`write`
/// accessors sound for non-`Sync` values: during the guard's lifetime, no
/// other execution context on this CPU can observe the slot at all.
///
/// The first implementor is `kspin::NoPreemptIrqSave`. `NoPreempt` and
/// `IrqSave` are deliberately **not** `CpuLocalGuard`s: the former leaves
/// local IRQs enabled, and the latter leaves preemption control to the
/// caller. IRQ and trap frames may implement it later once their reentry
/// rules are equally explicit.
///
/// # Safety
/// In addition to the [`CpuPin`] contract, the implementor's guard
/// must prevent any local interrupt handler that could access the same slot
/// from running while the guard is alive.
pub unsafe trait CpuLocalGuard: CpuPin {}
