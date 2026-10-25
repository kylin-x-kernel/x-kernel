# Console Input Startup Validation

## Scope

Early stdout construction leaves UART RX notification masked and retains only
physical IRQ metadata. The runtime boot task prepares optional input after CPU,
GP and worker services, before device discovery or the first native TTY mode
selection. The console owner prepares storage, maps the IRQ, installs an action,
enables RX and finally publishes input readiness. Failures retain polled I/O.

This is a fixed boot-console transition, not dynamic UART open/close or hotplug.
Linux 7.2 `drivers/tty/serial/amba-pl011.c` separates polled early-console setup
from `pl011_startup`: hardware preparation, `pl011_allocate_irq`, then
`pl011_enable_interrupts`. This implementation follows that ownership/order,
not the complete Linux serial-core lifecycle.

## Reproduction

Start each architecture run from its own `qemu_defconfig`:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make UNITTEST=y clippy
make unittest UNITTEST_CRATE=console_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock'
```

Repeat with `kplat-riscv64` and `kplat-x86_64`; use `ACCEL=n` for x86 TCG.
Use a private writable disk image for each run. On an x86_64 Linux host, run
`make check_uart` for the vendored register-write fixtures. That check is also
wired into the native x86_64 CI Build Artifact stage.

## Recorded Results

Local validation on 2026-10-25 used release QEMU builds, four guest CPUs, and no
network/vsock. The tested tree was preserved byte-for-byte when rebasing onto
the Ticket05 squash merge (`7a26f7f0a`).

| Validation | Result |
| --- | --- |
| AArch64 console/runtime UT | 92 passed, 0 failed, 0 ignored |
| RISC-V console/runtime UT | 90 passed, 0 failed, 0 ignored |
| x86 TCG console/runtime UT | 86 passed, 0 failed, 0 ignored |
| UNITTEST Clippy | Passed for all three configurations |
| UART host register fixtures | 7 passed |
| Coverage export | Succeeded for all three kernel UT runs |

The UT totals include pre-existing console/runtime tests, not only new tests.
New setup tests cover absent metadata, preparation failure, mapping failure,
action rejection/error, continued polled reads/writes, activation only after
successful registration, and repeated calls after success/failure. They use
private ports and setup state, never the live IRQ registry. Preparation failure
is a deterministic capacity-overflow error, not actual allocator exhaustion.
Existing serial tests cover initial consumer notification and buffered input.
Startup observations reject input readiness during local IRQ-source activation.

## Interactive AArch64 Check

A normal, non-UT boot used the existing rootfs/init script without modification.
Input was sent through QEMU's serial terminal:

1. Execute `read -r reply`, send `late-input-ok`, and print the value; the value
   returned exactly as entered.
2. Run `(sleep 1; printf 'CONSOLE06_BACKGROUND\n') &`; output appeared while
   the shell waited for input, without requiring another keystroke.
3. Run `cat`, send a line and observe echo plus application output.
4. Send Ctrl-C, observe the shell prompt, and execute another command.
5. Run `sync` and `poweroff -f`; QEMU exited normally.

Early boot output and runtime shell input both worked. This validates the normal
UART-to-TTY-to-userspace path, not hardware failure recovery.

## Boundaries

- No physical-board, LoongArch, or pure-UP run is recorded here.
- Interactive x86/RISC-V and real OOM were not exercised; kernel UT results do
  not substitute for those tests.
- Register fixtures prove write ordering/values, not physical interrupt timing.
- Failed setup retains boot-lifetime storage and any published mapping, because
  the mapping can be shared. It does not disable a shared controller line.
- Polled fallback uses existing TTY manual processing and immediate poll recheck;
  no efficiency or throughput claim is made for this fallback.
- No lossless-input guarantee covers bytes received from firmware handoff through
  successful late-init RX activation. Without polling, retention before activation
  is limited to hardware FIFO capacity; software-drop counts exclude hardware
  overflow. Late activation does not flush the FIFO or recover lost bytes.
