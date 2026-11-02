# KVMM MMIO Ownership Safety

This analysis covers prepared MMIO owners and GICD register storage, not the
entire hypervisor, guest isolation or world-switch assembly.

The MMIO bus consumes KBox owners prepared by the control plane before acquiring
the bus lock. Runtime self-tests likewise allocate before locking and report
preparation failure as false. GICD consumes a prepared register array and clears
it before exposing register reads, without allocating in its constructor.

Control-plane NOWAIT allocations must precede resource admission that lacks
rollback, particularly host GICH mapping. Nested Vec/Arc allocation, MMIO mapping
cleanup and vCPU hook creation retain their previous behavior. Hook factories
still run under their registry lock and return Option rather than Result;
changing their failure model is deferred rather than hidden behind a panic.
In particular, `handle_psci_cpu_on` marks the target CPU on before constructing
its Vcpu and hooks. A future fallible hook constructor must define rollback of
that state and partially prepared hooks before it can be used on this path.

Tests exercise MMIO dispatch and owner destruction, reject invalid allocation
flags, and check GICD register storage reset and read-back without host GICH
access. They do not force global OOM or validate hardware-backed boot rollback.
