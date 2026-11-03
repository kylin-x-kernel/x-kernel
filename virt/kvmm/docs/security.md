# KVMM Memory Ownership Safety

This analysis covers prepared MMIO owners, GICD register storage and page-table
storage lifetime transfers. It does not cover the entire hypervisor, guest
isolation or world-switch assembly.

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

## Page-table storage lifetime transfers

The unsafe `free_pt_page` boundary in `src/mm/mod.rs` reconstructs a one-page
`PageRef` with `PageRef::from_raw((pa as usize).into(), 1)` and drops it. The
physical address must identify exactly one outstanding, unconsumed
`PageRef::into_raw` transfer of
length one from the same allocation incarnation. The caller must end hardware
use of that table page and complete all required translation retirement before
calling; this boundary consumes the transfer exactly once. An address lookup
alone cannot establish ownership or hardware quiescence.

The EPT, Stage-2 and G-stage page-table builders transfer child-page holds into
raw addresses stored in parent entries. Their destruction paths call
`free_pt_page` for those child pages, while root-page fields retain their own
`PageRef` holders. These ownership holders protect storage lifetime; they do not
perform translation retirement or prove that a vCPU has stopped using a table.
