# kiface-cpu-api Security

Mask tokens must be restored on the originating CPU in nesting order. Snapshot bodies cannot enable IRQs, block or migrate. Providers of low-level identity/masking cannot call guards that recursively use the same capability. NMI handling and AArch64 PMR semantics remain architecture-owned. Notification requires initialized delivery and does not certify remote execution.

The scope includes the readiness contract in `src/lib.rs`.
`CpuInterruptReadinessIf` trusts runtime to publish each CPU only after its local
interrupt prerequisites, using release/acquire publication. Concurrent startup
may be missed but cannot expose an unprepared CPU. Readiness is not the current
IRQ mask, a delivery acknowledgement, hardware reachability, or a reclamation
barrier. A future CPU removal implementation must add exclusion against route
updates; clearing a bit alone cannot safely withdraw a routed CPU. Emergency
stop and system shutdown are not authorizations to continue retargeting IRQs.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
