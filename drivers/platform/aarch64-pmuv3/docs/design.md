# PMU Counter Activation

This note covers the counter lifecycle in `src/pmuv3.rs`, not the complete
PMUv3 register API. Platform PMU ownership, GIC configuration, CPU coordination
and handler retirement remain outside this hardware counter crate.

Activation first disables the selected counter and its overflow interrupt,
configures the period/filter and clears stale overflow, then publishes software
readiness before enabling the interrupt and counter. The selected-counter stop
does not bulk-disable unrelated counters. Existing global PMCR initialization
and overflow-reset policy are unchanged.

The execution filter preserves existing EL0/EL1 counting and explicitly includes
Non-secure EL2 when activation runs at EL2. VHE register aliases do not make EL2
cycles count as EL1: a zero `PMCCFILTR_EL0` excludes EL2 host cycles even when
counting and overflow interrupts are enabled. The cycle
counter writes `PMCCFILTR_EL0`; an event counter writes its selected
`PMXEVTYPER_EL0` with the event and the same EL2 inclusion policy, without
overwriting the cycle filter. This is not host/guest perf-event virtualization;
the previous EL0/EL1 coverage and shared PMCR reset policy remain unchanged.

This follows Linux's `armv8pmu_set_event_filter` EL2 inclusion for a host kernel
in hyp mode. The architectural filter is described by `PMCCFILTR_EL0.NSH` and
`PMEVTYPER<n>_EL0.NSH` (bit 27), separately from `HCR_EL2.E2H/TGE`.

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

`counter_filters_include_current_el_without_overwriting_cycle_owner` in
`platforms/kplat-aarch64/ut-tests/peripherals_pmu_shutdown.rs` checks the selected
counter's filter and preservation of the cycle filter during event activation,
with vacant pinned PMU ownership and scoped register restoration. The runtime
`real_nmi_can_publish_while_gic_lock_is_held_on_each_cpu` test verifies actual
periodic delivery while ordinary IRQs are masked; run it at both EL1 and VHE EL2.
