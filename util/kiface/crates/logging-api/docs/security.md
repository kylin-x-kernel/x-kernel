# kiface-logging-api Security

This analysis covers the entire crate, `src/lib.rs`.

## Trust model

The interfaces are contracts only: `klogger` (consumer) calls them, and the
kernel runtime (`core/kruntime`) implements them through `kiface` providers. No
untrusted input crosses this crate; the risk it can carry is *contract misuse*
by either side.

## External boundary / attack surface

| Boundary | Direction | Constraint |
|----------|-----------|------------|
| `LogSinkIf::write_str` | `klogger` → provider | Called under the logger's console lock: must not sleep, block, or log again. |
| `LogContextIf::now` / `cpu_id` / `task_id` | `klogger` → provider | Identity may be absent before runtime initialization; must not invent a current task and must not recurse into logging. |
| `LogContextIf::in_nmi` | `klogger` → provider | Answers whether the caller runs in non-maskable-interrupt context. |

This crate holds no implementation state and contains no `unsafe` code.

## Thread safety

Every method may be called from any context, including interrupt and softirq
context, and concurrently from several CPUs. Because the implementations are
provider functions, the constraint is on them: they must be reentrancy-free with
respect to logging, and they must not take the logger's own locks.

## Threat analysis

| ID | Threat | Severity | Trigger | Response |
|----|--------|----------|---------|----------|
| T-01 | Recursive logging deadlocks the console lock | Medium | A `LogSinkIf` implementation logs from inside `write_str` | The contract forbids it; `klogger` additionally drops a nested record on the same CPU instead of taking its lock twice, so the failure mode is a lost record, not a hang |
| T-02 | NMI reentrancy deadlocks the logger's critical sections | High | An NMI arrives on the CPU that is holding `klogger`'s `FILTER`/`LOGBUF` locks; those masks exclude ordinary interrupts only, so the NMI handler would spin forever on a lock the interrupted context still owns | `LogContextIf::in_nmi()` tells `klogger` that it is in NMI context, and `klogger` then **skips its locks and drops the record**. Emergency output from NMI/panic context must use the platform's atomic console path (`khal::console::write_data_atomic`), not this interface |

## Fault handling

`in_nmi()` must not itself log, allocate, block, or consult a provider that can
be unavailable in NMI context; the kernel implementation reads a per-CPU flag
maintained by the interrupt core. If a build provides no `LogContextIf` at all,
the interface has no fallback: the logger may not be installed in such a build.

## Known limitations

- The interfaces carry no error path: a provider that cannot answer (`cpu_id`,
  `task_id`) returns `None`, and a sink that cannot write silently drops output.
- There is no per-message backpressure or rate limit across this boundary.
- `in_nmi()` reports *effective* NMI context on the calling CPU only; it says
  nothing about other CPUs.

## Audit checklist

- [ ] Does a new provider method stay free of logging, allocation and sleeping?
- [ ] Does a new consumer treat `None` identity as "unknown" rather than
      inventing a task?
- [ ] Is any new call from `klogger` made *before* it takes its locks, when the
      answer decides whether the locks may be taken at all?
