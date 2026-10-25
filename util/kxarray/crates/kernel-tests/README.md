# XArray Kernel Integration Tests

This test-only crate owns fixtures requiring tasks, timer IRQs, softirq, controlled
NMI contexts and SMP migration. Basic container UT stays in `kxarray/ut-tests/`.
It adds no startup hook, service or benchmark runner.

The reason for this separate crate is the planned `kirq -> kxarray` consumer
dependency: placing these fixtures' `kirq`/`ktask` dependencies in `kxarray`
would create `kirq -> kxarray -> kirq`, also through `ktask -> kirq`.
`cfg(unittest)` does not remove Cargo dependency edges. This is an exception
for dependency cycles, not the default test layout. Ordinary UT, host tests and
doctests stay in their owning crate; requiring a running kernel is not by itself
a reason to create a test crate.

From the workspace root, after preparing a platform defconfig:

```sh
timeout --kill-after=10 600 make unittest UNITTEST_CRATE=kxarray,kxarray_kernel_tests
```

Use the underscore module name in filters, not the hyphenated Cargo package name.
The normal boot sequence must initialize RCU before the unit-test runner starts.
SMP migration requires at least three CPUs; unsupported CPU counts are ignored.
Handshake stalls are bounded by the external QEMU timeout, not a guest-latency
pass criterion. Controlled NMI fixtures do not prove hardware NMI delivery.

Only `cfg(unittest)` builds register fixtures. The unittest entry enables
`kfeat/kernel-tests`, which selects the optional container and integration crates.
Weak SMP forwarding does not activate this crate in ordinary system configurations.
Cargo's workspace lockfile still resolves optional dependencies; their kernel
edges belong to this test crate, never to the container. See the
[coverage notes](../../tests/README.md) for coverage boundaries.
