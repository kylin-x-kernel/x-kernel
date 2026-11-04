# Driver OS API safety

Providers own mapping/allocation validity and synchronous IRQ detach/drain;
device shutdown is a framework/driver responsibility, not inferred from Drop.

## Unsafe boundaries
- MMIO volatile accesses require a valid whole-region mapping, established by
  the unsafe MmioOp implementation. Offset
  bounds/alignment checks precede typed accesses.
- CpuBuffer retains its original Layout and provider. The unsafe provider
  contract guarantees valid, unique, aligned storage and paired release.
- Typed coherent pointer views require initialized DMA-safe values; their
  allocation must remain valid until guard release.
- Raw DMA tuples require caller pairing. Never free memory
  while hardware or escaped callbacks can still access it.

## Failure analysis
| Risk | Mitigation / owner |
|---|---|
| Wrong IRQ or shared peer removed | Preserve mapped origin and exact action token |
| Callback uses freed memory | Host detach/drain completes before release returns |
| Invalid register offset | Bounds/alignment checks before volatile access |
| Allocation failure | Typed error; failed acquisition registers nothing |
| DMA still running at drop | Driver stops hardware before cleanup |
| Invalid release context | Caller follows host provider's synchronous contract |

The existing consuming MSI free API cannot retain retryable ownership after
rejection. Network close/reset and legacy remove-error continuation are separate
known limitations, not repaired by migration. Framework cleanup registration
still assumes infallible kernel allocation policy. No credentials, persistent
data or network protocol state are stored.
