# inputdev — Design

## Fallible work callbacks

`InputEndpoint::new` allocates its erased work callback with KBox NOWAIT, because
`input_endpoint` holds INPUT_ENDPOINTS across creation/start to serialize endpoint
identity. Merely being a sleepable mutex does not establish reclaim safety:
endpoint discovery/removal also needs this registry. NOWAIT preserves the current
no-reclaim owner allocation without introducing synchronous subsystem re-entry.
NOFS/NOIO still allow reclaim and do not prove safety for this input registry;
restructuring creation outside the lock would require duplicate-publication and
notifier-lifetime handling and is not part of this migration. The whole API still
requires sleepable task context for worker/device operations, not IRQ context.

## Purpose

`inputdev` owns event progress and distribution after an input class device
becomes active. It drains completed driver events even when no evdev file is
open, tracks device-wide key state and the latest scalar absolute-axis
positions, and delivers complete frames to independent listeners. Hardware probing and user-facing file semantics stay in the driver
framework and devfs respectively.

## Architecture

```text
VirtIO input IRQ -> InputEventNotifier -> InputEndpoint.poller (WorkHandle)
                                            |
                                            v
                                InputDevice::read_event()
                                            |
                    frame, key-state, and abs-value updates
                                            |
                          InputEventListener::receive_frame()
                                            |
                          independent devfs open-file queues
```

`INPUT_ENDPOINTS` holds at most one `InputEndpoint` per driver-core device id.
`init_input` subscribes to removal and availability, then visits the existing
class snapshot. `input_endpoint` also creates an endpoint on demand because
device-node publication can precede the class availability callback. The same
mutex serializes both registration paths and prevents duplicate workers.

Each endpoint prepares a `kworkng::WorkHandle` in task context, attaches a
weak-backed `InputEventNotifier` to the class device, then schedules one initial
round.
That round drains events completed before notifier attachment. A hardware
interrupt only schedules the worker. A pending work occurrence coalesces
additional notifications; a notification during execution can queue a follow-up.
Task-context rounds read up to 64 events, with at most two immediate rounds
before queuing another occurrence and yielding to another work item.

The endpoint buffers events until `EV_SYN/SYN_REPORT` and distributes each
frame to the current listeners. A listener added during a partial frame starts
with the next frame. If a device sends more than 256 events without a report,
the endpoint discards the malformed frame until its next report and delivers
`EV_SYN/SYN_DROPPED` followed by `SYN_REPORT`. Device-wide pressed-key state
still tracks valid key transitions while events are drained. Accepted `EV_ABS`
events update the endpoint's scalar axis positions, including events that are
discarded from an oversized frame, except for multi-touch payload axes
(`ABS_MT_FIRST..=ABS_MT_LAST`). Linux keeps those in per-slot state and does not
write them to `input_absinfo.value`. `ABS_MT_SLOT` is a selector: Linux ignores
values outside `0..=abs_info(ABS_MT_SLOT).max`, so the endpoint caches that
maximum during activation. Its scalar value is committed when the next MT
payload flushes a valid new slot. The signed event payload is preserved as an
`i32`, matching Linux `input_absinfo.value`.

The endpoint exposes snapshots rather than synchronization objects:
`key_state()` returns the `EVIOCGKEY` bitmap and `abs_value(code)` returns one
scalar axis position, or `None` for a code outside `ABS_CNT`. Multi-touch
payload axes read as zero; `ABS_MT_SLOT` reads as the last flushed slot. devfs
combines the value with the driver's static `AbsInfo` metadata when serving
`EVIOCGABS`. Keeping live scalar positions here makes the reply independent of
whether an evdev file is open and prevents each driver from mirroring core
state.

## Event timestamps

Each successful driver read captures one `ktime::realtime_and_monotonic`
sample. `TimedInputEvent` carries both values through frame assembly and
broadcast, including synthetic overflow markers. These are input-layer receipt
times, not hardware-provided timestamps. A delayed listener must use the saved
values rather than reading the current clock or reconstructing an earlier
monotonic time from a later realtime correlation. Selecting an evdev file's
clock and converting to its userspace record remain devfs responsibilities.
The additional timestamp is bounded by the existing 256-event frame limit.

## Execution context and concurrency

`InputEventNotifier::notify_events` can run in hard IRQ context and only calls
the prepared handle's IRQ-safe `queue_default` method. Event reads, timestamps, listener
callbacks, registry changes, and teardown run in task context. The endpoint
releases its state mutex before calling any listener. The driver owns its
transport lock; no inputdev lock is held across a driver read.

The endpoint registry mutex protects identity and creation. The per-endpoint
mutex protects the partial frame, key and absolute-axis state, listener lists,
and detached state. Listeners are weak references, so open files do not form a cycle with
the endpoint. Stale weak references are pruned during frame delivery or
explicit unsubscription.

## Removal and failure handling

Removal marks the endpoint detached, clears its partial frame, removes the
driver notifier, shuts down the work handle after any running callback finishes, and
notifies all listeners. The device-node callback can initiate the same detach
before the inputdev removal callback; detach is idempotent. An endpoint is
published to the registry only after its work handle and driver notifier start
successfully. A failed registration leaves the class device untouched and is
reported to the caller.

The current managed removal path runs the driver and bus removal hooks and
`device.run_cleanups()` before emitting `DeviceEvent::Removed`. The inputdev
removal subscriber stops the worker only after that notification, so detach
does not establish a barrier before driver resources are released. Fixing that
ordering requires a separate driver-core lifecycle change.
