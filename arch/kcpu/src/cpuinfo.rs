// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-independent publication of immutable CPU identification.

use kcpu_id_map::LogicalCpuId;
use klazy::Once;

use crate::CpuInfo;

static CPU_INFO: [Once<CpuInfo>; kbuild_config::NR_CPUS] =
    [const { Once::new() }; kbuild_config::NR_CPUS];

/// Returns the architecture's immutable identity and usable-capability snapshot.
///
/// Unidentified or out-of-range logical CPUs return `None`. Reads never wait,
/// allocate, query hardware or require a current task; IRQ callers are allowed.
/// RISC-V identification uses firmware CPU nodes, so it can precede AP startup.
pub fn cpu_info(cpu_id: LogicalCpuId) -> Option<&'static CpuInfo> {
    lookup_cpu_info(&CPU_INFO, cpu_id)
}

/// Returns the calling CPU's identity and capability snapshot.
///
/// Unlike `system_hwcap` this describes the core it runs on, so callers
/// must not cache it across a migration when the answer feeds instruction
/// selection. It is still useful for diagnostics that report a specific core.
///
/// # Execution context
///
/// - **Any context that has a current CPU area**, including IRQ handlers; reads
///   the running CPU's per-CPU slot and takes no lock, allocation or task
///   reference.
/// - **Pinned to the calling CPU for the duration of the call.** The answer is
///   the core executing this function; a caller that migrates afterwards must
///   ask again rather than reuse the reference for per-core decisions.
/// - **Returns `None` before this CPU's trap initialization publishes its
///   snapshot.** Callers must handle that rather than assume identification has
///   happened.
pub fn current_cpu_info() -> Option<&'static CpuInfo> {
    let cpu_id = kcpu_slot::current_area()?.cpu();
    cpu_info(cpu_id)
}

/// Calls `visit` for every published CPU identification.
///
/// Slots are published during each CPU's trap initialization; a CPU whose trap
/// has not run yet is skipped. Callers that need a platform-wide answer must
/// account for the CPUs that are not published yet.
pub fn for_each_cpu_info(mut visit: impl FnMut(&CpuInfo)) {
    for slot in &CPU_INFO {
        if let Some(info) = slot.get() {
            visit(info);
        }
    }
}

/// The userspace-visible capability word that is safe on every online CPU.
///
/// Instruction selection must stay valid on whichever core a task is migrated
/// to, so this is the intersection of the usable-feature sets of the CPUs whose
/// identification has been published — not one core's set. Publishing a single
/// core's word would let a task that migrated to a weaker core execute an
/// instruction it does not implement. Linux derives the same answer from its
/// sanitised system-wide feature registers and parks CPUs that lack an
/// announced capability.
///
/// Identification is published per CPU during trap initialization, and boot
/// starts every present secondary CPU before the first user process, so at exec
/// time the intersection covers the whole online set. Returns zero when no CPU
/// has been identified yet.
#[cfg(target_arch = "aarch64")]
pub fn system_hwcap() -> usize {
    intersect_hwcap(CPU_INFO.iter().filter_map(Once::get).map(CpuInfo::hwcap))
}

/// Folds per-CPU capability words into the one every CPU advertises.
///
/// Split out so the fold is testable without publishing global CPU state. An
/// empty input has no CPU to agree with, so it yields zero — the same claim as
/// an unidentified system.
#[cfg(target_arch = "aarch64")]
pub(crate) fn intersect_hwcap(words: impl Iterator<Item = usize>) -> usize {
    words
        .fold(None, |acc: Option<usize>, word| {
            Some(match acc {
                Some(current) => current & word,
                None => word,
            })
        })
        .unwrap_or(0)
}

fn lookup_cpu_info(slots: &[Once<CpuInfo>], cpu_id: LogicalCpuId) -> Option<&CpuInfo> {
    slots.get(cpu_id.as_usize())?.get()
}

/// Runs on each CPU during trap initialization after its CPU area is published,
/// before scheduling or IRQ delivery. No process or locks are required; callers
/// must not migrate or sleep until hardware detection finishes.
#[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
pub(crate) fn init_current_cpu_info(detect: impl FnOnce() -> CpuInfo) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: trap initialization requires \
                  a published current CPU area"
    )]
    let area = kcpu_slot::current_area().expect("CPU area must precede CPU identification");
    publish_cpu_info(area.cpu(), detect);
}

/// Publishes once during serialized boot identification, outside IRQ handlers.
/// The CPU ID has already been validated by the CPU map. No process is needed;
/// the initializer must not sleep or recurse into this slot while holding locks.
pub(crate) fn publish_cpu_info(cpu_id: LogicalCpuId, detect: impl FnOnce() -> CpuInfo) {
    CPU_INFO[cpu_id.as_usize()].call_once(detect);
}

unittest::path_mod_test!(tests, "../ut-tests/cpuinfo.rs");
