# Interrupt Controller Preparation Validation

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

## Domain Instance Isolation

The GIC test suite registers a second firmware GIC controller with
`IrqDomainTarget::New`, forcing a dynamically allocated domain, and verifies
that `select_gic_flow` rejects every source in that domain with
`UnsupportedFlow`. This proves family metadata is not treated as instance
ownership; it does not add a second hardware backend. PLIC has the symmetric
test: it registers a second PLIC instance with `IrqDomainTarget::New` and
verifies that `select_plic_flow` rejects descriptors carrying that domain even
though their controller is `Plic`, while the initialized instance's own source
is still accepted.

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

The same guest run exercises `x86::discovery`: synthetic MADT byte streams
cover entry ordering, duplicate addresses/GSIs, reserved flag encodings,
unknown GSIs, fixed-capacity limits, and framing errors; a live test reads the
QEMU MADT through `from_acpi` and checks ACPI provenance plus a non-zero
controller address.

The x86-apic register-model tests also cover the deferred retarget protocol:
edge mask/destination/readback/restore, `KeepMasked`, physical-Fixed and logical
destination rejection, destination-write failure, and both level outcomes. The
private `Registers::remote_irr` observation lets the model keep the read-only
bit out of the vendored RTE layout: IRR set leaves destination unchanged and
keeps the request pending; clearing it permits the verified write. This is a
production-transaction branch, not a real MMIO timing proof.

KIRQ tests cover accepted-but-pending snapshots, pre-EOI edge completion,
post-EOI level completion, persistent pending, masked activation/recovery,
pending cancellation and immediate non-pending GIC/PLIC behavior. Replay has a
separate software-only regression that would panic if it entered prepare/finish.
Regular and shared registration are exercised through the public paths from a
failed Line that satisfies member-install eligibility: logical suppression,
pending recovery, and no blocked teardown. Successful masked recovery publishes
the new action; a failed regular recovery publishes no action, and a failed
shared recovery returns no token and leaves an existing shared action count
unchanged. The shared case can retain an existing action while the recovery
admission runs. These are not hardware-drained or physical Line lifecycle
proofs, and they do not replace Ticket07 delivery/EOI validation.

The IRQ-driver tests verify that dedicated spurious/error vectors bypass
regular claims. Representative non-fixed vectors are forwarded unchanged to
an injected regular dispatcher; this does not test that backend's domain
classification or hardware EOI behavior. The spurious test invokes the common
IRQ entry and checks balanced context and IRQ masking on return. It does not
simulate physical delivery or use the entry's return value as classification
evidence; the separate dispatcher test checks that regular dispatch is bypassed.
Do not invoke the real error handler directly from a test: its EOI requires an
in-service error interrupt. Physical APIC error injection is not covered here.

## RISC-V Source Activation

```sh
cp platforms/kplat-riscv64/qemu_defconfig .config
make defconfig
make UNITTEST=y clippy
make unittest UNITTEST_CRATE=irq_driver XKMAKE_ARGS='--no-net --no-vsock'
```

The local CSR test masks global IRQ delivery and excludes migration, clears
SSIE/STIE/SEIE, then invokes source activation. It checks the resulting source
bits and that global delivery remains masked. The original SIE value is restored
before assertions. This validates activation on the hart executing the test;
it does not inject physical PLIC interrupts or prove boot-handler ordering.
