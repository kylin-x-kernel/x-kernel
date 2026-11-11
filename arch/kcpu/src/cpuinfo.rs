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
