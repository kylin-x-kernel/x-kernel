// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod dump;

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu};
use khal::{context::TrapFrame, percpu::this_cpu_id};

struct SnapshotSlot(UnsafeCell<Option<TrapFrame>>);

impl SnapshotSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn clear(&self) {
        // SAFETY: snapshot session setup/teardown serializes bulk reset of all
        // slots, so no reader consumes stale data concurrently with this clear.
        unsafe { *self.0.get() = None };
    }

    fn write(&self, tf: Option<TrapFrame>) {
        // SAFETY: each CPU writes only its own slot, and publication is
        // ordered by the subsequent `COLLECTED.fetch_or(..., Release)`.
        unsafe { *self.0.get() = tf };
    }

    fn read(&self) -> Option<TrapFrame> {
        // SAFETY: readers only consume a slot after observing its `COLLECTED`
        // bit with `Acquire`, which pairs with the writer's `Release`.
        unsafe { *self.0.get() }
    }
}

// SAFETY: `SnapshotSlot` is a per-CPU publication cell. Writers mutate only
// the owning CPU's slot, while readers consume the stored trap frame only
// after `COLLECTED` establishes Release/Acquire ordering for that slot.
unsafe impl Sync for SnapshotSlot {}

struct TrapFrames([SnapshotSlot; kbuild_config::NR_CPUS]);

impl TrapFrames {
    const fn new() -> Self {
        Self([const { SnapshotSlot::new() }; kbuild_config::NR_CPUS])
    }

    fn clear(&self) {
        for slot in &self.0 {
            slot.clear();
        }
    }

    fn write_current(&self, cpu_id: LogicalCpuId, tf: Option<TrapFrame>) {
        self.0[cpu_id.as_usize()].write(tf);
    }

    fn read(&self, cpu_id: LogicalCpuId) -> Option<TrapFrame> {
        self.0[cpu_id.as_usize()].read()
    }
}

static SNAPSHOT_ACTIVE: AtomicBool = AtomicBool::new(false);
static COLLECTED: AtomicUsize = AtomicUsize::new(0);
static TRAP_FRAMES: TrapFrames = TrapFrames::new();
static SNAPSHOT_SEQ: AtomicUsize = AtomicUsize::new(1);

#[cfg(feature = "ipi")]
const SNAPSHOT_WAIT_TIMEOUT_NS: usize = 200_000_000;

struct SnapshotGuard;

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        finish();
    }
}

const _: () = assert!(
    kbuild_config::NR_CPUS <= usize::BITS as usize,
    "snapshot CPU mask cannot represent all configured CPUs"
);

/// Returns a bitmask with a bit set for each present logical CPU.
fn present_mask() -> usize {
    let mut mask = 0usize;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        mask |= 1usize << cpu_id.as_usize();
    });
    mask
}

fn cpu_bit(cpu: usize) -> Option<usize> {
    if cpu >= kbuild_config::NR_CPUS {
        None
    } else {
        Some(1usize << cpu)
    }
}

fn begin() -> bool {
    if SNAPSHOT_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }

    TRAP_FRAMES.clear();
    COLLECTED.store(0, Ordering::Release);
    true
}

fn finish() {
    COLLECTED.store(0, Ordering::Release);
    TRAP_FRAMES.clear();
    SNAPSHOT_ACTIVE.store(false, Ordering::Release);
}

fn begin_guard() -> Option<SnapshotGuard> {
    begin().then_some(SnapshotGuard)
}

fn collect_local() {
    let cpu_id = this_cpu_id();
    let cpu_index = cpu_id.as_usize();
    let Some(bit) = cpu_bit(cpu_index) else {
        return;
    };

    TRAP_FRAMES.write_current(cpu_id, khal::context::active_exception_context());
    COLLECTED.fetch_or(bit, Ordering::Release);
}

