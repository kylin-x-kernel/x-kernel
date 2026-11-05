# Counter Lifecycle Boundary

Scope: activation, software readiness and stop in `src/pmuv3.rs`. Other register
wrappers and general PMU resource arbitration are not audited by this note.

Callers validate counter support, pin execution to its owning CPU and serialize
configuration and lifetime. The `enable`/`disable` assembly writes only the
selected counter's system-register enable bits at the required privilege level.
Omitting `nomem` is intentional: memory publication must not move past delivery
activation, nor may readiness withdrawal move before hardware stop. ISB completes
the local context synchronization; it is not remote CPU acknowledgement.

The handler must remain capable of clearing overflow whenever production and
overflow IRQ delivery are enabled. Breaking this ordering can cause an NMI storm
that prevents startup/stop from progressing. Software enabled state is neither
a source ownership lock nor proof of GIC quiescence. A disabled counter can still
have an overflow flag or a controller claim pending/active. The platform must
clear source overflow and complete controller teardown before freeing counter
storage, and separately drain software readers before releasing shared handlers.

The real platform lifecycle regression covers pseudo-NMI delivery under QEMU;
it does not certify hardware-NMI silicon, KVM quirks or arbitrary perf counters.
