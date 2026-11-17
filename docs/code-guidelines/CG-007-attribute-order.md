---
id: CG-007-attribute-order
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [derive_not_last, unsorted_attributes]
---

# CG-007-attribute-order: Keep attributes and derive lists predictable

- **Rule**: "place `#[derive(...)]` after other outer attributes;" "attributes or derives ordered inconsistently with the project convention;" "keep attributes ordered predictably;" "keep derived traits ordered consistently;" Order attributes on all items alphabetically by their final path segment wherever their semantics permit. Put `derive` after other built-in attributes and before helper attributes introduced by that derive. Sort traits within `derive` alphabetically by their final path segment, using the full path as a tie-break. For equal `cfg_attr` keys, compare their complete source text lexicographically. Keep doc comments and `#[doc]` in source order. Preserve the relative order of attribute macros and any other attributes whose expansion or repeated application depends on order; record the specific dependency when order cannot follow the convention.
- **Scope**: The convention applies to outer and inner attributes, including attributes on fields, variants, impl items, and trait items. The automated pre-expansion lints check only explicit outer attributes on source items. They check crates in the scanned Cargo application dependency graph, skip vendored crates under `vendor/` (see [README.md](README.md#vendored-code)), see out-of-line modules, and exclude macro-generated items, inner attributes, fields, variants, impl and trait members, and items carrying custom attributes or derive helpers. They do not expand `cfg_attr`; they exclude order-sensitive conditional payloads. Code compiled by neither aarch64 nor x86_64 `qemu_defconfig` is not covered by those scans and requires review.
- **Why**: A stable convention makes declarations easier to scan and reduces unrelated diff churn.
- **How to check**:
  1. Review every attribute list against the rule above. The lints recognize only these built-in outer attributes: `allow`, `cfg`, `cold`, `deny`, `deprecated`, `derive`, `expect`, `forbid`, `inline`, `link_section`, `must_use`, `non_exhaustive`, `repr`, `target_feature`, `used`, and `warn`. Lint-level attributes are barriers for automated sorting, since their order can matter. An unknown or custom attribute macro makes its entire item's list ineligible: attribute macros expand in source order and may consume nearby attributes.
  2. Keep doc comments and `#[doc]` in source order; they are barriers between sortable groups. Keep derive helper attributes after their introducing `derive` and never move a derive through a helper. If a helper or an unrecognized attribute is present, leave that item to review.
  3. The sorting lint considers `cfg`, `cfg_attr`, `cold`, `deprecated`, `inline`, `link_section`, `must_use`, `non_exhaustive`, `repr`, `target_feature`, and `used`. It accepts `cfg_attr` only when every payload is one of `cold`, `deprecated`, `inline`, `must_use`, or `non_exhaustive`; any other payload, especially `derive`, makes the item ineligible. It refuses a run when direct or conditional attributes repeat a name or combine order-sensitive codegen kinds (`inline`, `cold`, `target_feature`). Conditions and payload text stay intact.
  4. Sort a derive list automatically only when every trait is a built-in derive (`Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`) named directly or through its canonical `core` or `std` path. A final segment matching a built-in name does not establish that a qualified custom path is built-in. If a module or enclosing block imports a name or glob that could shadow a bare built-in derive, review it instead of applying an automatic fix. Lists containing custom derives or comments require review; the lint reports a commented unsorted list without a machine fix.
  5. `derive_not_last` reports misplaced derives; `unsorted_attributes` reports non-alphabetical safe attribute groups and built-in derive lists. Their machine suggestions never overlap in one scan. Re-scan after fixing, since moving a derive can expose a sortable group.
- **Exceptions**: Use the paired Dylint exception attributes from
  [README.md](README.md#suppressing-a-check). Attribute macro expansion order, derive helper requirements, or conditional expansion may require source order. Suppress only an otherwise reported item with `#[allow(unknown_lints, derive_not_last, unsorted_attributes, reason = "CG-007-attribute-order: <specific ordering dependency>")]`.
- **Counter-example**:

```rust
#[derive(Debug, Clone)]
#[repr(C)]
struct Header(u32);
```

The safe attributes and built-in derives are out of order.

- **Example**:

```rust
#[repr(C)]
#[derive(Clone, Debug)]
struct Header(u32);
```

- **Run**: `make dylint` scans the configured kernel; `make dylint XKMAKE_ARGS=--fix` applies safe suggestions to configured local packages; `make dylint_test` runs the UI tests.
