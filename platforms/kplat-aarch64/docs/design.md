# kplat-aarch64 Design

## Scope

This crate implements the generic AArch64 device-tree platform and common
peripherals. `src/lib.rs` gates the crate on `target_arch = "aarch64"` and
`k_plat_name = "kplat-aarch64"`; peripheral feature gates remain in
`src/peripherals/mod.rs`.

## PMU Interrupt Ownership

The platform prepares the PMU's GIC domain mapping with `kirq::try_map` before
installing either its ordinary IRQ action or NMI action. Mapping preparation
does not configure or enable the source. The mapping belongs to platform
lifetime, independently of the handler; failed installation leaves it reserved
for retry rather than attempting early-boot RCU reclamation. Mapping errors
are logged with the descriptor and returned as registration failure. Existing
normal-IRQ activation and later NMI promotion remain separate operations.
This startup owner exposes no mapping-disposal operation; explicit preparation
does not change KIRQ's generic action-teardown behavior.

## Guest Test Layout

Guest tests live in crate-root `ut-tests/`, mounted by
`unittest::path_mod_test!` at their original parent modules:

| Parent module | Test file | Cases |
| --- | --- | --- |
| `peripherals::smccc::tests` | `ut-tests/peripherals_smccc.rs` | 4 |
| `peripherals::trng::tests` | `ut-tests/peripherals_trng.rs` | 6 |

The private `tests` modules retain access to parent helpers through `super`.
They are compiled only with `cfg(unittest)`, within the crate's architecture
and platform gates. The SMCCC conduit expectation uses `cfg!(feature = "vmm")`:
both feature settings register all four cases, with different expected HVC
handling. No case is serial or user-mode.

SMCCC cases exercise conduit decoding and selection without issuing firmware
calls. TRNG cases inject SMCCC results into `read_trng_random_with` to check
errors before data and partial progress after data. Moving these tests does
not change production code or firmware initialization.