fn wait_mask(timeout_ns: usize) -> usize {
    let expect = present_mask();
    let start = khal::time::monotonic_time_nanos();
    let timeout_ns = timeout_ns as u64;

    loop {
        let mask = COLLECTED.load(Ordering::Acquire);
        if mask & expect == expect {
            return mask;
        }
        if khal::time::monotonic_time_nanos().wrapping_sub(start) >= timeout_ns {
            return mask;
        }
        core::hint::spin_loop();
    }
}

fn dump_all(mask: usize, symbolize: bool) {
    let expect = present_mask();

    for_each_present_logical_cpu(|_, cpu_id, _| {
        let cpu = cpu_id.as_usize();
        let bit = 1usize << cpu;

        if expect & bit != 0 && mask & bit == 0 {
            khal::kprint_atomic!("[snapshot] cpu={cpu} NOT RESPONDING\n");
            return;
        }

        match TRAP_FRAMES.read(cpu_id) {
            Some(tf) => dump::dump_cur_task_backtrace(cpu_id, &tf, true, symbolize),
            None => khal::kprint_atomic!("[snapshot] cpu={cpu} no active trap frame\n"),
        }
        dump::dump_cpu_task_backtrace(cpu_id, true, symbolize);
    });
}

fn trigger_impl(reason: &str, collect_mask: impl FnOnce(usize) -> Option<usize>) {
    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let symbolize = backtrace::is_enabled();
    khal::kprint_atomic!("\n[snapshot {seq}] trigger={reason}\n");

    let Some(_guard) = begin_guard() else {
        khal::kprint_atomic!("[snapshot {seq}] snapshot already running\n");
        return;
    };

    if let Some(mask) = collect_mask(seq) {
        dump_all(mask, symbolize);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Trigger a global task snapshot (e.g. from sysrq).
///
/// Broadcasts an IPI to collect trap frames from all CPUs, then dumps
/// backtraces for every task. When the `ipi` feature is disabled, only the
/// local CPU is collected.
pub fn trigger(reason: &str) {
    #[cfg(feature = "ipi")]
    {
        trigger_impl(reason, |seq| {
            // `run_on_each_cpu()` executes the callback on the current CPU
            // synchronously before broadcasting to the other CPUs, so one call
            // is sufficient to collect both the triggering CPU and all remotes.
            match kipi::run_on_each_cpu(nmi_collect_local) {
                Ok(()) => Some(wait_mask(SNAPSHOT_WAIT_TIMEOUT_NS)),
                Err(err) => {
                    khal::kprint_atomic!("[snapshot {seq}] failed to broadcast snapshot: {err}\n");
                    None
                }
            }
        });
    }

    #[cfg(not(feature = "ipi"))]
    {
        trigger_impl(reason, |_seq| {
            collect_local();
            Some(COLLECTED.load(Ordering::Acquire))
        });
    }
}

/// Dump backtraces for all tasks on the given CPU (softlockup path).
///
/// Uses the current trap frame for the running task (if in interrupt context),
/// then dumps all non-running tasks.
pub fn dump_cpu_tasks(cpu_id: LogicalCpuId) {
    if let Some(tf) = khal::context::active_exception_context() {
        dump::dump_cur_task_backtrace(cpu_id, &tf, false, true);
    }
    dump::dump_cpu_task_backtrace(cpu_id, false, true);
}

/// Begin an NMI-driven snapshot session.
///
/// Returns `true` if the snapshot was successfully initiated. When `true` is
/// returned the caller is the *cause CPU* and must eventually call
/// [`nmi_finish`] after calling [`nmi_dump_all`].
pub fn nmi_begin() -> bool {
    begin()
}

/// Collect the local CPU's trap frame from NMI context.
pub fn nmi_collect_local() {
    collect_local();
}

/// Dump all CPU task backtraces from NMI context.
///
/// `mask` is the bitmap of CPUs that have collected their trap frames.
pub fn nmi_dump_all(mask: usize, symbolize: bool) {
    dump_all(mask, symbolize);
}

/// Finish an NMI-driven snapshot session.
pub fn nmi_finish() {
    finish();
}
