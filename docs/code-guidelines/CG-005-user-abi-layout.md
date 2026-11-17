---
id: CG-005-user-abi-layout
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [unsound_user_abi_impl]
---

# CG-005-user-abi-layout: Prove by-value user ABI layouts

- **Rule**: Every manual or derive-generated `UserRead` / `UserWrite` implementation
  must satisfy the `# Safety` contract in `posix/types/src/ptr.rs`. `UserRead`
  admits every byte pattern. `UserWrite` exposes only initialized bytes: no
  implicit field, tail, or nested padding. ABI records have a primitive,
  `repr(C)`, `repr(transparent)`, or `repr(packed)` layout.
- **Scope**: Implementations compiled by the selected kernel configuration,
  including macro output and implementations for foreign types. Generic
  implementations must be layout independent: a thin raw pointer, a transparent
  thin raw-pointer wrapper, or a (possibly nested) array whose leaf element has
  the matching trait bound or a concrete layout proof.
  Other generic records require a concrete carrier or a focused reviewed
  exception; checking one use site cannot prove every future instantiation.
  Code not built by either CI
  `qemu_defconfig` is outside those scans.
- **Why**: Reading arbitrary bytes into a type with invalid bit patterns is
  undefined behavior. Copying padding to user space can disclose kernel memory.
  A union value initialized through a smaller member can leave the rest of the
  union uninitialized, even when its layout has no outer padding.
- **How to check**:
  1. Resolve the implemented trait by identity, including derive output and
     reexports. Resolve concrete types and use `tcx.layout_of` for size, field
     offsets and field layouts; recursively inspect nested records and arrays.
  2. For `UserWrite`, every byte must belong to an initialized field. Every
     union member must cover the full union size and meet the same contract.
     Explicit padding fields count when constructors initialize them. Data
     enums are rejected until each variant's payload layout has its own proof;
     a scalar enum's discriminant storage must cover its full size, including
     any alignment-expanded tail;
     padding-free `bool` and `char` fields are valid on the write side.
  3. For `UserRead`, reject niches and invalid scalar patterns, including
     `bool`, `char`, references, nonzero integers, and discriminated enums.
     Thin raw pointer values are permitted; the copy does not dereference them.
     A fat pointer needs a metadata validity proof, so a `T: ?Sized` pointer
     wrapper is rejected.
     Scalar layout alone does not prove a payload valid: `Option<&T>` is
     rejected. The nullable function-pointer form `Option<extern "C" fn()>`
     is accepted because null encodes `None` and every non-null word meets the
     function-pointer validity requirement.
  4. Check the record's representation and the bounds on generic array
     elements. Review constructors of union-bearing records: a type-level
     layout check alone cannot prove that every union byte was initialized.
- **Exceptions**: Use the paired Dylint exception attributes from
  [README.md](README.md#suppressing-a-check). A focused `#[allow(unknown_lints, unsound_user_abi_impl,
  reason = "CG-005-user-abi-layout: <layout or construction proof>")]` may
  document an invariant the lint cannot prove, such as an always-zeroed union
  or a separately proven generic layout. The reason must identify the constructor
  and show that every copied byte is initialized, or explain why every input
  bit pattern is valid. An allow cannot excuse a known disclosure or invalid
  value.
- **Counter-example**:

```rust
#[repr(C)]
struct Reply { tag: u8, value: u32 } // three implicit padding bytes
// SAFETY: false: Reply contains uninitialized padding.
unsafe impl UserWrite for Reply {}
```

- **Example**:

```rust
#[repr(C)]
struct Reply { tag: u8, abi_pad: [u8; 3], value: u32 }
// SAFETY: every byte is an initialized integer field, including abi_pad.
unsafe impl UserWrite for Reply {}
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs
  UI tests. See `tools/lints/README.md`.
