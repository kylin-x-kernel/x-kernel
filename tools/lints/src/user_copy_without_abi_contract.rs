// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed user copies through unbounded `osvm` primitives or raw-pointer methods.
//! Rule: `docs/code-guidelines/CG-006-user-copy-abi-contract.md`.

use rustc_hir::{
    Expr, ExprKind,
    def::{DefKind, Res},
    def_id::{DefId, LOCAL_CRATE},
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};
use rustc_trait_selection::infer::{InferCtxtExt, TyCtxtInferExt};

rustc_session::declare_lint! {
    /// ### What it does
    /// Reports typed user-memory copies through unbounded `osvm` primitives or
    /// raw-pointer virtual-memory methods. Use `UserPtr` / `UserConstPtr` APIs
    /// requiring `UserWrite` / `UserRead` instead.
    pub USER_COPY_WITHOUT_ABI_CONTRACT,
    Deny,
    "typed user-memory copy without a UserRead or UserWrite bound",
    report_in_external_macro
}

rustc_session::declare_lint_pass!(UserCopyWithoutAbiContract => [USER_COPY_WITHOUT_ABI_CONTRACT]);

fn is_byte(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Uint(ty::UintTy::U8))
}

fn pointer_element(ty: Ty<'_>) -> Option<Ty<'_>> {
    match ty.kind() {
        ty::RawPtr(pointee, _) => Some(*pointee),
        _ => None,
    }
}

fn slice_element(ty: Ty<'_>) -> Option<Ty<'_>> {
    let ty::Ref(_, pointee, _) = ty.kind() else {
        return None;
    };
    match pointee.kind() {
        ty::Slice(element) | ty::Array(element, _) => Some(*element),
        _ => None,
    }
}

fn external_trait(cx: &LateContext<'_>, crate_name: &str, trait_name: &str) -> Option<DefId> {
    if cx.tcx.crate_name(LOCAL_CRATE).as_str() == crate_name {
        return cx
            .tcx
            .hir_crate_items(())
            .free_items()
            .map(|id| id.owner_id.to_def_id())
            .find(|id| {
                cx.tcx.def_kind(*id) == DefKind::Trait
                    && cx.tcx.item_name(*id).as_str() == trait_name
            });
    }
    cx.tcx
        .crates(())
        .iter()
        .copied()
        .find(|krate| cx.tcx.crate_name(*krate).as_str() == crate_name)
        .and_then(|krate| {
            cx.tcx
                .module_children(krate.as_def_id())
                .iter()
                .find_map(|child| match child.res {
                    Res::Def(DefKind::Trait, id) if child.ident.name.as_str() == trait_name => {
                        Some(id)
                    }
                    _ => None,
                })
        })
}

fn has_trait_bound<'tcx>(
    cx: &LateContext<'tcx>,
    ty: Ty<'tcx>,
    crate_name: &str,
    trait_name: &str,
) -> bool {
    let Some(trait_id) = external_trait(cx, crate_name, trait_name) else {
        return false;
    };
    cx.tcx
        .infer_ctxt()
        .build(cx.typing_mode())
        .type_implements_trait(trait_id, [ty], cx.param_env)
        .must_apply_modulo_regions()
}

fn success_type(ty: Ty<'_>) -> Option<Ty<'_>> {
    let ty::Adt(_definition, args) = ty.kind() else {
        return None;
    };
    args.types().next()
}

fn uninit_element(ty: Ty<'_>) -> Option<Ty<'_>> {
    success_type(ty).and_then(success_type)
}

fn has_read_contract<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> bool {
    has_trait_bound(cx, ty, "posix_types", "UserRead")
        || has_trait_bound(cx, ty, "bytemuck", "AnyBitPattern")
}

fn has_abi_bound<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>, direction: &str) -> bool {
    if matches!(direction, "read_vm" | "read_vm_mem" | "read_uninit") {
        has_read_contract(cx, ty)
    } else {
        has_trait_bound(cx, ty, "posix_types", "UserWrite")
    }
}

impl<'tcx> LateLintPass<'tcx> for UserCopyWithoutAbiContract {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let (definition, element, direction) = match expr.kind {
            ExprKind::Call(callee, args) => {
                let ty::FnDef(definition, _) = cx.typeck_results().expr_ty(callee).kind() else {
                    return;
                };
                let name = cx.tcx.item_name(*definition);
                if args.is_empty() {
                    return;
                }
                if name.as_str() == "read_uninit"
                    && matches!(
                        cx.tcx.crate_name(definition.krate).as_str(),
                        "osvm" | "posix_types"
                    )
                {
                    if let Some(element) = uninit_element(cx.typeck_results().expr_ty(expr)) {
                        emit_copy_lint(cx, expr, element, "read_uninit");
                    }
                    return;
                }
                let element = match name.as_str() {
                    "read_vm_mem" | "write_vm_mem" => {
                        pointer_element(cx.typeck_results().expr_ty(&args[0]))
                    }
                    "read_vm" => success_type(cx.typeck_results().expr_ty(expr)),
                    "write_vm" => args.get(1).map(|arg| cx.typeck_results().expr_ty(arg)),
                    "write_vm_slice" => args
                        .get(1)
                        .and_then(|arg| slice_element(cx.typeck_results().expr_ty(arg))),
                    _ => return,
                };
                (*definition, element, name)
            }
            ExprKind::MethodCall(_, _receiver, args, _) => {
                let Some(definition) = cx.typeck_results().type_dependent_def_id(expr.hir_id)
                else {
                    return;
                };
                let name = cx.tcx.item_name(definition);
                if name.as_str() == "read_uninit"
                    && matches!(
                        cx.tcx.crate_name(definition.krate).as_str(),
                        "osvm" | "posix_types"
                    )
                {
                    if let Some(element) = uninit_element(cx.typeck_results().expr_ty(expr)) {
                        emit_copy_lint(cx, expr, element, "read_uninit");
                    }
                    return;
                }
                if !matches!(name.as_str(), "read_vm" | "write_vm" | "write_vm_slice") {
                    return;
                }
                let element = match name.as_str() {
                    "read_vm" => success_type(cx.typeck_results().expr_ty(expr)),
                    "write_vm" => args.first().map(|arg| cx.typeck_results().expr_ty(arg)),
                    "write_vm_slice" => args
                        .first()
                        .and_then(|arg| slice_element(cx.typeck_results().expr_ty(arg))),
                    _ => None,
                };
                (definition, element, name)
            }
            _ => return,
        };
        if cx.tcx.crate_name(definition.krate).as_str() != "osvm"
            || cx.tcx.crate_name(LOCAL_CRATE).as_str() == "osvm"
        {
            return;
        }
        if let Some(element) = element {
            emit_copy_lint(cx, expr, element, direction.as_str());
        }
    }
}

fn emit_copy_lint<'tcx>(
    cx: &LateContext<'tcx>,
    expr: &Expr<'tcx>,
    element: Ty<'tcx>,
    direction: &str,
) {
    if is_byte(element) || has_abi_bound(cx, element, direction) {
        return;
    }
    cx.tcx.node_span_lint(
        USER_COPY_WITHOUT_ABI_CONTRACT,
        expr.hir_id,
        expr.span,
        |diagnostic| {
            diagnostic.primary_message(format!(
                "`{direction}` copies a typed value without a user ABI contract"
            ));
            diagnostic.help(
                "prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a \
                 focused low-level exception; see \
                 docs/code-guidelines/CG-006-user-copy-abi-contract.md",
            );
        },
    );
}

