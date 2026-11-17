// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Layout checks for unsafe user ABI trait implementations.
//! Rule: `docs/code-guidelines/CG-005-user-abi-layout.md`.

use rustc_abi::Variants;
use rustc_hir::{Item, ItemKind, def_id::DefId};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty, TypeVisitableExt};

rustc_session::declare_lint! {
    /// ### What it does
    /// Checks `UserRead` and `UserWrite` implementations, including derives,
    /// against the actual compiler layout. `UserRead` must admit every bit
    /// pattern. `UserWrite` must have no implicit padding; each union member
    /// must cover the entire union. ABI records need a stable representation.
    pub UNSOUND_USER_ABI_IMPL,
    Deny,
    "UserRead or UserWrite implementation violates its layout contract",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(UnsoundUserAbiImpl => [UNSOUND_USER_ABI_IMPL]);

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

/// Returns the first concrete reason the ABI contract cannot be proven.
fn check_type<'tcx>(
    cx: &LateContext<'tcx>,
    ty: Ty<'tcx>,
    direction: Direction,
    depth: usize,
) -> Result<(), String> {
    if depth > 32 {
        return Err(format!("{ty} is nested too deeply to verify"));
    }
    let tcx = cx.tcx;
    let layout = tcx
        .layout_of(cx.typing_env().as_query_input(ty))
        .map_err(|_| format!("cannot calculate layout of {ty}"))?;
    if matches!(direction, Direction::Read) && layout.largest_niche.is_some() {
        return Err(format!("{ty} has invalid bit patterns"));
    }

    match ty.kind() {
        ty::Array(element, _) => check_type(cx, *element, direction, depth + 1),
        ty::Tuple(fields) => {
            // Only the unit type is an approved tuple ABI carrier.
            if fields.is_empty() {
                Ok(())
            } else {
                Err(format!("{ty} has no stable ABI representation"))
            }
        }
        ty::Adt(definition, args) => {
            let repr = definition.repr();
            if definition.is_enum() {
                let variants = definition.variants();
                // Nullable function pointers occupy one word. Null is None;
                // every non-null word is a valid function-pointer value.
                let is_nullable_fn = layout.backend_repr.is_scalar()
                    && layout.largest_niche.is_none()
                    && variants.len() == 2
                    && variants.iter().filter(|v| v.fields.is_empty()).count() == 1
                    && variants.iter().any(|v| {
                        v.fields.len() == 1
                            && v.fields.iter().next().is_some_and(|field| {
                                matches!(field.ty(tcx, args).kind(), ty::FnPtr(..))
                            })
                    });
                if is_nullable_fn {
                    return Ok(());
                }
                if matches!(direction, Direction::Read) {
                    return Err(format!("{ty} has unproven enum discriminants or payloads"));
                }
                if !(repr.c() || repr.transparent() || repr.packed()) {
                    return Err(format!(
                        "{ty} needs repr(C), repr(transparent), or repr(packed)"
                    ));
                }
                if variants.iter().any(|v| !v.fields.is_empty()) {
                    return Err(format!("{ty} is a data enum with unproven variant padding"));
                }
                let tag_size = match layout.variants {
                    Variants::Multiple { tag, .. } => tag.size(&tcx).bytes(),
                    Variants::Single { .. } | Variants::Empty => 0,
                };
                if tag_size != layout.size.bytes() {
                    return Err(format!(
                        "{ty} has implicit enum padding after byte {tag_size}"
                    ));
                }
                return Ok(());
            }
            if !(repr.c() || repr.transparent() || repr.packed()) {
                return Err(format!(
                    "{ty} needs repr(C), repr(transparent), or repr(packed)"
                ));
            }

            let fields = &definition.non_enum_variant().fields;
            let mut intervals = Vec::new();
            for (index, field) in fields.iter().enumerate() {
                let field_ty = field.ty(tcx, args);
                check_type(cx, field_ty, direction, depth + 1)?;
                if matches!(direction, Direction::Write) {
                    let field_layout = tcx
                        .layout_of(cx.typing_env().as_query_input(field_ty))
                        .map_err(|_| {
                            format!("cannot calculate layout of field {} in {ty}", field.name)
                        })?;
                    let start = layout.fields.offset(index).bytes();
                    let end = start + field_layout.size.bytes();
                    if definition.is_union() && (start != 0 || end != layout.size.bytes()) {
                        return Err(format!(
                            "union {ty} member {} covers {end} of {} bytes",
                            field.name,
                            layout.size.bytes()
                        ));
                    }
                    intervals.push((start, end));
                }
            }
            if matches!(direction, Direction::Write) && !definition.is_union() {
                intervals.sort_unstable();
                let mut covered = 0;
                for (start, end) in intervals {
                    if start > covered {
                        return Err(format!("{ty} has implicit padding at byte {covered}"));
                    }
                    covered = covered.max(end);
                }
                if covered != layout.size.bytes() {
                    return Err(format!("{ty} has implicit tail padding at byte {covered}"));
                }
            }
            Ok(())
        }
        ty::Bool | ty::Char | ty::Ref(..) | ty::FnPtr(..)
            if matches!(direction, Direction::Read) =>
        {
            Err(format!("{ty} does not admit every bit pattern"))
        }
        ty::Bool | ty::Char | ty::FnPtr(..) if matches!(direction, Direction::Write) => Ok(()),
        ty::RawPtr(pointee, _)
            if matches!(direction, Direction::Read)
                && !pointee.is_sized(cx.tcx, cx.typing_env()) =>
        {
            Err(format!(
                "{ty} has raw-pointer metadata without a validity proof"
            ))
        }
        ty::RawPtr(..) | ty::Int(..) | ty::Uint(..) | ty::Float(..) => Ok(()),
        _ => Err(format!("{ty} has no proven user ABI layout")),
    }
}

