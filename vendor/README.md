# Vendored Dependencies

Vendored packages retain their upstream licenses, manifests and formatting.
They are excluded from the kernel workspace, not treated as first-party
workspace members. Kernel consumers still pass the normal build and lint gates.
Do not apply kernel copyright headers or repository-wide refactors to them.

## x2apic

- Published version: 0.5.1
- Upstream: https://github.com/kzhao0986/x2apic-rs
- Source revision: b8f989f781f44ba9a2e504c55dba2f2d526590f5
- Archive: x2apic-0.5.1.crate
- Archive SHA-256: e64dd21b207e918377ce79266b54517c75262031948f06324b6ef8473d3294cc
- License: MIT OR Apache-2.0; original license files are retained.

The import commit preserves the published archive byte-for-byte. Local changes
belong in separate commits. When updating, compare against that baseline,
reapply only required patches, and rerun dependency tests plus x86 kernel
build, Clippy and xAPIC/x2APIC boot tests. Remove the patch override once an
upstream release supplies the required behavior.

### Local Patch

`LocalApic::enable_masked()` first masks LVTERR with a valid error vector,
then prepares a stopped one-shot timer and masks both LINT pins without
changing their routing. `enable_error()` activates error reporting separately
after the host publishes its APIC handle and has a ready fixed-vector handler.
Neither operation alters `enable()` or
introduce any kernel dependency. The register-write observer exists only in
host tests and checks the entire write sequence, not just its final state.
This is not a drain of firmware-pending interrupts or a reset of other LVTs.

Run the dependency regression on an x86_64 Linux host:

```sh
make check_x2apic
```

Do not replace this with only a successful kernel boot: boot alone cannot
detect a temporary unmask followed by a later mask.

CI runs this target in the normal x86_64 Build Artifact stage on a native
x86_64 agent. Other architectures do not execute this architecture-specific
host binary. The standalone lockfile pins x86_64 to 0.15.4, matching the kernel;
this is a local change separate from the pristine archive import.
Kernel unittest boot checks additionally verify the caller's prepared hardware
state before activation; host tests alone cannot detect the host choosing the
wrong initialization API.

`LocalApic::acknowledge_error()` supplies the separate APIC error protocol:
refresh ESR (subject to the legacy maximum-LVT guard), read status, and EOI
once. The host register-access tests verify this order and the legacy branch;
they do not emulate the hardware mechanism that latches physical error bits.
