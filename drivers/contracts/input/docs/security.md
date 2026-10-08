# input — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs` (the
evdev-compatible value types, `InputDevice`, and `InputEventNotifier`). The crate
holds no state and no unsafe code; the security-relevant content is the
`repr(C)` ABI contract and the obligations placed on driver
implementations.


## Trust Model

Pure contract layer: evdev-compatible value types plus one trait. The
crate processes no untrusted data and holds no state; its security
relevance is the data-layout contract (`repr(C)` structs crossing to user
space) and the obligations placed on driver implementations.

## External Boundaries

- `Event`, `InputDeviceId`, and `AbsInfo` are `repr(C)` contract values.
  `Event` omits the userspace `input_event` timestamp; devfs constructs
  that complete record. Layout changes require checking both the driver
  contract and the devfs conversion.
- Driver implementations fetch events from real hardware; hardware event
  payloads (`code`, `value`) are device-controlled input once they reach
  consumers, but sanitization belongs to the input layer, not here.
- Drivers may call `InputEventNotifier` from hard IRQ context. An
  implementation must schedule work without sleeping, allocating, or
  re-entering the driver.

## Unsafe Code

None. The crate contains no `unsafe`, no FFI, and no inline assembly.

## Invariants

- `EventType` discriminants match the Linux input subsystem constants
  (`Synchronization = 0x00` ... `ForceFeedback = 0x15`); `MAX = 0x1f`.
- `bits_count` returns the exact bitmap length per category; a consumer
  allocating `bits_count/8` bytes must receive no more event codes than
  that bitmap can describe.
- `get_event_bits` writes only within the caller-provided `out` buffer
  and reports `Ok(false)` for unsupported types rather than touching the
  buffer.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Layout drift from the Linux uapi structs | Medium — user-visible ABI corruption | Editing the `repr(C)` structs | `repr(C)` plus documented Linux-origin of each field; changes require an explicit ABI review. |
| T-02 | Driver writes past the capability bitmap | High — kernel buffer overflow | Misbehaving `get_event_bits` | Contract: bounded by `out.len()` and `bits_count(ty)`; enforcement by driver review (trait cannot constrain implementations). |
| T-03 | Malicious device flooding the event stream | Low — consumer starvation | Device emits events continuously | `read_event` returns one event per call with `WouldBlock` when empty; throttling is the input layer's policy. |
| T-04 | Notification re-enters a driver while its IRQ state is locked | Medium — deadlock | A notifier calls `read_event` synchronously | The notifier contract permits scheduling only; the input layer drains in task context. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No event pending | `DriverError::WouldBlock` | Consumer parks or polls | 3 | Explicit non-blocking contract. |
| F-02 | Hardware query failure in `get_event_bits` | `DriverError` returned | Consumer treats device as unknown-capability | 3 | Error propagation via `DriverResult`. |
| F-03 | Unsupported event type queried | `Ok(false)` | Consumer skips bitmap | 4 | Documented tri-state result. |

## Known Limitations

- The bitmap length table is a fixed snapshot of the Linux categories;
  future event types require extending `EventType` and `bit_len_of`
  together, or the two can drift.

## Audit Checklist

- `EventType` values still match the Linux constants.
- `bit_len_of` covers every variant exhaustively (compile-time enforced
  by the match).
- New `repr(C)` structs are checked against their Linux counterparts.