/// Prove the generic shapes whose layout is independent of type arguments.
fn check_generic_impl<'tcx>(
    cx: &LateContext<'tcx>,
    impl_id: DefId,
    trait_id: DefId,
    ty: Ty<'tcx>,
    direction: Direction,
) -> Result<(), String> {
    let field = match ty.kind() {
        ty::RawPtr(pointee, _) if pointee.is_sized(cx.tcx, cx.typing_env()) => return Ok(()),
        ty::Array(element, _) => *element,
        ty::Adt(definition, _) if definition.is_enum() => {
            return Err(format!("generic enum {ty} needs a concrete ABI proof"));
        }
        ty::Adt(definition, args) if definition.repr().transparent() => {
            let fields = &definition.non_enum_variant().fields;
            if fields.len() == 1 {
                let field = fields.iter().next().expect("one field").ty(cx.tcx, args);
                if let ty::RawPtr(pointee, _) = field.kind() {
                    if pointee.is_sized(cx.tcx, cx.typing_env()) {
                        return Ok(());
                    }
                    return Err(format!(
                        "generic {ty} has raw-pointer metadata without a validity proof"
                    ));
                }
                if matches!(field.kind(), ty::Array(..)) {
                    field
                } else {
                    return Err(format!(
                        "generic {ty} needs a layout-independent field proof"
                    ));
                }
            } else {
                return Err(format!(
                    "generic {ty} needs a layout-independent field proof"
                ));
            }
        }
        _ => {
            return Err(format!(
                "generic {ty} needs a layout-independent field proof"
            ));
        }
    };
    let mut field = field;
    while let ty::Array(element, _) = field.kind() {
        field = *element;
    }
    let has_bound = cx
        .tcx
        .predicates_of(impl_id)
        .predicates
        .iter()
        .any(|(clause, _)| {
            if let ty::ClauseKind::Trait(predicate) = clause.kind().skip_binder() {
                predicate.trait_ref.def_id == trait_id && predicate.self_ty() == field
            } else {
                false
            }
        });
    if has_bound || (!field.has_param() && check_type(cx, field, direction, 0).is_ok()) {
        Ok(())
    } else {
        Err(format!(
            "generic {ty} needs an element UserRead/UserWrite bound"
        ))
    }
}

