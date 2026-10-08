# console-driver — Emergency Output Security and Reliability

## Scope

This analysis covers the entire crate: `src/lib.rs` (IRQ handler
registration and console state), `src/runtime.rs` (console adoption and
the adopted stdout runtime identity), and `src/serial.rs` (PL011/NS16550 UART
drivers). No modules are excluded; the emergency-output path is
reachable from the audited entry points.


## Trust Model and External Boundaries

Port construction trusts the platform/device layer to provide valid UART addresses,
mapping lifetimes, and device-tree layouts.
Emergency transmission reads hardware status and writes transmit registers using
byte slices supplied by the kernel.
It does not access user pointers, allocate DMA or ordinary memory, or parse external
configuration at call time.

## Unsafe Code and Memory Safety Invariants

- `SerialPort::new_mmio_pl011` requires a valid window aligned for 32-bit access,
  a mapping that outlives the port, and exclusive initialization.
  The stdout mapping comes from `iomap_device`; auxiliary mappings come from `devm_iomap`.
- `SerialPort::new_mmio_ns16550` and `Port::new` require the same mapping validity;
  register stride and width must match the device.
  Emergency handles reuse fixed values checked during normal construction.
- `EmergencyTx::try_send_raw` builds an independent local handle for each backend
  (`Pl011Uart::new` for PL011, `Port::new` or `uart_16550::SerialPort::new` for
  NS16550) over the same hardware window or I/O ports.
  The handles create no reference to the locked `Backend` and touch only status
  and data registers, without accessing mutable storage owned by Rust.
- NMI output does not read, dereference, or forcibly unlock `SpinNoIrq<SerialIo>`.
  It creates no `&mut Backend` overlapping the interrupted guard's borrow.

## Thread Safety

State outside the lock is immutable after initialization.
Concurrent emergency calls do not race to modify software state, but hardware
transmission has no mutual exclusion guarantee.
Volatile access is not synchronization: characters may interleave, writers may
race for FIFO capacity, and bytes may be lost.
This path provides no Linux nbcon-style ownership arbitration and does not support
concurrent changes to the baud rate or other configuration.

## Threats and Failure Handling

Prepared receive buffers hold untrusted UART bytes without interpreting them.
They have a fixed 4096-byte capacity and never overwrite unread data. Overflow
discards the new byte and increments a saturating software-loss count; hardware
FIFO loss is separate and cannot be inferred from this count. Each collection
attempt is bounded to 256 hardware samples, even when the buffer is full.
Bounded sampling cannot recover from a faulting or stalled individual MMIO access.
Preparation can fail allocation without installing partial state, and allocation
or disposal of an unused preparation occurs outside the port lock. Receive
methods never call wake callbacks or acquire task locks under that lock.
The port retains an Arc-wrapped Waker so IRQ snapshots only adjust the Arc
count under the lock; custom Waker clone/drop callbacks run outside it.
Software-drop accounting is consumed through `take_input_dropped_bytes()`;
native TTY reports nonzero counts outside the UART lock in task context, never
from collection in hardirq. Counts saturate between reports and do not include
unobservable hardware FIFO losses. No received contents are logged.
The stdout IRQ path enables storage before action registration. Allocation
failure prevents publishing an unprepared receiver. Early IRQs may collect bytes
before the TTY input notification is attached; attachment sends an initial wake.

Input notification uses a prepared Waker outside the port lock. The provider
must guarantee IRQ-safe, non-sleeping, allocation-free wake behavior and avoid
strong ownership cycles with the port. Waker's type alone does not enforce those
requirements. Owners must detach and drain their processing before releasing its
resources; already cloned notifications can still fire after detachment.
TTY handles those stale notifications through weak work ownership and shutdown.
Runtime stdout publication trusts the serial adapter to wrap the adopted early
port. The retained class handle does not by itself prevent removal; only the
kernel-lifetime boot port is used for native terminal fallback.

| Risk | Trigger | Mitigation and Residual Limitations |
|------|---------|------------------------------------|
| NMI self-deadlock | Interrupting a normal serial lock holder | Emergency transmission bypasses the port lock and the object it protects. |
| Indefinite transmit wait | FIFO remains full or transmitter remains unready | [`EmergencyTx::send_raw`](../src/serial/emergency.rs) limits polling for each transmitted byte to `TX_POLL_LIMIT = 100_000` attempts. On exhaustion, `EmergencyTx::write_data` returns and discards the remainder of the current byte slice. |
| Hardware access fault | Invalid mapping, powered-off device, or MMIO bus fault | Relies on the port lifetime contract. The polling budget cannot handle an individual bus access that never returns or faults. |
| Interleaved or lost logs | Writers on multiple CPUs, or normal and emergency writers, compete | Best-effort output is accepted; there is no buffering or replay guarantee. |
| Configuration reset | Emergency path reinitializes the UART | Emergency transmission only accesses status and data registers and performs no initialization. |
| Lock reentry during formatting | `Display`/`Debug` acquires a lock | `kprint_atomic!` callers must audit formatting; bypassing the serial lock does not remove this obligation. |

## Privacy and Audit Checklist

Output may contain task names, registers, and kernel addresses.
Visibility depends on access to the physical or virtual console.

- Does each new backend provide emergency transmission without accessing the locked object?
- Do the address, stride, and access width come from the same initialized device?
- Does the timeout path return directly without recursively printing an error?
- Are MMIO/PIO window lifetimes and natural alignment preserved?
- Does the documentation distinguish bypassing ordinary locks from complete, serialized, reliable output?
