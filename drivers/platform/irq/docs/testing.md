# ARM Controller Preparation Validation

Use a disposable disk image and the repository build workflow. These checks
cover boot preparation, not CPU hotplug or arbitrary controller reinitialization.

## Normal Boot and Duplicate Rejection

Prepare the AArch64 QEMU configuration:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make clippy
make unittest UNITTEST_CRATE=irq_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock'
```

The irq-driver test calls local initialization after normal boot while retaining
CPU/IRQ exclusion. It expects an error and unchanged masking state. It must not
reset the live controller. This exercises the common rejection path on the CPU
running the test; it does not simulate a physical hardware initialization error.

For GICv2, disable `KFEAT_NMI`, its mode/source selections and
`KFEAT_WATCHDOG_HARDLOCKUP` in the defconfig-derived configuration, run
`make defconfig`, then:

```sh
make clippy
make unittest UNITTEST_CRATE=irq_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock' QEMU_ARGS='-machine virt,gic-version=2'
```

Restore the default configuration afterward. Check both the test results and
coverage-completion markers. Passing GICv2 without NMI does not validate hardware
NMI or physical platforms.

## Missing Supported Controller Description

With the default GICv3 unit-test image already built, use `fdtput` from
device-tree-compiler to remove the discoverable GIC compatible string without
changing production code:

```sh
qemu-system-aarch64 -machine virt,gic-version=3,dumpdtb=target/missing-gic.dtb -cpu cortex-a76 -smp 4 -m 1g -display none
fdtput -t s target/missing-gic.dtb /intc@8000000 compatible test,unsupported-gic
timeout 30 make justrun UNITTEST=y UNITTEST_CRATE=irq_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock' QEMU_ARGS='-dtb target/missing-gic.dtb'
```

This is an expected-failure boot. The required diagnostic is
`primary GIC initialization failed: "failed to parse GIC from device tree"`.
The platform's fatal error proves discovery returned an error to the boot hook;
an unrelated earlier panic or a timeout alone is not a pass. No UT success
marker is expected. The firmware fixture is deliberately incomplete for normal
device operation and must only be used for this early-failure check.

## Early-Transition Coverage Boundary

Normal kernel UT runs after IRQ activation. It must not assert that PMR still
holds the boot-time blocking value, nor rerun live initialization to manufacture
that state. Boot-time PMR ordering, backend-error propagation, and seeded
GICv2 private-state reset require early probes or hardware fault injection.
Report such runs separately from persistent UT coverage, including removal of
temporary probes. Static review must verify that local-attempt rejection
precedes destructive MMIO and that EOI/PMR publication follows backend success.

## x86 Fixed Vectors

Prepare `platforms/kplat-x86_64/qemu_defconfig`, then run:

```sh
make check_x2apic
make unittest ACCEL=n UNITTEST_CRATE=x86_apic,irq_driver,timer_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock'
make unittest ACCEL=y UNITTEST_CRATE=x86_apic,irq_driver,timer_driver,kruntime XKMAKE_ARGS='--no-net --no-vsock'
```

The second guest run requires KVM with x2APIC exposed; record the actual CPU
capabilities rather than treating acceleration alone as evidence of x2APIC.
The host suite checks masked preparation and ESR-write/read/EOI ordering,
including the legacy maximum-LVT exception. It uses ordinary memory as a
register fixture, not an emulation of hardware error latching.

The IRQ-driver tests verify that dedicated spurious/error vectors bypass
regular claims. Representative non-fixed vectors are forwarded unchanged to
an injected regular dispatcher; this does not test that backend's domain
classification or hardware EOI behavior. The spurious test invokes the common
IRQ entry and checks balanced context and IRQ masking on return. It does not
simulate physical delivery or use the entry's return value as classification
evidence; the separate dispatcher test checks that regular dispatch is bypassed.
Do not invoke the real error handler directly from a test: its EOI requires an
in-service error interrupt. Physical APIC error injection is not covered here.
