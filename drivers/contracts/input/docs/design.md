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
  (`get_event_bits` per `EventType`), and non-blocking event fetch
  (`read_event`). `set_event_notifier` attaches the input layer's IRQ-safe
  scheduling capability.
- Define `InputEventNotifier` so a driver can report a possible event-queue
  completion without depending on host workqueue or IRQ APIs.
- Define the evdev-compatible wire types: `Event` (`event_type`, `code`,
  `value` — `repr(C)`; the evdev file layer adds the timestamp),
  `InputDeviceId` (`bus_type`/`vendor`/`product`/`version`), and
  `AbsInfo` (absolute-axis `min`/`max`/`fuzz`/`flat`/`res`).
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
- No bitmap storage: `get_event_bits` fills a caller-provided buffer; the
  crate does not allocate or cache capability bitmaps.

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
      |  read_event() / get_event_bits() / set_event_notifier()
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
- `read_event` and capability queries run in task context; the notification
  callback is the only part of this contract invoked from hard IRQ context.

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

## Error Model

All fallible operations return `driver_base::DriverResult`:

- `get_event_bits` — `Ok(true)` when the event type is supported and the
  bitmap was written into `out`; `Ok(false)` when the type is not
  supported; a `DriverError` when the hardware query itself fails.
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
- Caller-provided capability buffers (`get_event_bits(ty, out)`) instead
  of owned bitmaps: bitmap sizes vary per event type and the input layer
  already knows its own allocation policy.
- `WouldBlock` instead of blocking reads: consistent with the other
  driver contract crates (`char`, `net`) and keeps scheduler policy out
  of drivers.
- An IRQ-safe notifier follows the existing network receive scheduler
  contract and keeps host workerqueue policy in the input subsystem.
- Re-export `driver_base` types: one vocabulary and one import root for
  implementers, matching the sibling contract crates.