#[cfg(test)]
mod tests {
    const MOCK_BYTEMUCK: crate::testing::MockCrate = crate::testing::MockCrate {
        name: "bytemuck",
        proc_macro: false,
        source: r#"pub unsafe trait AnyBitPattern {}
// SAFETY: all u32 bit patterns are valid.
unsafe impl AnyBitPattern for u32 {}"#,
    };
    const MOCK_OSVM: crate::testing::MockCrate = crate::testing::MockCrate {
        name: "osvm",
        proc_macro: false,
        source: r#"
pub fn read_vm_mem<T>(_: *const T, _: &mut [core::mem::MaybeUninit<T>]) {}
pub fn write_vm_mem<T>(_: *mut T, _: &[T]) {}
pub trait VirtPtr: Copy {
    type Target;
    fn read_uninit(self) -> Result<core::mem::MaybeUninit<Self::Target>, ()> {
        Ok(core::mem::MaybeUninit::uninit())
    }
    fn read_vm(self) -> Result<Self::Target, ()> where Self::Target: Default {
        Ok(Self::Target::default())
    }
}
pub trait VirtMutPtr: VirtPtr {
    fn write_vm(self, _: Self::Target) {}
    fn write_vm_slice(self, _: &[Self::Target]) {}
}
impl<T> VirtPtr for *const T { type Target = T; }
impl<T> VirtPtr for *mut T { type Target = T; }
impl<T> VirtMutPtr for *mut T {}
impl<T> VirtPtr for core::ptr::NonNull<T> { type Target = T; }
impl<T> VirtMutPtr for core::ptr::NonNull<T> {}
#[macro_export]
macro_rules! external_copy { ($p:expr, $v:expr) => { $crate::write_vm_mem($p, &[$v]) }; }
"#,
    };

