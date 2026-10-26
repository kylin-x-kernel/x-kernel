# kiface-cpu-api Security

Mask tokens must be restored on the originating CPU in nesting order. Snapshot bodies cannot enable IRQs, block or migrate. Providers of low-level identity/masking cannot call guards that recursively use the same capability. NMI handling and AArch64 PMR semantics remain architecture-owned. Notification requires initialized delivery and does not certify remote execution.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
