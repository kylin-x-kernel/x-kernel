# input — Design

## Purpose

`input` defines the contract between input device drivers (keyboards,
mice, tablets) and the kernel input layer: Linux-evdev-compatible event
types and payloads (`EventType`, `Event`, `InputDeviceId`, `AbsInfo`) and
the `InputDevice` trait that drivers implement on top of the shared
`driver_base::Device` base.

## Responsibilities

- Define `InputDevice: Device`: identity reporting (`device_id`,
  `physical_location`, `unique_id`), capability bitmap queries
  (`get_event_bits` per `EventType`, `get_prop_bits` for device
  properties), static absolute-axis metadata (`abs_info`), and non-blocking
  event fetch (`read_event`). `set_event_notifier` attaches the input
  layer's IRQ-safe scheduling capability.
- Define `InputEventNotifier` so a driver can report a possible event-queue
  completion without depending on host workqueue or IRQ APIs.
- Define the evdev-compatible wire types: `Event` (`event_type`, `code`,
  `value` — `repr(C)`; the evdev file layer adds the timestamp),
  `InputDeviceId` (`bus_type`/`vendor`/`product`/`version`), and
  `AbsInfo` (static absolute-axis `min`/`max`/`fuzz`/`flat`/`res`).
- Define `EventType`, the Linux input subsystem event categories, with the
  per-category bitmap lengths (`bits_count`) that size capability queries.

## Non-Responsibilities

- No event queueing or synthesis: the input layer owns buffering,
  repetition, and filtering; drivers hand over one event at a time.
- No user-space interface: `/dev/input` node exposure and the evdev
  `ioctl` surface belong to the device/fs layers; this crate only fixes
  the data layout.
- No device discovery or binding: the `driver_base` pipeline owns probe
  and activation.
- No bitmap storage: `get_event_bits` and `get_prop_bits` fill a
  caller-provided buffer; the crate does not allocate or cache capability
  bitmaps.
- No live axis-state storage: `abs_info` returns only immutable calibration
  metadata; the input core owns the latest scalar axis positions and
  synthesizes `EVIOCGABS` replies.

## Scope

```text
drivers/contracts/input/
├── src/
│   └── lib.rs        # EventType, Event, InputDeviceId, AbsInfo, InputDevice, notifier
└── Cargo.toml
```

## Architecture

```text
driver_base (Device base, DriverError/Result, discovery pipeline)
      ^
      |  impl InputDevice for <driver>
      |
input (this crate: event types + driver and notifier traits)
      ^
      |  read_event() / get_event_bits() / abs_info() / get_prop_bits()
      |  set_event_notifier()
      |
kernel input layer (schedules collection, queues events, exposes them upward)
```

`Event` contains the Linux event type, code, and value fields. The evdev file
layer adds the user ABI's timestamp and converts the value to `i32` when it
copies a complete `input_event` record to userspace.

## Data Model

- `EventType` (`repr(u8)`, derived `FromRepr`): the Linux categories
  (`Synchronization = 0x00` … `ForceFeedback = 0x15`), `MAX = 0x1f`,
  `COUNT = MAX + 1` slots, and `bits_count` returning the bitmap length
  per category (e.g. `Key` = 0x300 bits, `Absolute` = 0x40 bits).
- `Event`: category (as `u16`), code, and value; `is_type` compares
  against an `EventType`.
- `InputDeviceId`: `UNKNOWN` constant (all zeros) for anonymous devices.
- `AbsInfo`: immutable absolute-axis metadata (`min`, `max`, `fuzz`, `flat`,
  `res`). All five fields are Linux `__s32` values, so negative bounds and
  resolutions survive the config-space round trip without reinterpretation.
  The live axis position is intentionally absent: inputdev owns it, just as
  it owns key state. `Event::value` carries that signed value as an unsigned
  transport word.
- `ABS_MT_SLOT`, `ABS_MT_FIRST`, and `ABS_MT_LAST` name the Linux
  multi-touch selector and payload range. `ABS_MT_LAST` is `ABS_MT_TOOL_Y`
  (0x3d); codes 0x3e and 0x3f remain scalar axes. The input core never writes
  payload axes into the scalar `input_absinfo.value`; Linux keeps them in
  per-slot state outside this contract. The contract exposes the constants so
  every consumer applies the same split.
- `InputDevice::abs_info(code) -> DriverResult<Option<AbsInfo>>`: `Some`
  identifies an advertised axis and returns its metadata; `None` means the
  axis is not advertised, whether the device lacks `EV_ABS` entirely or only
  omits this code. Whether the device has `EV_ABS` at all is answered by
  `get_event_bits(EventType::Absolute, ..)`, so the driver contract has no
  third state.

## Execution Context

- `read_event` is non-blocking: no events means
  `Err(DriverError::WouldBlock)`, so the input layer decides how to wait.