    const MOCK_POSIX_TYPES: crate::testing::MockCrate = crate::testing::MockCrate {
        name: "posix_types",
        proc_macro: false,
        source: r#"
pub unsafe trait UserRead {}
pub unsafe trait UserWrite {}
#[repr(transparent)] pub struct UserPtr<T>(*const T);
impl<T> UserPtr<T> {
    pub fn from_ptr(ptr: *const T) -> Self { Self(ptr) }
    pub fn read_uninit(self) -> Result<core::mem::MaybeUninit<T>, ()> {
        let _ = self.0;
        Ok(core::mem::MaybeUninit::uninit())
    }
}
"#,
    };

    const SOURCE: &str = r#"#![allow(dead_code, unused)]
use osvm::{VirtMutPtr, VirtPtr, read_vm_mem as copy_in, write_vm_mem as copy_out};
use bytemuck as _;
use posix_types::{UserPtr, UserRead, UserWrite};

#[derive(Clone, Copy, Default)]
struct Record(u32);
#[repr(C)] #[derive(Clone, Copy)] struct BoundRecord(u32);
// SAFETY: every byte is an initialized integer.
unsafe impl UserWrite for BoundRecord {}
// SAFETY: every byte pattern is a valid BoundRecord.
unsafe impl UserRead for BoundRecord {}

fn flagged(p: *mut Record, value: Record) {
    copy_out(p, &[value]);
    let mut slot = [core::mem::MaybeUninit::<Record>::uninit()];
    copy_in(p.cast_const(), &mut slot);
    p.write_vm(value);
    p.write_vm_slice(&[value]);
    let _: Record = (p as *const Record).read_vm().unwrap();
}

fn generic<T: Copy>(p: *mut T, value: T) {
    copy_out(p, &[value]);
}

fn ufcs(p: *mut Record, value: Record) {
    VirtMutPtr::write_vm(p, value);
    <*mut Record as VirtMutPtr>::write_vm(p, value);
    let p = core::ptr::NonNull::new(p).unwrap();
    p.write_vm(value);
}
fn generic_pointer<P: VirtMutPtr<Target = Record>>(p: P, value: Record) {
    p.write_vm(value);
}
fn bounded<T: Copy + UserWrite>(p: *mut T, value: T) {
    p.write_vm(value);
}
fn concrete_bound(p: *mut BoundRecord, value: BoundRecord) {
    p.write_vm(value);
}
fn valid_raw_read(p: *const u32) {
    let _: u32 = p.read_vm().unwrap();
    let _: u32 = VirtPtr::read_vm(p).unwrap();
}
fn valid_generic_read<T: Default + UserRead>(p: *const T) {
    let _: T = p.read_vm().unwrap();
    let _: T = VirtPtr::read_vm(p).unwrap();
}
fn unchecked_read(p: *const bool) {
    let _: bool = unsafe { p.read_uninit().unwrap().assume_init() };
    let _: bool = unsafe { UserPtr::from_ptr(p).read_uninit().unwrap().assume_init() };
    let value = p.read_uninit().unwrap();
    let _: bool = unsafe { value.assume_init_read() };
    let _retained = p.read_uninit();
}
#[allow(untyped_error_result, reason = "mirrors the mock read_uninit error type")] fn unchecked_question(p: *const bool) -> Result<(), ()> {
    let value = p.read_uninit()?;
    let _: bool = unsafe { value.assume_init() };
    Ok(())
}
fn unchecked_saved(p: *const bool) {
    let saved = p.read_uninit();
    let _: bool = unsafe { saved.unwrap().assume_init() };
}
fn external(p: *mut Record, value: Record) { osvm::external_copy!(p, value); }

