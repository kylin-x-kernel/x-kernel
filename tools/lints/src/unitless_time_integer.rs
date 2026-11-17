// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Detects unitless integer names for explicitly time-related quantities. See CG-011.

use rustc_hir::{
    ImplItem, ImplItemKind, Item, ItemKind, Pat, PatKind, TraitFn, TraitItem, TraitItemKind,
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};

rustc_session::declare_lint! {
    /// ### What it does
    /// Reports integer-typed values with names such as `timeout` or `deadline` but no unit suffix.
    /// ### Why is this bad?
    /// Integer time values with different units are otherwise easy to confuse.
    /// ### Example
    /// `let timeout = 100;`
    pub UNITLESS_TIME_INTEGER, Deny, "integer time quantity name has no unit suffix", report_in_external_macro
}
rustc_session::declare_lint_pass!(UnitlessTimeInteger => [UNITLESS_TIME_INTEGER]);

const TIME_PARTS: &[&str] = &[
    "timeout", "delay", "interval", "period", "deadline", "duration",
];
const UNITS: &[&str] = &[
    "ns",
    "us",
    "ms",
    "s",
    "sec",
    "secs",
    "second",
    "seconds",
    "tick",
    "ticks",
    "jiffies",
    "nsec",
    "usec",
    "msec",
    "cycle",
    "cycles",
    "nanoseconds",
    "microseconds",
    "milliseconds",
    "seconds",
];
const NON_TIME_ENDINGS: &[&str] = &["count", "len", "length", "num", "index", "idx"];

fn needs_suffix(name: &str) -> bool {
    let normalized_name = name.to_ascii_lowercase();
    // Syscall multiplexed arguments and address pointers are not time values,
    // even though their ABI names contain a time-related word.
    if normalized_name.ends_with("_address")
        || normalized_name.contains("_or_value")
        || normalized_name.starts_with("tee_error_")
        || normalized_name.ends_with("_infinite")
    {
        return false;
    }
    let parts: Vec<_> = normalized_name.split('_').collect();
    !parts
        .last()
        .is_some_and(|part| NON_TIME_ENDINGS.contains(part))
        && parts.iter().any(|part| TIME_PARTS.contains(part))
        && !UNITS.contains(parts.last().unwrap_or(&""))
}
fn is_integer(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Int(_) | ty::Uint(_))
}
fn check<'tcx>(
    cx: &LateContext<'tcx>,
    id: rustc_hir::HirId,
    ident: rustc_span::Symbol,
    ty: Ty<'tcx>,
    span: rustc_span::Span,
) {
    let name = ident.as_str();
    if !needs_suffix(name) {
        return;
    }
    // Declared item and field types may be associated-type projections that
    // only resolve to an integer after normalization.
    let ty = cx
        .tcx
        .try_normalize_erasing_regions(cx.typing_env(), ty)
        .unwrap_or(ty);
    if is_integer(ty) {
        cx.tcx
            .node_span_lint(UNITLESS_TIME_INTEGER, id, span, |diag| {
                diag.primary_message(format!("integer time quantity `{name}` has no unit suffix"));
                diag.help(
                    "add the stored unit to the name (for example `_ms`); see \
                     docs/code-guidelines/CG-011-time-unit-names.md",
                );
            });
    }
}