impl<'tcx> LateLintPass<'tcx> for UnsoundUserAbiImpl {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Impl(_) = item.kind else { return };
        let Some(trait_id) = cx.tcx.impl_opt_trait_id(item.owner_id) else {
            return;
        };
        if cx.tcx.crate_name(trait_id.krate).as_str() != "posix_types" {
            return;
        }
        let direction = match cx.tcx.item_name(trait_id).as_str() {
            "UserRead" => Direction::Read,
            "UserWrite" => Direction::Write,
            _ => return,
        };
        let ty = cx.tcx.type_of(item.owner_id).instantiate_identity();
        let result = if ty.has_param() {
            check_generic_impl(cx, item.owner_id.to_def_id(), trait_id, ty, direction)
        } else {
            check_type(cx, ty, direction, 0)
        };
        if let Err(reason) = result {
            cx.tcx.node_span_lint(
                UNSOUND_USER_ABI_IMPL,
                item.hir_id(),
                item.span,
                |diagnostic| {
                    diagnostic
                        .primary_message(format!("unsound user ABI implementation: {reason}"));
                    diagnostic.help("see docs/code-guidelines/CG-005-user-abi-layout.md");
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    const MOCK_POSIX_TYPES: crate::testing::MockCrate = crate::testing::MockCrate {
        name: "posix_types",
        proc_macro: false,
        source: r#"pub unsafe trait UserRead {} pub unsafe trait UserWrite {}"#,
    };
    const SOURCE: &str = r#"#![allow(dead_code, unused)]
use posix_types::{UserRead, UserWrite};
#[repr(C)] struct Padded { a: u8, b: u32 }
// SAFETY: test fixture intentionally violates the contract.
unsafe impl UserWrite for Padded {}
#[repr(C)] struct BadRead { valid: bool }
// SAFETY: test fixture intentionally violates the contract.
unsafe impl UserRead for BadRead {}
#[repr(C)] union Partial { small: u32, wide: u64 }
// SAFETY: test fixture intentionally violates the contract.
unsafe impl UserWrite for Partial {}
#[repr(C)] struct Good { a: u32, b: u32 }
// SAFETY: every byte is initialized.
unsafe impl UserWrite for Good {}
// SAFETY: every bit pattern is valid.
unsafe impl UserRead for Good {}
#[repr(C)] struct Explicit { tag: u8, abi_pad: [u8; 3], value: u32 }
// SAFETY: all bytes are fields.
unsafe impl UserWrite for Explicit {}
#[repr(C)] union Full { word: u64, bytes: [u8; 8] }
// SAFETY: either member initializes the entire union.
unsafe impl UserWrite for Full {}
#[repr(C)] enum DataEnum { Small(u8), Wide(u64) }
// SAFETY: test fixture intentionally has variant padding.
unsafe impl UserWrite for DataEnum {}
#[repr(C, align(16))] enum AlignedEnum { A, B }
// SAFETY: test fixture intentionally has trailing enum padding.
unsafe impl UserWrite for AlignedEnum {}
#[repr(C)] struct RefOption(Option<&'static u32>);
// SAFETY: test fixture intentionally has an invalid reference payload.
unsafe impl UserRead for RefOption {}
#[repr(transparent)] enum GenericEnum<T> { Pointer(*const T) }
// SAFETY: test fixture intentionally needs an enum proof.
unsafe impl<T> UserWrite for GenericEnum<T> {}
#[repr(transparent)] struct BoolOut(bool);
// SAFETY: a valid bool always has initialized bytes.
unsafe impl UserWrite for BoolOut {}
#[repr(transparent)] struct CharOut(char);
// SAFETY: a valid char always has initialized bytes.
unsafe impl UserWrite for CharOut {}
#[repr(C)] struct NullableFn { handler: Option<extern "C" fn()> }
// SAFETY: null is None and every non-null word is a valid function pointer.
unsafe impl UserRead for NullableFn {}
#[repr(transparent)] struct Nested<T, const M: usize, const N: usize>([[T; M]; N]);
// SAFETY: each nested element has the matching contract.
unsafe impl<T: UserWrite, const M: usize, const N: usize> UserWrite for Nested<T, M, N> {}
#[repr(transparent)] struct NestedNoBound<T, const M: usize, const N: usize>([[T; M]; N]);
// SAFETY: test fixture intentionally lacks the element bound.
unsafe impl<T, const M: usize, const N: usize> UserWrite for NestedNoBound<T, M, N> {}
#[repr(transparent)] struct Pointer<T>(*mut T);
// SAFETY: a transparent raw pointer admits every bit pattern.
unsafe impl<T> UserRead for Pointer<T> {}
// SAFETY: a transparent raw pointer has no padding.
unsafe impl<T> UserWrite for Pointer<T> {}
#[repr(transparent)] struct Fat<T: ?Sized>(*const T);
// SAFETY: test fixture intentionally leaves fat-pointer metadata unproven.
unsafe impl<T: ?Sized> UserRead for Fat<T> {}
#[repr(transparent)] struct ConcreteNested<const M: usize, const N: usize>([[u32; M]; N]);
// SAFETY: u32 elements and nested arrays are padding-free.
unsafe impl<const M: usize, const N: usize> UserWrite for ConcreteNested<M, N> {}
#[repr(C)] struct Generic<T> { tag: u8, value: T }
// SAFETY: test fixture intentionally lacks a generic layout proof.
unsafe impl<T> UserWrite for Generic<T> {}
#[repr(C)] struct AliasTarget { tag: u8, value: u32 }
type Alias = AliasTarget;
// SAFETY: test fixture intentionally has padding.
unsafe impl UserWrite for Alias {}
#[repr(C)] struct MacroTarget { tag: u8, value: u32 }
macro_rules! emit_impl { () => {
    // SAFETY: test fixture intentionally has padding.
    unsafe impl UserWrite for MacroTarget {}
}; }
emit_impl!();
#[cfg(any())]
// SAFETY: disabled configuration is not compiled.
unsafe impl UserRead for bool {}
#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, unsound_user_abi_impl, reason = "CG-005-user-abi-layout: fixture demonstrates focused exception")]
// SAFETY: test fixture intentionally violates the contract.
unsafe impl UserWrite for PaddedAllowed {}
#[repr(C)] struct PaddedAllowed { a: u8, b: u32 }
fn main() {}
"#;
    const STDERR: &str = r#"error: unsound user ABI implementation: Padded has implicit padding at byte 1
  --> $DIR/unsound_user_abi_impl.rs:5:1
   |
LL | unsafe impl UserWrite for Padded {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md
   = note: `#[deny(unsound_user_abi_impl)]` on by default

error: unsound user ABI implementation: BadRead has invalid bit patterns
  --> $DIR/unsound_user_abi_impl.rs:8:1
   |
LL | unsafe impl UserRead for BadRead {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: union Partial member small covers 4 of 8 bytes
  --> $DIR/unsound_user_abi_impl.rs:11:1
   |
LL | unsafe impl UserWrite for Partial {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: DataEnum is a data enum with unproven variant padding
  --> $DIR/unsound_user_abi_impl.rs:25:1
   |
LL | unsafe impl UserWrite for DataEnum {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: AlignedEnum has implicit enum padding after byte 4
  --> $DIR/unsound_user_abi_impl.rs:28:1
   |
LL | unsafe impl UserWrite for AlignedEnum {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: std::option::Option<&'static u32> has unproven enum discriminants or payloads
  --> $DIR/unsound_user_abi_impl.rs:31:1
   |
LL | unsafe impl UserRead for RefOption {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: generic enum GenericEnum<T> needs a concrete ABI proof
  --> $DIR/unsound_user_abi_impl.rs:34:1
   |
LL | unsafe impl<T> UserWrite for GenericEnum<T> {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: generic NestedNoBound<T, M, N> needs an element UserRead/UserWrite bound
  --> $DIR/unsound_user_abi_impl.rs:49:1
   |
LL | unsafe impl<T, const M: usize, const N: usize> UserWrite for NestedNoBound<T, M, N> {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: generic Fat<T> has raw-pointer metadata without a validity proof
  --> $DIR/unsound_user_abi_impl.rs:57:1
   |
LL | unsafe impl<T: ?Sized> UserRead for Fat<T> {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: generic Generic<T> needs a layout-independent field proof
  --> $DIR/unsound_user_abi_impl.rs:63:1
   |
LL | unsafe impl<T> UserWrite for Generic<T> {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: AliasTarget has implicit padding at byte 1
  --> $DIR/unsound_user_abi_impl.rs:67:1
   |
LL | unsafe impl UserWrite for Alias {}
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md

error: unsound user ABI implementation: MacroTarget has implicit padding at byte 1
  --> $DIR/unsound_user_abi_impl.rs:71:5
   |
LL |     unsafe impl UserWrite for MacroTarget {}
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
LL | }; }
LL | emit_impl!();
   | ------------ in this macro invocation
   |
   = help: see docs/code-guidelines/CG-005-user-abi-layout.md
   = note: this error originates in the macro `emit_impl` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 12 previous errors

"#;
    #[test]
    fn ui() {
        crate::testing::run_ui("unsound_user_abi_impl", SOURCE, STDERR, &[MOCK_POSIX_TYPES]);
    }
}