macro_rules! copy_record { ($p:expr, $v:expr) => { copy_out($p, &[$v]); }; }
fn expanded(p: *mut Record, value: Record) { copy_record!(p, value); }

#[cfg(any())]
fn disabled(p: *mut Record, value: Record) { copy_out(p, &[value]); }

fn bytes(p: *mut u8, value: u8) {
    copy_out(p, &[value]);
    p.write_vm(value);
}

#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, user_copy_without_abi_contract, reason = "CG-006-user-copy-abi-contract: low-level copy test")]
fn allowed(p: *mut Record, value: Record) {
    p.write_vm(value);
}

fn main() {}
"#;

    const STDERR: &str = r#"error: `write_vm_mem` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:15:5
   |
LL |     copy_out(p, &[value]);
   |     ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md
   = note: `#[deny(user_copy_without_abi_contract)]` on by default

error: `read_vm_mem` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:17:5
   |
LL |     copy_in(p.cast_const(), &mut slot);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:18:5
   |
LL |     p.write_vm(value);
   |     ^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm_slice` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:19:5
   |
LL |     p.write_vm_slice(&[value]);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:20:21
   |
LL |     let _: Record = (p as *const Record).read_vm().unwrap();
   |                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm_mem` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:24:5
   |
LL |     copy_out(p, &[value]);
   |     ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:28:5
   |
LL |     VirtMutPtr::write_vm(p, value);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:29:5
   |
LL |     <*mut Record as VirtMutPtr>::write_vm(p, value);
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:31:5
   |
LL |     p.write_vm(value);
   |     ^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:34:5
   |
LL |     p.write_vm(value);
   |     ^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:51:28
   |
LL |     let _: bool = unsafe { p.read_uninit().unwrap().assume_init() };
   |                            ^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:52:28
   |
LL |     let _: bool = unsafe { UserPtr::from_ptr(p).read_uninit().unwrap().assume_init() };
   |                            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:53:17
   |
LL |     let value = p.read_uninit().unwrap();
   |                 ^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:55:21
   |
LL |     let _retained = p.read_uninit();
   |                     ^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:58:17
   |
LL |     let value = p.read_uninit()?;
   |                 ^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `read_uninit` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:63:17
   |
LL |     let saved = p.read_uninit();
   |                 ^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md

error: `write_vm_mem` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:66:46
   |
LL | fn external(p: *mut Record, value: Record) { osvm::external_copy!(p, value); }
   |                                              ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md
   = note: this error originates in the macro `osvm::external_copy` (in Nightly builds, run with -Z macro-backtrace for more info)

error: `write_vm_mem` copies a typed value without a user ABI contract
  --> $DIR/user_copy_without_abi_contract.rs:68:52
   |
LL | macro_rules! copy_record { ($p:expr, $v:expr) => { copy_out($p, &[$v]); }; }
   |                                                    ^^^^^^^^^^^^^^^^^^^
LL | fn expanded(p: *mut Record, value: Record) { copy_record!(p, value); }
   |                                              ---------------------- in this macro invocation
   |
   = help: prove UserRead/AnyBitPattern for reads or UserWrite for writes, or document a focused low-level exception; see docs/code-guidelines/CG-006-user-copy-abi-contract.md
   = note: this error originates in the macro `copy_record` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 18 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui(
            "user_copy_without_abi_contract",
            SOURCE,
            STDERR,
            &[MOCK_OSVM, MOCK_POSIX_TYPES, MOCK_BYTEMUCK],
        );
    }
}
