# ktime_types - Security and Reliability Analysis

## Trust Model

`ktime_types` trusts callers to use the integer conversion APIs only at real ABI, hardware, protocol, or serialization boundaries.
The types themselves are responsible for canonicalizing the representation and for preventing direct arithmetic across different clock domains.

## External Boundaries / Attack Surface

This crate does not access user memory, devices, the network, or the filesystem. External input enters only through constructors;
the calling boundary must first validate the range of seconds and nanoseconds, as well as signedness.

## Memory Safety Invariants

There is no `unsafe` code. All types are composed of plain Rust values.

## Thread Safety

The types contain no interior mutable state and can be copied across threads and CPUs. Synchronization of clock sources is the responsibility of the subsystem that supplies these values.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Different clock domains are treated as the same timeline | Medium | Bare integers are used to bypass the types | Internal APIs use `Instant<C>`; integer conversion is confined to boundaries |
| T-02 | External time values overflow | Medium | Unvalidated, excessively large duration or timestamp | Provide checked/saturating APIs; boundaries reject or clamp the input |
| T-03 | A wall-clock step backward is treated as a positive duration | Medium | realtime time ordering is reversed | `SystemTime::duration_since` returns an error instead of underflowing |
| T-04 | Bare integers in Hz, kHz, and MHz are mixed up | Medium | Firmware or hardware boundary uses the wrong unit | `Frequency` canonically stores Hz; scaled constructors check for overflow, and precision-reducing conversions state round-down in their names |

## Known Limitations

`TimeSpan` can only express non-negative durations; protocols that need signed differences must model them separately at the protocol boundary.

## Audit Checklist

- Whether a new API returns a semantic type rather than an integer carrying a unit.
- Whether both operands of an `Instant` operation belong to the same clock domain.
- Whether raw integer conversions sit right next to an ABI, hardware, serialization, or third-party interface boundary.
- Whether frequencies are passed internally as `Frequency`, and whether unit conversions use named conversion methods.
- Whether boundaries that use saturating conversions explicitly accept `u64::MAX` as the clamped result for an unrepresentable value.

`ClockId` provides no raw integer conversion and specifies no ABI layout. Validation of external numbering and the check of the set of supported operations are the responsibility of the
system call adaptation layer; an enum value being valid does not mean that a given operation supports that clock.
