# Legacy block backend verification and re-enablement

AHCI, BCM SDHCI and SD/MMC production features, SDK dependencies and probe
registration remain disabled. Source changes and kernel unit tests do not
enable any of these controllers or establish hardware correctness.

## BCM compile and execution path available in kernel UT

`cfg(unittest)` compiles the actual `src/bcm2835sdhci.rs` adapter. Its controller
import is replaced by `ut-tests/bcm_controller_fixture.rs`, matching the subset
of the pinned SDK API that the adapter uses. Its real geometry guards,
`align_to[_mut]::<u32>` views, CPU-provider injection and per-sector loops run
against that CPU controller through `BlockDeviceOperations`. No SDK feature or
dependency is restored. Existing legacy interface tests are retained.

The fixture models sector contents and injected errors. It cannot verify MMIO,
interrupt timing, SDK internals, hardware DMA, cache maintenance or probe wiring.
SDK-only compilation, where recorded separately, is also not hardware testing.

## AHCI admission and kernel UT

The AHCI SDK places physical data-buffer addresses directly in PRDT entries.
The current host `flush_dcache` callback supplies only an ordering barrier;
it does not clean/invalidate cached data or command memory. `AhciHal` therefore
requires an explicit `DMA_COHERENT` claim; false rejects initialization before
any SDK MMIO/DMA. The host only opts in on x86_64. Non-coherent AHCI is not
implemented. Changing only unaligned CPU scratch to uncached memory would not
fix direct aligned callers or SDK command allocations.

Kernel UT compiles the same AHCI adapter with a CPU controller import. Tests
verify rejection before SDK entry, aligned forwarding without CPU staging,
and unaligned success/error/allocation/release behavior. Those tests do not
establish hardware coherency, physical/bus address equivalence or device DMA
completion. BCM's pinned DATA FIFO path is PIO and retains ordinary CPU scratch.

## Required before production re-enablement

- [ ] Review the pinned SDK and target-platform versions and dependencies.
- [ ] Compile the real SDK import and host probe/constructor wiring with the
  intended platform configuration; the test controller is not that SDK.
- [ ] Extend each existing single adapter module predicate when enabling its
  production feature; do not append a second declaration beside `cfg(unittest)`.
- [ ] For AHCI, establish cached data **and command** memory coherency. The
  admission flag is not a workaround for missing clean/invalidate operations.
  A non-coherent port needs a complete SDK/HAL mapping/cache protocol before
  it can opt in; `dma_read_barrier` alone does not meet that requirement.
- [ ] Verify reported block geometry and sector-number limits against the SDK;
  BCM requires its reported block size to equal the SDK `BLOCK_SIZE`.
- [ ] Check CPU-buffer borrowing and any device DMA mapping, contiguity and
  cache-maintenance requirements end to end, including teardown and errors.
- [ ] Run aligned and each unaligned address residue for reads and writes on
  real hardware; cover multi-sector requests and allocation/I/O failures.
- [ ] Establish that no device retains a buffer after a synchronous call
  returns and that cleanup returns all mappings and owned storage once.
- [ ] For SD/MMC, add automated adapter checks. For every legacy backend,
  validate the actual controller/HAL; CPU fixtures do not cover hardware paths.
- [ ] Only then consider enabling production features, dependencies and probes
  in a separate change with platform evidence.

Until these checks are completed, hardware results for all three legacy
backends remain `NOT_RUN`. Unit-test coverage of the BCM adapter is a narrower
claim than production integration or hardware validation.
