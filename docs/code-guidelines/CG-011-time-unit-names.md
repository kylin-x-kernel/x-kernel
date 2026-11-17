---
id: CG-011-time-unit-names
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [unitless_time_integer]
---

# CG-011-time-unit-names: Name integer time units

- **Rule**: Integer bindings, fields, and parameters whose names explicitly identify a time quantity must carry a unit suffix unless their type encodes the unit.
- **Scope**: Integer-typed bindings, fields (including enum variant fields), function parameters (including required trait method parameters), free and associated consts, and statics with a name containing one of these segments: `timeout`, `delay`, `interval`, `period`, `deadline`, or `duration`. Classification is case-insensitive while diagnostics preserve the source spelling. Accepted suffixes include `_ns`, `_us`, `_ms`, `_s`, `_sec`, `_secs`, `_second`, `_seconds`, `_tick`, `_ticks`, `_jiffies`, `_nsec`, `_usec`, `_msec`, `_cycle`, and `_cycles`. Unit-bearing types such as `Duration` and `TimeValue` are outside the integer scope. A final segment such as `count`, `len`, `length`, `num`, `index`, or `idx` classifies the name as a count/index rather than a time quantity; size and count units remain a review concern.
- **Why**: A unit in the name prevents mixing otherwise indistinguishable integer time values.
- **How to check**:
  1. Confirm the value has an integer type.
  2. Check whether its identifier contains an explicit time-quantity segment.
  3. Add a suffix naming the stored unit, or use a unit-encoding type.
- **Exceptions**: Names ending in `_address` and names containing `_or_value` are excluded because they identify ABI address/value alternatives rather than stored time quantities. `TEE_ERROR_*` names are status codes, and names ending in `_infinite` denote a timeout sentinel rather than a measured duration. Items that mirror a Linux UAPI name (ioctl numbers, flag bits, operation codes, and `#[repr(C)]` ABI fields) may keep the upstream spelling with an item-level allowance whose reason names the UAPI definition. Per-crate opt-outs follow [README.md](README.md#per-crate-opt-outs); registered targets: `krcu-core` (case (a): `period` names an RCU grace-period sequence number, not a time quantity). No other exception; use a unit-encoding type or a precise unit suffix. Consts and statics are checked, including lowercase identifiers.
- **Counter-example**:

```rust,ignore
let timeout = 250;
```

The value's unit cannot be determined from its name.
- **Example**:

```rust,ignore
let timeout_ms = 250;
```
- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs UI tests.
