# Build And Config

Use this reference for:

- `make defconfig` or Kconfig refresh failures;
- compile errors;
- link errors;
- feature or configuration mismatches across crates.

## Primary Goal

Narrow the failure to one of these:

- configuration preparation;
- one crate boundary;
- one `cfg` or feature edge;
- one link-time ownership problem.

## Execution Path

1. Confirm `.config` was prepared from the intended platform defconfig.
2. Re-run the narrowest build command that reproduces the issue.
3. Identify the first real compiler or linker error.
4. Determine whether the failure is:
   - missing config symbol;
   - API mismatch;
   - type or trait error;
   - missing dependency or feature edge;
   - arch-specific compile break;
   - link-time symbol or section failure.
5. Map the first failing file to the owning crate or subsystem.

Ignore later cascaded errors until the first one is understood.

## Typical Signals

- `cannot find` often indicates missing imports, cfg edges, or moved APIs;
- trait bound errors often point to an API contract mismatch;
- duplicate or undefined symbols often indicate link or feature wiring issues;
- errors appearing only on one platform often indicate arch or config coupling.

## Action Rules

- if the error appears before any Rust compilation,
  inspect config preparation and generated files first;
- if only one platform fails,
  treat `cfg` and Kconfig wiring as primary suspects;
- if many crates fail after one moved API,
  fix the first owner boundary first;
- if the failure is a link error,
  search for ownership duplication, missing object inclusion,
  or feature-gated symbol absence.

## Follow-Up Actions

After initial localization, check:

- whether the crate boundary changed recently;
- whether a `cfg` path no longer matches Kconfig features;
- whether the failure is introduced only under a specific platform defconfig;
- whether the first error is upstream of many secondary errors.

## Stop Condition

Stop this first pass once you can say:

- which file or crate first fails;
- whether the failure is config, API, type, or link related;
- which owner is most likely responsible.
