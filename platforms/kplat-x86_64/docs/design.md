# x86 guest S3 execution context

`suspend.rs` provides the reset-aware HAL operations used by the system PM
coordinator. It accepts the QEMU/SeaBIOS FADT family and a boot-evaluated `_S3`
package; the encoded sleep type is not guessed. The implementation currently
uses one system-I/O PM1a register, a real-mode FACS vector, retained guest RAM,
and stable TSC frequency. It is not a general physical-PC ACPI implementation.

The reserved cold-boot AP page is exclusively borrowed for each transaction.
WakePage saves its complete original contents and the FACS vector/OS flags,
installs a small real/protected/long-mode trampoline, and restores the original
bytes only after every AP callback has joined. Fixed per-CPU contexts preserve
callee registers, original stack, control registers, syscall/segment/APIC MSRs,
GDT/IDT/TR and FXSAVE state. Assembly returns once on the saved stack after
abort or reset, avoiding a Rust returns-twice boundary.

On wake, the boot CPU reconstructs its local APIC and masks legacy PICs. INIT
puts all remote CPUs into wait-for-SIPI before the shared low page is retargeted
sequentially. Each AP uses its actual raw APIC ID, restores its own context and
controller, and publishes acknowledgment before the next target starts. The
software CPU barrier remains held until device/time recovery completes.
Raw TSC delays implement the INIT/SIPI budget while MONOTONIC is frozen.
IOAPIC routes are reconstructed masked and original masks are restored only by
finish, after device recovery and remote join.

The abort-path register fixture uses a test-only assembly wrapper to seed
all six SysV callee-saved registers, clobber them in a nonreturning assembly
callback, jump to the real s3_restore_context and inspect the restored values.
The wrapper restores the Rust caller ABI before returning a mismatch bitmask.
