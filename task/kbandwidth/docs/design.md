# kbandwidth — Design Document

## Positioning

`kbandwidth` owns the `cpu.max` quota/period state machine for one cgroup. It
does not own task lists or scheduler queues; `ktask::run_queue::bandwidth`
translates charge/poll outcomes into parked scheduler entities.

## State Machine

- `quota_us == u64::MAX` means Linux `max` (unlimited).
- A period starts on the first charge/poll and is anchored to the monotonic
  clock in nanoseconds.
- `charge(runtime, now)` rolls expired periods, adds runtime, and returns true
  only when this charge starts a throttle window.
- `poll(now)` advances the period without charging and returns true when it
  closes a throttle window. Every scheduler account point polls, including idle
  runners, so a window cannot remain open solely because no member runs.
- Changing quota/period preserves the current period anchor; the new limit is
  observed by the next charge.

## Counters

`nr_periods`, `nr_throttled`, and `throttled_usec` are cumulative scheduler
statistics. `usage_us` is the cumulative charged runtime. Counters use
`AcqRel` updates because they are read by control files while scheduler CPUs
charge concurrently; no other payload is published by these counters.

## Locking

One IRQ-safe spinlock protects the period/anchor/runtime/throttle state. The
lock is held only for the state-machine update; it never calls the scheduler,
allocates, or sleeps. `charge`/`poll` are callable from the scheduler tick
path.

## Non-goals

- Task membership tracking.
- Weight/nice translation.
- Reclaim or OOM policy.
