# inputdev — Design

## Purpose

`inputdev` owns event progress and distribution after an input class device
becomes active. It drains completed driver events even when no evdev file is
open, tracks device-wide key state, and delivers complete frames to independent
listeners. Hardware probing and user-facing file semantics stay in the driver
framework and devfs respectively.

## Architecture

```text
VirtIO input IRQ -> InputEventNotifier -> InputEndpoint::WorkHandle
                                            |
                                            v
                                InputDevice::read_event()
                                            |
                               frame and key-state updates
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
still tracks valid key transitions while events are drained.

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
mutex protects the partial frame, key state, listener lists, and detached
state. Listeners are weak references, so open files do not form a cycle with
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