- `InputEventNotifier::notify_events` is hard-IRQ safe. It only schedules
  task-context processing; it must not sleep, allocate, or read the driver.
- The input layer attaches the notifier and checks once for events that
  completed before attachment. The driver may invoke it after acknowledging
  an event-queue interrupt. Attaching and detaching occur in task context;
  the driver synchronizes both with its IRQ handler and invokes the notifier
  without holding locks needed by `read_event`.
- Methods take `&self`; drivers with shared hardware must synchronize
  internally.
- `read_event`, capability queries, and axis/property queries run in task
  context; the notification callback is the only part of this contract
  invoked from hard IRQ context.

## Known Limitations

- `EventType` defines only a subset of the Linux input categories:
  within the `0x00..=0x1f` slot range, values `0x06`-`0x10`, `0x13`,
  `0x14`, and `0x16`-`0x1f` have no variant today. Adding a category
  requires extending both the `EventType` enum and the `bit_len_of`
  bitmap-length table together; `bits_count` of an existing category
  never covers the new one.
- Event codes beyond a category's `bits_count` cannot be reported
  through `get_event_bits` bitmaps; consumers must size their buffers
  from `bits_count(ty)`.
- `abs_info` describes an axis that exists. Drivers must not synthesize
  metadata for unadvertised codes; callers interpret `None` as the Linux
  zero-filled descriptor for an axis the device does not expose.
- The contract cannot represent a per-axis metadata query failure separately
  from absence; a failed hardware query is a `DriverError`, while `None` is a
  successful "not advertised" answer.
- `get_prop_bits` exposes `INPUT_PROP_*` bits as a little-endian byte
  bitmap; the contract does not normalize the caller's buffer to a
  `usize`-rounded length, which remains the evdev layer's ABI concern.

## Error Model

All fallible operations return `driver_base::DriverResult`:

- `get_event_bits` — `Ok(true)` when the event type is supported and the
  bitmap was written into `out`; `Ok(false)` when the type is not
  supported; a `DriverError` when the hardware query itself fails.
- `abs_info` — `Ok(Some(info))` for an advertised axis, `Ok(None)` when the
  device has no such axis, and a `DriverError` when the hardware query fails.
  The default implementation returns `Ok(None)` for every code.
- `get_prop_bits` — `Ok(true)` when the driver wrote a property bitmap
  into `out` (an all-zero bitmap is still a reported capability set);
  `Ok(false)` when the device reports no property bitmap; a `DriverError`
  when the hardware query itself fails. Bits are little-endian, bit
  `INPUT_PROP_*` at byte `n / 8`, bit `n % 8`. The default implementation
  reports no bitmap, so consumers zero-fill the ABI result.
- `read_event` — `Ok(Event)` or `Err(DriverError::WouldBlock)` when no
  event is pending.
- `set_event_notifier` — accepts attach or detach, or returns a driver error
  if the device cannot provide event notifications. Its default returns
  `Unsupported`; inputdev currently rejects activation without notifications.

## Design Decisions

- Mirror the Linux evdev definitions (event types, codes, ID tuple, abs
  bits, `repr(C)` layouts): the kernel can expose a compatible interface
  to user space without translation, and Linux driver knowledge transfers
  directly.
- Caller-provided capability buffers (`get_event_bits(ty, out)`,
  `get_prop_bits(out)`) instead of owned bitmaps: bitmap sizes vary per
  event type and the input layer already knows its own allocation policy.
- Return owned `Option<AbsInfo>` from `abs_info`: `AbsInfo` is `Copy` and
  allocation-free, the option makes absence explicit without an out-parameter
  that can be mistaken for valid data, and `EV_ABS` capability stays solely in
  `get_event_bits` rather than being duplicated in a tri-state result.
- Keep live scalar axis positions out of the driver contract: inputdev
  observes every `EV_ABS` event and serves `EVIOCGABS` from its own state,
  which keeps ioctl replies coherent even when no evdev client is open and
  prevents drivers from having to mirror core state. Multi-touch payload axes
  do not enter that scalar snapshot; the core only remembers the last slot
  flushed by a payload for `ABS_MT_SLOT`.
- `WouldBlock` instead of blocking reads: consistent with the other
  driver contract crates (`char`, `net`) and keeps scheduler policy out
  of drivers.
- An IRQ-safe notifier follows the existing network receive scheduler
  contract and keeps host workerqueue policy in the input subsystem.
- Re-export `driver_base` types: one vocabulary and one import root for
  implementers, matching the sibling contract crates.

## Optional readiness hint

`InputDevice::poll_event_ready` is a bounded, allocation-free, IRQ-safe
readiness snapshot for timer-driven poll fallbacks. `None` preserves the
ordinary retry wake, `Some(false)` may suppress an idle wake, and `Some(true)`
still requires normal polling/reading to consume the event. It never
subscribes to input and never consumes an event.
