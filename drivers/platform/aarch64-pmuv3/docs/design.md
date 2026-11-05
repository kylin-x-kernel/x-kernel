# PMU Counter Activation

This note covers the counter lifecycle in `src/pmuv3.rs`, not the complete
PMUv3 register API. Platform PMU ownership, GIC configuration, CPU coordination
and handler retirement remain outside this hardware counter crate.

Activation first disables the selected counter and its overflow interrupt,
configures the period/filter and clears stale overflow, then publishes software
readiness before enabling the interrupt and counter. The selected-counter stop
does not bulk-disable unrelated counters. Existing global PMCR initialization
and overflow-reset policy are unchanged.

An NMI can arrive as soon as hardware delivery is enabled, even with ordinary
IRQs masked. Publishing `enabled` last would let the NMI handler return
NotEnabled without acknowledging overflow, repeatedly interrupting the setup
that must publish readiness. Shutdown uses the reverse obligation: keep the
handler ready to acknowledge until the counter/interrupt disable writes and
ISB finish, then withdraw software readiness. The activation/stop assembly has
a memory clobber to preserve ordering against the software flag.

Linux v6.12's [ARM PMUv3 enable path](https://github.com/torvalds/linux/blob/v6.12/drivers/perf/arm_pmuv3.c)
also configures a disabled counter before enabling interrupt/counting. This is
an ordering reference, not a claim of identical perf-event ownership machinery.

The platform integration test `platforms/kplat-aarch64/ut-tests/pmu_lifecycle.rs`
arms short-period real PMU callbacks on each boot CPU and verifies global stop,
retry and repeated activation. It exposed an NMI storm during the old startup
window. QEMU validation does not replace silicon/KVM coverage.
