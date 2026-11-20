// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reset-aware system sleep operations supplied by the x86 platform.

#![no_std]

#[cfg(target_arch = "x86_64")]
use kerrno::KResult;

/// Reset-aware platform sleep operations for the system PM coordinator.
/// These operations do not freeze tasks, drain devices or stop DMA themselves.
#[cfg(target_arch = "x86_64")]
#[kiface::interface]
pub trait SystemSleepIf {
    /// Reads the platform clock that continues across sleep, independently of
    /// the guest monotonic mapping. Read failure leaves accounting unsettled.
    fn persistent_time() -> Option<ktime_types::SystemTime>;
    /// Claims wake resources on the boot CPU before freezing ordinary execution.
    /// # Safety
    /// Caller must serialize PM and exclude CPU hotplug until finish.
    unsafe fn prepare(sleep_types: [u8; 2]) -> KResult;
    /// Saves a remote CPU and calls its IRQ-off acknowledgment/wait callback.
    /// # Safety
    /// The callback and argument must remain valid through resumed return;
    /// local IRQs and migration must be disabled and devices remain usable.
    unsafe fn park_cpu(wait: extern "C" fn(*mut ()), argument: *mut ());
    /// Enters platform sleep on the boot CPU, returning on its saved stack.
    /// # Safety
    /// Other CPUs, devices, NMI sources, IRQs and clocks must be quiesced.
    unsafe fn enter() -> KResult;
    /// Restarts saved remote contexts, retaining their software wait barriers.
    /// # Safety
    /// Boot CPU IRQs remain disabled and all shared resources stay excluded.
    unsafe fn restore_cpus() -> KResult;
    /// Releases wake resources after all CPUs have left their resume trampoline.
    /// # Safety
    /// Caller must have joined remote CPU callbacks before releasing ownership.
    unsafe fn finish();
}
