# console-driver — Console and Emergency Output Design

## Purpose and Scope

This crate connects `khal::console::ConsoleIf` to the platform UART and shares
the early stdout instance with the runtime serial driver.
It separates normal and NMI emergency output and owns optional bounded input storage.

- `src/lib.rs`: Resolves stdout, maps MMIO, implements `ConsoleIf`, and registers input IRQs.
- `src/serial.rs`: Defines one `SerialPort` per UART, publishes stdout, and shares instances.
- `src/serial/emergency.rs`: Transmits emergency output without taking ordinary locks.
- `src/serial/input.rs`: Stores bounded input and counts software overflow under the port lock.
- `src/ns16550_mmio.rs`: Accesses NS16550 MMIO registers using the stride and width specified by the device tree.
- `src/runtime.rs`: Tracks runtime console devices and the adopted stdout identity.

## Architecture and Algorithms

`runtime::stdout_console` snapshots the adopted stdout handle
(`ClassDevice<CharDeviceImpl>`) under the subsystem lock. Native TTY retains
that handle and performs I/O without holding the registry lock. The owning
serial driver's character-device implementation forwards to the same early
`SerialPort`. The established TTY remains connected to that port.
Kernel normal/emergency output uses the early port through `ConsoleIf`, not
the runtime registry.

```text
TTY connection -> CharDevice::write -> SerialPort::write_data -> io: SpinNoIrq<SerialIo> -> UART
Emergency output -> SerialPort::write_data_atomic -> EmergencyTx              -> UART
TTY connection -> CharDevice::read -> SerialPort::read_data -> UART
```

Port construction initializes the device and saves its virtual address or I/O-port
base in `EmergencyTx`, outside the lock.
For NS16550 MMIO, it also saves the register stride and access width decoded during
the same construction path.
The stdout instance is published through `LazyInit` after construction;
the early console and runtime driver share one `Arc<SerialPort>`.

Emergency transmission does not access `io`.
Each backend builds a local register handle containing only fixed address information
and calls its nonblocking transmit primitive without initializing the device:
PL011 uses `arm_pl011`'s local `Pl011Uart` handle and `try_putchar`;
the NS16550 backends use `try_send_raw`.
Each transmitted byte is polled at most 100,000 times.
A failure immediately ends output of the current byte slice.
This is an iteration budget, not a wall-clock time guarantee.
Newline and backspace conversions follow the corresponding backend's transmit semantics.

## Receive Storage

`SerialIo` owns the backend and optional `SerialInput` under the same port
lock. `SerialInput` uses a `ringbuf::LocalRb` with a fixed 4096-byte heap allocation;
the exclusive port lock makes separate producer/consumer synchronization unnecessary.
Storage preparation allocates outside the lock, rechecks before installation,
and preserves an existing buffer on repeated preparation. No normal receive
operation allocates or grows the buffer.

Collection reads at most 256 hardware bytes, retaining older buffered data and
counting rejected new bytes with a saturating `u64`. Reading consumes buffered
bytes first and, when necessary, samples at most one further hardware batch.
An empty output slice neither consumes input nor reads hardware. Boot and runtime
handles share this state; `getchar` cannot bypass older buffered input.

Software overflow accounting saturates between reports. The native TTY input
consumer calls `take_input_dropped_bytes()` and logs nonzero losses in task
context, outside the UART lock. Reading the report clears only the count, not
buffered data. Before attachment the count accumulates for the initial pass.

The stdout IRQ registration path prepares storage before publishing its action.
The action acknowledges and collects a receive batch under the port lock, then
wakes the bound input `Waker` after unlocking. An unrelated IRQ reports unhandled.
Ports without prepared IRQ input retain direct hardware reads.

The port owns only an optional input `Waker`, not a work handle or CPU routing.
The consumer must supply a non-sleeping, allocation-free IRQ-safe implementation.
TTY supplies a weak notification adapter to its prepared work, so character
processing never executes inline in IRQ. Attachment immediately notifies outside
the port lock to cover pre-attachment bytes and sub-threshold FIFO tails.

Attachment rejects duplicates. Owners serialize attach/detach, detach the
producer, then stop and drain their processing without port or TTY locks. Retained
IRQ clones can still notify after detach; detach alone is not a completion barrier.
Callback resources remain live through this barrier. Last-handle Drop is not a
synchronous barrier. Callbacks must not strongly own their port. The native
console is permanent; dynamic switching and UART hot-unplug are not implemented.

## Stdout Runtime Identity

`register_stdout_console` records the adopted early stdout device ID in the
existing console subsystem. The character-availability bridge recognizes this
ID even before activation. Auxiliary serial devices remain ordinary character
devices. Removal clears the stdout association as well as the console view.
`stdout_console` snapshots this specific available device. Native TTY retains
that snapshot for access to its original port. Snapshot ownership is not a
hot-unplug barrier.
Devfs uses `stdout_console_id` to exclude this port from auxiliary serial
enumeration. The subsystem exposes no active/default console selection API.

## Calling Constraints and Concurrency

- Normal reads, writes, and input IRQ handling use the port lock and must not be reentered from NMI context.
- Emergency transmission may run in NMI context or while the normal port lock is held.
  It does not allocate or sleep and does not depend on the scheduler, clocks, or IRQ completion.
- MMIO mappings, device power, and transmit configuration must remain valid.
  Emergency transmission does not restore device configuration.
- The emergency path shares only hardware addresses and creates no reference to the locked `Backend`.
- Emergency transmission is not serialized with normal transmission or other CPUs.
  Bytes may interleave or be lost; there is no log buffer, replay, or console ownership takeover protocol.
  This is a best-effort path for diagnostics during shutdown.
- Argument evaluation and `Display`/`Debug` implementations used by `kprint_atomic!`
  must also satisfy the calling context's constraints.

## Lifetime and Validation

`EmergencyTx` does not own a second mapping and does not unmap the device.
A static reference retains the stdout port.
Auxiliary ports follow the device mapping's lifetime; the mapping may be removed
only after all handles have stopped using it.

Regression tests cover routing empty output while both the normal print lock and
serial lock are held, transmission through simulated registers while the port lock
is held, return when hardware remains unready, preservation of configuration
registers, and all three NS16550 access widths.
