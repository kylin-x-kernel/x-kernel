# xconfig Linux-Kconfig Audit Notes

## Scope

This note tracks behavior that was explicitly compared against Linux
`scripts/kconfig/conf.c`, `confdata.c`, and `symbol.c`.

The focus of this audit pass was the non-interactive configuration flows:

- `defconfig`
- `savedefconfig`
- `olddefconfig`
- `oldconfig --auto-defaults`
- `saveconfig`
- `gen_cargo`

A follow-up pass also covered key interactive/menuconfig behaviors:

- choice selection semantics
- save-time state recomputation

## Linux Reference Model

For these flows, Linux effectively does three things before writing output:

1. Load user-provided configuration values.
2. Recompute the effective symbol state from Kconfig semantics:
   - `choice` selection
   - `if` / `depends on` visibility
   - defaults for derived and visible symbols
   - reverse dependencies from `select`
   - lower-bound defaults from `imply`
   - scalar `range` clamping
3. Serialize either the full `.config` or a minimal `defconfig`.

The important property is that writing happens from the recomputed effective
configuration, not from the raw parsed input.

## Findings Fixed In This Pass

### 1. Cross-architecture `defconfig` expansion was not recomputing derived symbols

Symptoms:

- minimal cross-arch `defconfig` inputs such as
  `ARCH_X86_64=y` + `PLATFORM_KPLAT_X86_64=y`
  could still expand with `ARCH="aarch64"` and
  `PLATFORM="kplat-aarch64"`.

Cause:

- `defconfig` loaded raw values but did not re-run the same post-load
  state refresh used by menuconfig-style flows.
- values loaded from `defconfig` were not marked as user-provided, so
  choice reconciliation did not treat them as the chosen branch.

Fix:

- `ConfigEngine::load_config()` now loads through the tracked/user-aware path.
- `defconfig_to_output()` now refreshes effective prompt state before writing.

Validation:

- `arch_config_bugs_test::test_defconfig_to_output_recomputes_cross_arch_derived_values`
- repository-wide `defconfig -> savedefconfig -> defconfig` round-trip

### 2. `select` / `imply` were not applied in non-interactive flows

Symptoms:

- `defconfig` and `olddefconfig` could write `# HELPER is not set`
  even when `SELECTOR=y` and `SELECTOR select HELPER`.

Cause:

- reverse-dependency propagation only happened in interactive toggle paths.

Fix:

- added non-interactive reverse-dependency propagation during
  `refresh_prompt_state()`
- `oldconfig --auto-defaults` / `olddefconfig` now refresh effective state
  before saving

Validation:

- `linux_golden_flow_tests::test_defconfig_golden_output_applies_selects_and_implies`
- `linux_golden_flow_tests::test_olddefconfig_golden_output_applies_selects`

### 3. Scalar `range` constraints were parsed but not enforced

Symptoms:

- out-of-range `u32` / `hex` values from `defconfig` could survive unchanged.

Cause:

- parser recorded `properties.range`, but the configuration engine never
  clamped effective values before write-out.

Fix:

- added range validation/clamping during `refresh_prompt_state()`
  for scalar integer and hex symbols

Validation:

- `linux_golden_flow_tests::test_defconfig_golden_output_clamps_range_violations`

### 4. `saveconfig` rewrote defaults instead of normalizing the current config

Symptoms:

- `saveconfig` could discard the caller's current `.config` values and emit
  a default-only configuration.

Cause:

- the command instantiated a fresh `ConfigEngine` and wrote it out without
  loading the existing config file first.

Fix:

- `saveconfig` now loads the target config file, refreshes effective state,
  and writes the normalized result back.

Validation:

- `linux_golden_flow_tests::test_saveconfig_golden_output_preserves_effective_config`

### 5. `gen_cargo` consumed raw `.config` text instead of effective Kconfig state

Symptoms:

- minimal configs that relied on Kconfig defaults could miss derived
  `KFEAT_*` features in `.cargo/.xconfig.toml`.

Cause:

- `gen_cargo` extracted build features directly from `ConfigReader::read()`
  output without first replaying Kconfig defaults and dependencies.

Fix:

- when a local `Kconfig` is present, `gen_cargo` now loads the config through
  `ConfigEngine`, refreshes effective state, and extracts features from that
  recomputed symbol set.

Validation:

- `gen_cargo_flow_tests::test_gen_cargo_uses_effective_kconfig_defaults_from_minimal_config`

### 6. Menuconfig choice/save paths skipped Linux-style state reconciliation

Symptoms:

- selecting a choice option in the TUI did not propagate that option's
  `select` side effects.
- saving from the TUI could write intermediate values without a final
  recomputation pass.

Cause:

- choice interaction used a separate simplified path from ordinary enable
  toggles.
- save used `write_config()` directly after dependency audit.

Fix:

- TUI choice selection now checks enableability and applies `select` / `imply`
  propagation like ordinary config toggles.
- TUI save now runs `refresh_prompt_state()` before auditing and writing.

Validation:

- `ui::app::tests::test_choice_selection_applies_selects`
- `ui::app::tests::test_save_config_refreshes_effective_state_before_write`

### 7. `select`-enabled symbols were pruned or rejected during later validation

Symptoms:

- a symbol enabled through `select` could be set to `y`, then later cleared by
  inactive-symbol pruning.
- TUI save-time dependency audit could reject Linux-valid states where a symbol
  is enabled only through reverse dependency propagation.

Cause:

- post-processing still treated `can_enable()` failure as a hard invalid-state
  signal even for symbols that were currently selected by an enabled symbol.

Fix:

- inactive-symbol pruning now preserves symbols that are selected by an enabled
  selector.
- save-time dependency auditing no longer treats those Linux-style reverse
  dependency states as fatal errors.

Validation:

- `ui::app::tests::test_save_config_allows_linux_style_selected_symbol_with_unmet_direct_deps`

## Current Status After Fixes

Validated green:

- full `xconfig` test suite
- repository-wide `defconfig -> savedefconfig -> defconfig` round-trip

The checked-in platform defconfigs still differ textually from freshly
generated `savedefconfig` output, but they now expand back to the same
effective `.config` on every audited platform.

## Remaining Audit Areas

These areas still deserve explicit Linux-side comparison in later passes:

1. `saveconfig` exact write semantics versus Linux `conf_write`
2. prompt visibility and write suppression for unchangeable symbols
3. warning behavior for `select` on symbols with unmet direct dependencies
4. range validation during interactive prompt entry, not just final refresh
5. broader menuconfig parity: search, help panes, and prompt ordering details
