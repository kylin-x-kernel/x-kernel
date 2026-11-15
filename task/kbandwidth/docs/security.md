# kbandwidth — Security Document

## External Boundary / Attack Surface

- `cpu.max` quota/period values parsed by the cgroup filesystem adapter.
- Scheduler-provided monotonic timestamps and elapsed runtime.
- Control-file reads of cumulative counters.

No user pointer is dereferenced here.

## Invariants

- Quota is either unlimited or at least `QUOTA_MIN_US`; period is inside
  `PERIOD_MIN_US..=PERIOD_MAX_US`.
- A rejected `set()` leaves the previous pair unchanged.
- `charge()` returns true only for the rising edge of a throttle window.
- `poll()` returns true only when an elapsed period closes an active window.
- Counters are monotonic except when a caller intentionally resets the
  accounting tree; no counter can make a task runnable by itself.

## Execution Context

`charge`/`poll` use `SpinNoIrq` and are safe from scheduler accounting context.
They must not be called with a lock that can be taken by an interrupt handler
which also enters this state machine.

## Failure Modes

| Failure | Handling |
|---|---|
| Invalid quota/period | `set()` returns false; old pair retained |
| Clock moves backwards | saturating arithmetic; no period close |
| Idle CPU with open window | every account point polls, closing it |
| Concurrent control write | atomic loads/stores, state lock |
