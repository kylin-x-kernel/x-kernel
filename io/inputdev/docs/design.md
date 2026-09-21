# inputdev — Design

## Purpose

`inputdev` maintains a live snapshot of input class handles for kernel
consumers. It seeds from the `kclass` registry, subscribes to later arrivals,
deduplicates by driver-core id, and removes handles on device removal.

## Responsibilities

- Seed a process-wide snapshot from the input class registry.
- Track later registration and removal callbacks by driver-core id.
- Return cloned class handles so multiple kernel consumers can rescan without
  consuming shared discovery state.

## Non-responsibilities

`inputdev` does not detect hardware, bind drivers, or define input-device
identity; `kdevice`, `kclass`, and input drivers own those tasks. It does not
read input events, implement evdev operations, or provide blocking waits.
Userspace device-node creation, numbering, file operations, and hot-remove
behavior belong to devfs and its `EventDev` adapter.

## Architecture and lifecycle

```text
kclass input snapshot + availability callback
                    │
                    ▼
INPUT_DEVICES: Mutex<Vec<ClassDevice<InputDeviceImpl>>>
                    │ clone
                    ▼
             input_devices()

kdevice removal callback ──> remove matching id
```

`init_input` initializes the registry, takes the existing class snapshot,
installs the availability callback, and installs removal handling. Registration
is idempotent. `input_devices` clones the current vector and does not consume
it, so multiple peers such as devfs can obtain consistent handles without
stealing devices from one another.

## Execution context

Initialization and callbacks run in device-probe/removal task context. They
acquire a sleepable mutex and registration may grow the vector. The API is not
valid in interrupt context and requires the registry to have been initialized.

## Concurrency model

One mutex protects the vector and deduplication. Device callbacks hold it only
for lookup and push/remove. Consumers receive cloned class handles and operate
outside the registry lock. Removal uses `swap_remove`; ordering is deliberately
unspecified.

## Design decisions

- Snapshot semantics support multiple consumers and repeated rescans.
- Driver-core ids are the identity key across probe and removal.
- Device behavior remains in class handles; this crate only owns discovery
  state.
