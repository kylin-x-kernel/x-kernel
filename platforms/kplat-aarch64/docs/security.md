# kplat-aarch64 Security Scope

## Scope

This note covers the guest tests in `ut-tests/peripherals_smccc.rs` and
`ut-tests/peripherals_trng.rs` and their mounts in `src/peripherals/smccc.rs`
and `src/peripherals/trng.rs`, plus PMU mapping preparation in
`src/peripherals/pmu.rs`. It is not a whole-platform unsafe-code audit.

## PMU Mapping Preparation

Startup owns PMU mapping preparation and keeps the source quiescent until
handler installation and the existing activation/promotion steps permit
delivery. A mapping error is logged and rejects registration before handler
allocation. Successful mapping alone neither enables hardware nor establishes
NMI quiescence. An installation failure retains the platform-owned mapping;
no early-boot RCU disposal or rollback of controller writes is promised.

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