impl<'tcx> LateLintPass<'tcx> for UnitlessTimeInteger {
    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'tcx>) {
        if let PatKind::Binding(_, _, ident, _) = pat.kind {
            check(
                cx,
                pat.hir_id,
                ident.name,
                cx.typeck_results().pat_ty(pat),
                pat.span,
            );
        }
    }

    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        match item.kind {
            ItemKind::Struct(_, _, fields) | ItemKind::Union(_, _, fields) => {
                check_fields(cx, fields.fields());
            }
            ItemKind::Enum(_, _, def) => {
                for variant in def.variants {
                    check_fields(cx, variant.data.fields());
                }
            }
            ItemKind::Const(..) | ItemKind::Static(..) => {
                let ty = cx.tcx.type_of(item.owner_id).instantiate_identity();
                check(
                    cx,
                    item.hir_id(),
                    cx.tcx.item_name(item.owner_id.to_def_id()),
                    ty,
                    item.span,
                );
            }
            _ => {}
        }
    }

    fn check_trait_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx TraitItem<'tcx>) {
        match item.kind {
            TraitItemKind::Const(ty, ..) => check(
                cx,
                item.hir_id(),
                item.ident.name,
                cx.tcx.type_of(item.owner_id).instantiate_identity(),
                ty.span,
            ),
            TraitItemKind::Fn(_sig, TraitFn::Required(names)) => {
                let input_tys = cx
                    .tcx
                    .fn_sig(item.owner_id)
                    .instantiate_identity()
                    .skip_binder()
                    .inputs();
                for (name, ty) in names.iter().zip(input_tys) {
                    if let Some(name) = name {
                        check(cx, item.hir_id(), name.name, *ty, name.span);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_impl_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ImplItem<'tcx>) {
        if let ImplItemKind::Const(ty, _) = item.kind {
            check(
                cx,
                item.hir_id(),
                item.ident.name,
                cx.tcx.type_of(item.owner_id).instantiate_identity(),
                ty.span,
            );
        }
    }
}

fn check_fields<'tcx>(cx: &LateContext<'tcx>, fields: &'tcx [rustc_hir::FieldDef<'tcx>]) {
    for field in fields {
        let ty = cx.tcx.type_of(field.def_id).instantiate_identity();
        check(cx, field.hir_id, field.ident.name, ty, field.span);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ui() {
        crate::testing::run_ui(
            "unitless_time_integer",
            r#"#![allow(dead_code)]
struct Timers { timeout: u64, timeout_ms: u64, deadline: i32, byte_count: usize, period_ticks: u32, timeout_count: usize, poll_interval_count: usize }
enum Event { Wait { retry_delay: u64 } }
trait Poller { fn wait(timeout: u64, poll_interval_count: usize); }
#[allow(non_upper_case_globals)]
trait TimeConstants { const timeout: u64; }
struct Clock;
impl Clock { const TIMEOUT: u64 = 1; const TIMEOUT_MS: u64 = 1; const TIMEOUT_COUNT: u64 = 1; }
#[allow(non_upper_case_globals)]
const request_timeout: u64 = 3;
#[allow(non_upper_case_globals)]
static poll_deadline: u32 = 4;
const TIMEOUT: u64 = 4;
static RETRY_DELAY: u64 = 5;
const TIMEOUT_US: u64 = 6;
static RETRY_DELAY_COUNT: u64 = 7;
const TEE_TIMEOUT_INFINITE: u32 = u32::MAX;
const TEE_ERROR_TIMEOUT: u32 = 1;
fn f(timeout: u64, delay_ns: u32, unrelated: u32, timeout_address: usize, timeout_or_value2: usize) {
    let interval = 10u64;
    let retry_delay_secs = 1u64;
    let bytes = 4usize;
    let _ = (timeout_address, timeout_or_value2);
}
struct DurationLike;
fn typed(_time: DurationLike) {}
type Nanos = u64;
struct Aliased { delay: Nanos }
trait Units { type Raw; }
impl Units for () { type Raw = u64; }
struct Projected { timeout: <() as Units>::Raw }
fn main() {}
"#,
            r###"error: integer time quantity `timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:2:17
   |
LL | struct Timers { timeout: u64, timeout_ms: u64, deadline: i32, byte_count: usize, period_ticks: u32, timeout_count: usize, poll_inter...
   |                 ^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md
   = note: `#[deny(unitless_time_integer)]` on by default

error: integer time quantity `deadline` has no unit suffix
  --> $DIR/unitless_time_integer.rs:2:48
   |
LL | struct Timers { timeout: u64, timeout_ms: u64, deadline: i32, byte_count: usize, period_ticks: u32, timeout_count: usize, poll_inter...
   |                                                ^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `retry_delay` has no unit suffix
  --> $DIR/unitless_time_integer.rs:3:21
   |
LL | enum Event { Wait { retry_delay: u64 } }
   |                     ^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:4:24
   |
LL | trait Poller { fn wait(timeout: u64, poll_interval_count: usize); }
   |                        ^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:6:38
   |
LL | trait TimeConstants { const timeout: u64; }
   |                                      ^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `TIMEOUT` has no unit suffix
  --> $DIR/unitless_time_integer.rs:8:29
   |
LL | impl Clock { const TIMEOUT: u64 = 1; const TIMEOUT_MS: u64 = 1; const TIMEOUT_COUNT: u64 = 1; }
   |                             ^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `request_timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:10:1
   |
LL | const request_timeout: u64 = 3;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `poll_deadline` has no unit suffix
  --> $DIR/unitless_time_integer.rs:12:1
   |
LL | static poll_deadline: u32 = 4;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `TIMEOUT` has no unit suffix
  --> $DIR/unitless_time_integer.rs:13:1
   |
LL | const TIMEOUT: u64 = 4;
   | ^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `RETRY_DELAY` has no unit suffix
  --> $DIR/unitless_time_integer.rs:14:1
   |
LL | static RETRY_DELAY: u64 = 5;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:19:6
   |
LL | fn f(timeout: u64, delay_ns: u32, unrelated: u32, timeout_address: usize, timeout_or_value2: usize) {
   |      ^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `interval` has no unit suffix
  --> $DIR/unitless_time_integer.rs:20:9
   |
LL |     let interval = 10u64;
   |         ^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `delay` has no unit suffix
  --> $DIR/unitless_time_integer.rs:28:18
   |
LL | struct Aliased { delay: Nanos }
   |                  ^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: integer time quantity `timeout` has no unit suffix
  --> $DIR/unitless_time_integer.rs:31:20
   |
LL | struct Projected { timeout: <() as Units>::Raw }
   |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add the stored unit to the name (for example `_ms`); see docs/code-guidelines/CG-011-time-unit-names.md

error: aborting due to 14 previous errors

"###,
            &[],
        );
    }
}
