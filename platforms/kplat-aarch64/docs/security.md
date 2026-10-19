# kplat-aarch64 Test Security Scope

## Scope

This note covers the guest tests in `ut-tests/peripherals_smccc.rs` and
`ut-tests/peripherals_trng.rs` and their mounts in `src/peripherals/smccc.rs`
and `src/peripherals/trng.rs`. It is not a whole-platform unsafe-code audit.

## Boundaries

The SMCCC tests call pure conduit helpers, not HVC/SMC assembly, and do not
change the global conduit. The TRNG tests supply local closures and buffers;
they do not call firmware or change the global availability flag. Neither
test file contains unsafe code or accesses live peripheral registers.

The original architecture/platform gates and `cfg(unittest)` keep these
modules out of non-test builds. Private module visibility is retained;
extraction does not widen the production API or change the existing unsafe
firmware-call contracts. These tests check result handling, not firmware
quality or entropy quality.
