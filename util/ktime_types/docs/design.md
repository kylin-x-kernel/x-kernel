# ktime_types - Design Document

## Positioning

`ktime_types` provides the kernel-internal unified semantic time and frequency types. It does not read clocks and does not program hardware.
`khal::time` provides the monotonic hardware clock, and `ktime` (the timekeeping layer) is responsible for correlating monotonic and
realtime.

## Scope

- `src/clock_id.rs`: the `ClockId` semantic enum passed across modules; it is not bound to Linux ABI values.
- `src/span.rs`: `TimeSpan`, a non-negative time length.
- `src/instant.rs`: `Instant<C>`, a clock-domain instant distinguished by marker types.
- `src/system_time.rs`: signed Unix wall-clock time.
- `src/frequency.rs`: the `Frequency` type, with Hz as the canonical storage unit.
- `src/units.rs`: fixed unit conversion constants among seconds, milliseconds, microseconds and nanoseconds.
- `src/timestamp.rs`: `TimestampLimits`, the timestamp granularity and inclusive seconds range a storage or protocol format can represent.
- `src/lib.rs`: the stable public re-exports at the crate root; it does not host concrete type implementations.

## Architecture

```text
hardware frequency -> Frequency
hardware counter ---> khal::time --+-> MonotonicInstant
RTC sample ------------------------+-> SystemTime
                                      |
                                      +-> TimeSpan arithmetic
```

`MonotonicInstant`, `BoottimeInstant`, `ProcessCpuInstant` and
`ThreadCpuInstant` use distinct markers, and comparing or subtracting across clock domains is forbidden. `core::time::Duration`
is converted explicitly only through `TimeSpan::from_core` and `TimeSpan::into_core`.

## Calling Constraints / Execution Context

All types are pure value types; they do not allocate or block, and can be used in interrupt, early boot and ordinary task contexts.
`Frequency` always stores Hz internally; conversion to kHz/MHz should happen only at hardware, firmware, logging or third-party
API boundaries, and the name must make explicit that the result is rounded down.
Constructing time values from integer, ABI or hardware representations should happen only at the corresponding boundaries.
Likewise, the reverse conversion is allowed only at hardware register, ABI, serialization or third-party interface boundaries;
`as_nanos_u64_saturating` explicitly expresses the policy of clamping to `u64::MAX` when the value cannot be represented.

## Design Decisions

- `TimeSpan` is not an alias of `Duration`, to prevent dependent code from inadvertently mixing two duration types.
- `Frequency` uses Hz as the only internal representation, to avoid passing bare integers in Hz, kHz and MHz to one another;
  scaling constructors perform overflow checks, and reads with reduced precision are explicitly labeled `floor`.
- The generic clock domain of `Instant<C>` prevents mixing realtime, monotonic and CPU time at compile time.
- `SystemTime` stores normalized seconds and nanoseconds, and can represent times before the Unix epoch.
- `SystemTime::{MIN, MAX}` uniformly define the representable bounds of the wall-clock type.
- The checked addition and subtraction of `SystemTime` operate directly on the seconds and nanoseconds components, so that high-frequency wall-clock paths avoid the
  multiplication and division normalization over `i128` total nanoseconds.
- Checked and saturating operations are for external input and paths without strict ordering; operators are used only on internal paths where the ordering and range invariants have been established.

`ClockId` only identifies the kind of clock; it does not guarantee that every operation supports all kinds, nor that every identifier has an independent clock source.
Parsing of Linux numbers and conversion of the timerfd supported set are done at the `ksyscall` boundary; this crate depends on neither the Linux ABI nor file objects.
