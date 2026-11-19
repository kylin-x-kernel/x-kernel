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
- `MmioClaimOp::release_mmio` is unsafe: the token must be an unreleased
  acquisition from the same provider. MmioClaim releases exactly once on drop
  and never constructs a token itself.

## Failure analysis
| Risk | Mitigation / owner |
|---|---|
| Wrong IRQ or shared peer removed | Preserve mapped origin and exact action token |
| Firmware cells become a wrong logical IRQ | Provider maps by exact controller node; malformed/unsupported/not-ready/conflict errors are distinct; no raw-number fallback |
| Callback uses freed memory | Host detach/drain completes before release returns |
| Invalid register offset | Bounds/alignment checks before volatile access |
| Allocation failure | Typed error; failed acquisition registers nothing |
| DMA still running at drop | Driver stops hardware before cleanup |
| Invalid release context | Caller follows host provider's synchronous contract |
| Two participants access one MMIO range | Token claim refuses any overlap while the first owner lives; failure transfers nothing and release is by exact token |
| Claim mistaken for system-wide exclusivity | Only claim participants are arbitrated; other mapping entry points and non-participating drivers remain outside the contract |
| DMA view silently widened | Explicit identity/window constructors; mask validated as contiguous low bits; whole-interval translation checks window and mask; no missing-firmware default |

MSI `try_free_msix` rejection must have no release side effects and returns the
unchanged resource to its owner. Only success consumes ownership. The terminal
`free_msix` wrapper panics on rejection; it is not a retry or recovery mechanism.
The caller must stop the source, order outstanding messages, detach/drain actions,
disable the line and serialize ownership before task-context release. No spinlock,
IRQ/preemption exclusion or RCU read scope may span release; host retirement
services must be running. Neither this contract nor action drain proves DMA stop.
Network close/reset, fallible action teardown and legacy remove-error continuation
remain separate limitations. Framework cleanup registration
still assumes infallible kernel allocation policy. No credentials, persistent
data or network protocol state are stored.

## Monotonic clock context

TimeOp::monotonic_time is non-allocating and non-sleeping in task, IRQ and
softirq context, including under a short IRQ-safe block-accounting lock. Hosts
must not implement this operation with a sleepable lock or task-only resource.
