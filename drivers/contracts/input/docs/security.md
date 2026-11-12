# input — Security And Reliability

## Scope

This analysis covers the entire crate — the single `src/lib.rs` (the
evdev-compatible value types, `InputDevice`, and `InputEventNotifier`). The crate
holds no state and no unsafe code; the security-relevant content is the
`repr(C)` ABI contract and the obligations placed on driver
implementations.


## Trust Model

Pure contract layer: evdev-compatible value types plus the `InputDevice` and `InputEventNotifier` traits. The
crate processes no untrusted data and holds no state; its security
relevance is the data-layout contract (`repr(C)` structs crossing to user
space) and the obligations placed on driver implementations.

## External Boundaries

- `Event`, `InputDeviceId`, and `AbsInfo` are `repr(C)` contract values.
  `Event` omits the userspace `input_event` timestamp; devfs constructs
  that complete record. `AbsInfo` carries the five static Linux
  `input_absinfo` fields after `value`; the input core supplies the live
  `value` when devfs builds the user ABI record. Layout changes require
  checking the driver contract and the devfs conversion together.
- Capability queries (`get_event_bits`, `get_prop_bits`) write into
  caller-provided buffers. Only the caller knows the buffer length, so a
  driver that ignores `out.len()` is a kernel-memory corruption risk; the
  contract can only require the bound. `abs_info` returns a `Copy` value and
  owns no buffer.
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
  allocating `bits_count.div_ceil(8)` bytes must receive no more event codes than
  that bitmap can describe.
- `get_event_bits` writes only within the caller-provided `out` buffer
  and reports `Ok(false)` for unsupported types rather than touching the
  buffer.
- `abs_info` either returns the complete `AbsInfo` metadata for an
  advertised axis, `Ok(None)` for a code the device does not advertise, or a
  `DriverError`; a `Some` value never contains a partially initialized
  descriptor.
- `abs_info` never returns a live position. The core owns that value, so a
  driver cannot make `EVIOCGABS` observe stale or driver-specific state.
- `get_prop_bits` writes only within `out` and reports whether the device
  supplied a property bitmap; it does not clear the remaining bytes.
  Consumers that expose a fixed ABI length must zero the buffer before the
  call.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Layout drift from the Linux uapi structs | Medium — user-visible ABI corruption | Editing the `repr(C)` structs | `repr(C)` plus documented Linux-origin of each field; changes require an explicit ABI review. |
| T-02 | Driver writes past the capability bitmap | High — kernel buffer overflow | Misbehaving `get_event_bits` | Contract: bounded by `out.len()` and `bits_count(ty)`; enforcement by driver review (trait cannot constrain implementations). |
| T-03 | Malicious device flooding the event stream | Low — consumer starvation | Device emits events continuously | `read_event` returns one event per call with `WouldBlock` when empty; throttling is the input layer's policy. |
| T-04 | Notification re-enters a driver while its IRQ state is locked | Medium — deadlock | A notifier calls `read_event` synchronously | The notifier contract permits scheduling only; the input layer drains in task context. |
| T-05 | Driver writes past the property buffer | High — kernel buffer overflow | Misbehaving `get_prop_bits` | Contract bounds the query by the caller-provided buffer; enforcement is driver review, as with T-02. `abs_info` returns a value and has no buffer. |
| T-06 | Driver reports inverted or overflowing axis bounds | Low — userspace cursor or calibration misbehavior | Malformed hardware metadata | The contract transports signed Linux `__s32` values without clamping; the evdev layer and consumers must treat device metadata as untrusted input. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | No event pending | `DriverError::WouldBlock` | Consumer parks or polls | 3 | Explicit non-blocking contract. |
| F-02 | Hardware query failure in `get_event_bits` | `DriverError` returned | Consumer treats device as unknown-capability | 3 | Error propagation via `DriverResult`. |
| F-03 | Unsupported event type queried | `Ok(false)` | Consumer skips bitmap | 4 | Documented boolean result. |
| F-04 | Axis queried that the device does not expose | `Ok(None)` | Consumer returns a zero-filled descriptor and succeeds | 4 | `None` explicitly means "not advertised"; devfs zero-fills the whole Linux `input_absinfo` slot. |
| F-05 | Hardware query fails in `abs_info`/`get_prop_bits` | `DriverError` returned | Consumer fails the ioctl with `EIO` | 3 | Error propagation via `DriverResult`; devfs logs and does not mask the failure as an absent capability. |

## Known Limitations

- The bitmap length table is a fixed snapshot of the Linux categories;
  future event types require extending `EventType` and `bit_len_of`
  together, or the two can drift.
- `AbsInfo` uses signed `i32` fields matching Linux `__s32`. Drivers that
  decode transport words must preserve the two's-complement bit pattern;
  narrowing or clamping negative bounds corrupts the userspace ABI silently.
- The crate stores no live axis values. `EVIOCGABS` correctness depends on
  every `EV_ABS` event reaching inputdev, which owns the scalar snapshots and
  must keep them coherent with the events it accepts. Multi-touch payload
  axes remain per-slot in Linux; inputdev deliberately keeps them out of the
  scalar snapshot and exposes `ABS_MT_SLOT` as the last flushed slot.

## Audit Checklist

- `EventType` values still match the Linux constants.
- `bit_len_of` covers every variant exhaustively (compile-time enforced
  by the match).
- New `repr(C)` structs are checked against their Linux counterparts.

## Optional readiness hint

Consumers may call the readiness hint from timer/IRQ context. Implementations
must not sleep, allocate, copy user memory or consume events, and must release
internal locks before returning. False negatives can stall input, so uncertain
state must return `Some(true)` or `None`; false positives are harmless because
the caller rechecks through the ordinary read/poll path.
