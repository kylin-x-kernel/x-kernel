// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Records taken from bodies: `call` (MIR call and drop edges), `ref`
//! (functions used as values) and `site` (unsafe blocks).

use std::collections::HashSet;

use rustc_hir::{
    BlockCheckMode, Expr, ExprKind, UnsafeSource,
    def::DefKind,
    def_id::{DefId, LocalDefId},
    intravisit::{self, Visitor},
};
use rustc_middle::{
    mir::{self, Operand, TerminatorKind, visit::Visitor as MirVisitor},
    ty::{self, Instance, InstanceKind, Ty, TypeVisitableExt, TypingEnv},
};
use rustc_span::{Span, source_map::Spanned, sym};

use super::{Exporter, json, json::Record};

/// Callee module whose calls are formatting plumbing (`Argument::new_*`),
/// one per formatted argument; they are counted, not written.
const FMT_RT_PREFIX: &str = "core::fmt::rt::";

/// The caller side of the records written from one MIR body.
struct Caller<'b, 'tcx> {
    body: &'b mir::Body<'tcx>,
    key: String,
    /// Key of the enclosing fn when the body is a closure.
    root_key: Option<String>,
    root: LocalDefId,
    typing_env: TypingEnv<'tcx>,
}

/// How a call terminator's callee was resolved.
struct Callee {
    kind: &'static str,
    def: Option<DefId>,
    /// The impl item (or closure) a trait call resolves to.
    resolved: Option<DefId>,
    trait_id: Option<DefId>,
    generic: bool,
    name: Option<String>,
}

impl Callee {
    fn new(kind: &'static str, def: Option<DefId>) -> Self {
        Self {
            kind,
            def,
            resolved: None,
            trait_id: None,
            generic: false,
            name: None,
        }
    }
}

/// True when `scope` belongs to a body that the MIR inliner copied in.
fn is_inlined(body: &mir::Body<'_>, scope: mir::SourceScope) -> bool {
    scope.inlined_instance(&body.source_scopes).is_some()
}

impl<'tcx> Exporter<'tcx> {
    pub(super) fn bodies(&mut self) {
        let tcx = self.tcx;
        let owners: Vec<LocalDefId> = tcx
            .hir_body_owners()
            .filter(|def| {
                matches!(
                    tcx.def_kind(*def),
                    DefKind::Fn
                        | DefKind::AssocFn
                        | DefKind::Closure
                        | DefKind::Const { .. }
                        | DefKind::AssocConst { .. }
                        | DefKind::Static { .. }
                )
            })
            .collect();
        // Unsafe blocks first: calls look them up by typeck root, and a
        // closure body may come before the fn that contains it.
        for def in &owners {
            self.unsafe_sites(*def);
        }
        for def in owners {
            self.mir_of(def);
        }
    }

    /// Picks the earliest MIR still available for `def`; see the module
    /// documentation of `facts_export`.
    fn mir_of(&mut self, def: LocalDefId) {
        let tcx = self.tcx;
        let steal = tcx.mir_drops_elaborated_and_const_checked(def);
        if !steal.is_stolen() {
            let body = steal.borrow();
            self.body(def, &body);
            return;
        }
        let kind = tcx.def_kind(def);
        if !kind.is_fn_like() {
            self.bump("mir_ctfe_const");
            self.body(def, tcx.mir_for_ctfe(def.to_def_id()));
        } else if tcx.is_const_fn(def.to_def_id()) {
            self.bump("mir_ctfe_fn");
            self.body(def, tcx.mir_for_ctfe(def.to_def_id()));
        } else {
            // Coroutine layout needs optimised MIR during analysis; the
            // inliner leaves coroutine bodies alone.
            if tcx.is_coroutine(def.to_def_id()) {
                self.bump("mir_optimized_coroutine");
            } else {
                self.bump("mir_optimized_fn");
            }
            self.body(def, tcx.optimized_mir(def.to_def_id()));
        }
    }

    fn unsafe_sites(&mut self, def: LocalDefId) {
        let tcx = self.tcx;
        let Some(body) = tcx.hir_maybe_body_owned_by(def) else {
            return;
        };
        let mut finder = UnsafeFinder { spans: Vec::new() };
        finder.visit_expr(body.value);
        if finder.spans.is_empty() {
            return;
        }
        let owner = self.key(def.to_def_id());
        let root = tcx.typeck_root_def_id(def.to_def_id()).expect_local();
        for span in finder.spans {
            let place = self.place(span);
            self.bump("unsafe_blocks");
            let record = Record::new(&mut self.buf, "site")
                .str("kind", "unsafe")
                .str("owner", &owner);
            Self::put_place(record, &place).end();
            self.unsafe_blocks
                .entry(root)
                .or_default()
                .push(span.source_callsite());
        }
    }

    fn in_unsafe(&self, root: LocalDefId, span: Span) -> bool {
        let site = span.source_callsite();
        self.unsafe_blocks
            .get(&root)
            .is_some_and(|blocks| blocks.iter().any(|block| block.contains(site)))
    }

    fn body(&mut self, def: LocalDefId, body: &mir::Body<'tcx>) {
        let tcx = self.tcx;
        let root = tcx.typeck_root_def_id(def.to_def_id());
        let caller = Caller {
            body,
            key: self.key(def.to_def_id()),
            root_key: (root != def.to_def_id()).then(|| self.key(root)),
            root: root.expect_local(),
            typing_env: TypingEnv::post_analysis(tcx, def.to_def_id()),
        };
        for block in body.basic_blocks.iter() {
            let Some(term) = &block.terminator else {
                continue;
            };
            // Terminators copied in by the inliner belong to the callee;
            // `inlined_calls` records the inlined call itself.
            if is_inlined(body, term.source_info.scope) {
                continue;
            }
            match &term.kind {
                TerminatorKind::Call {
                    func,
                    args,
                    call_source,
                    fn_span,
                    ..
                } => {
                    let site = if fn_span.is_dummy() {
                        term.source_info.span
                    } else {
                        *fn_span
                    };
                    self.call(
                        &caller,
                        func,
                        args,
                        *call_source,
                        site,
                        term.source_info.span,
                    );
                }
                TerminatorKind::Drop { place, .. } => {
                    self.drop_edge(&caller, place.ty(body, tcx).ty, term.source_info.span);
                }
                _ => {}
            }
        }
        self.inlined_calls(&caller);
        self.refs(&caller);
    }

    fn caller_record<'a>(
        &self,
        out: &'a mut String,
        tag: &str,
        caller: &Caller<'_, 'tcx>,
    ) -> Record<'a> {
        Record::new(out, tag)
            .str("caller", &caller.key)
            .opt("caller_root", caller.root_key.as_deref())
    }

    /// Edge from a `Drop` terminator to the user `Drop` impl it runs.
    fn drop_edge(&mut self, caller: &Caller<'_, 'tcx>, ty: Ty<'tcx>, span: Span) {
        let tcx = self.tcx;
        let Some(adt) = ty.ty_adt_def() else {
            return;
        };
        let Some(dtor) = adt.destructor(tcx) else {
            return;
        };
        self.bump("drop_edges");
        let place = self.place(span);
        let mut line = String::new();
        let mut record = self.caller_record(&mut line, "call", caller);
        record = Self::put_place(record, &place)
            .str("kind", "drop")
            .str("callee", &self.key(dtor.did))
            .str("callee_name", "drop")
            .str("ty", &self.type_short(ty))
            .flag("in_unsafe", self.in_unsafe(caller.root, span));
        record = self.def_place(record, dtor.did);
        record.end();
        self.buf.push_str(&line);
    }

    /// Calls that the MIR inliner replaced by the callee's body, recovered
    /// from the outermost inlined source scopes of optimised MIR.
    fn inlined_calls(&mut self, caller: &Caller<'_, 'tcx>) {
        for data in caller.body.source_scopes.iter() {
            let Some((instance, span)) = data.inlined else {
                continue;
            };
            // Nested inlining happened inside another inlined body.
            if data.inlined_parent_scope.is_some() {
                continue;
            }
            let InstanceKind::Item(target) = instance.def else {
                continue;
            };
            self.bump("call_edges");
            self.bump("inlined_calls");
            let place = self.place(span);
            let mut line = String::new();
            let mut record = self.caller_record(&mut line, "call", caller);
            record = Self::put_place(record, &place)
                .str("kind", "static")
                .str("callee_name", self.tcx.item_name(target).as_str())
                .str("callee", &self.key(target));
            record = self.def_place(record, target);
            if let Some(owner) = self.impl_owner(target) {
                record = record.str("callee_owner", &owner);
            }
            record
                .flag("inlined", true)
                .flag("in_unsafe", self.in_unsafe(caller.root, span))
                .end();
            self.buf.push_str(&line);
        }
    }

    /// Functions used as values (`register(handler)`, `f(x, callback)`,
    /// `as fn(..)`), not called: the candidates for fn-pointer and generic
    /// `Fn*` calls.
    fn refs(&mut self, caller: &Caller<'_, 'tcx>) {
        let mut finder = RefFinder {
            body: caller.body,
            refs: Vec::new(),
        };
        for (bb, data) in caller.body.basic_blocks.iter_enumerated() {
            finder.visit_basic_block_data(bb, data);
        }
        let mut seen = HashSet::new();
        for (def_id, span) in finder.refs {
            let place = self.place(span);
            if !seen.insert((def_id, place.line)) {
                continue;
            }
            self.bump("fn_refs");
            let mut line = String::new();
            let mut record = self.caller_record(&mut line, "ref", caller);
            record = Self::put_place(record, &place).str("target", &self.key(def_id));
            record = self.def_place(record, def_id);
            record.end();
            self.buf.push_str(&line);
        }
    }

    /// Classifies the callee of a call terminator.
    fn callee(&self, typing_env: TypingEnv<'tcx>, func_ty: Ty<'tcx>) -> Callee {
        let tcx = self.tcx;
        let (def_id, fn_args) = match func_ty.kind() {
            ty::FnDef(def_id, fn_args) => (*def_id, *fn_args),
            ty::FnPtr(..) => return Callee::new("fnptr", None),
            ty::Closure(closure, _) => return Callee::new("closure", Some(*closure)),
            _ => return Callee::new("other", None),
        };
        let mut callee = Callee::new("static", Some(def_id));
        callee.name = Some(tcx.item_name(def_id).to_string());
        callee.generic = fn_args.iter().any(|a| a.as_region().is_none());
        if tcx.intrinsic(def_id).is_some() {
            callee.kind = "intrinsic";
            return callee;
        }
        if tcx.is_foreign_item(def_id) {
            callee.kind = "foreign";
            return callee;
        }
        let Some(trait_id) = tcx.trait_of_assoc(def_id) else {
            if tcx.is_closure_like(def_id) {
                callee.kind = "closure";
            }
            return callee;
        };
        callee.trait_id = Some(trait_id);
        callee.kind = match Instance::try_resolve(tcx, typing_env, def_id, fn_args) {
            Ok(Some(instance)) => match instance.def {
                InstanceKind::Virtual(..) => "dyn",
                InstanceKind::Item(target) if target == def_id => "trait_default",
                InstanceKind::Item(target) => {
                    callee.resolved = Some(target);
                    if tcx.is_closure_like(target) {
                        "closure"
                    } else {
                        "trait_static"
                    }
                }
                InstanceKind::Intrinsic(_) => "intrinsic",
                InstanceKind::ClosureOnceShim { .. } => {
                    if let ty::Closure(closure, _) = instance.args.type_at(0).kind() {
                        callee.resolved = Some(*closure);
                    }
                    "closure"
                }
                InstanceKind::FnPtrShim(..) => "fnptr",
                _ => "shim",
            },
            Ok(None) if fn_args.has_param() => "trait_generic",
            Ok(None) => "trait_unresolved",
            Err(_) => "error",
        };
        callee
    }

    fn call(
        &mut self,
        caller: &Caller<'_, 'tcx>,
        func: &Operand<'tcx>,
        args: &[Spanned<Operand<'tcx>>],
        source: mir::CallSource,
        site: Span,
        whole: Span,
    ) {
        let tcx = self.tcx;
        let callee = self.callee(caller.typing_env, func.ty(caller.body, tcx));
        let callee_key = callee.def.map(|def| self.key(def));
        if let (Some(def), Some(key)) = (callee.def, &callee_key)
            && tcx.crate_name(def.krate) == sym::core
            && key.starts_with(FMT_RT_PREFIX)
        {
            self.bump("fmt_rt_calls_skipped");
            return;
        }
        // Prefer the span of the whole call for macro attribution: the
        // callee name may be an argument token while the call itself is
        // macro output.
        let mut place = self.place(site);
        let whole_place = self.place(whole);
        if place.mac.is_none() {
            place.mac = whole_place.mac;
        }
        if place.desugar.is_none() {
            place.desugar = whole_place.desugar;
        }
        match callee.kind {
            "dyn" => self.bump("dyn_calls"),
            "fnptr" => self.bump("fnptr_calls"),
            "trait_generic" | "trait_unresolved" => self.bump("trait_generic_calls"),
            _ => {}
        }
        self.bump("call_edges");
        let source = match source {
            mir::CallSource::Normal => None,
            other => Some(format!("{other:?}")),
        };
        let mut line = String::new();
        let mut record = self.caller_record(&mut line, "call", caller);
        record = Self::put_place(record, &place)
            .str("kind", callee.kind)
            .opt("callee_name", callee.name.as_deref());
        if let (Some(def), Some(key)) = (callee.def, &callee_key) {
            record = record.str("callee", key);
            record = self.def_place(record, callee.resolved.unwrap_or(def));
            if callee.kind == "foreign" {
                let symbol = tcx
                    .codegen_fn_attrs(def)
                    .symbol_name
                    .unwrap_or_else(|| tcx.item_name(def));
                record = record.str("symbol", symbol.as_str());
            }
            if let Some(owner) = self.impl_owner(def) {
                record = record.str("callee_owner", &owner);
            }
        }
        if let Some(resolved) = callee.resolved {
            record = record.str("resolved", &self.key(resolved));
            if let Some(owner) = self.impl_owner(resolved) {
                record = record.str("resolved_owner", &owner);
            }
        }
        if let Some(trait_id) = callee.trait_id {
            record = record.str("trait", &self.key(trait_id));
        }
        // Only a method call has a receiver: the `self` of a `dyn` call, or
        // the fn pointer an `Fn*` trait call goes through (`FnPtrShim`, which
        // has a callee). A call through a plain fn pointer (`f(x)`, no
        // callee) passes only the pointee's parameters.
        if (callee.kind == "dyn" || (callee.kind == "fnptr" && callee.def.is_some()))
            && let Some(first) = args.first()
        {
            let ty = first.node.ty(caller.body, tcx);
            record = record.str("recv", &self.type_short(ty.peel_refs()));
        }
        record = record
            .flag("generic", callee.generic)
            .opt("source", source.as_deref())
            .flag("in_unsafe", self.in_unsafe(caller.root, site));
        if let (Some(def), Some(key)) = (callee.def, &callee_key)
            && tcx.crate_name(def.krate) == sym::core
            && key.starts_with("core::sync::atomic::")
        {
            record = record.raw("orderings", &self.orderings(caller, args));
            self.bump("atomic_calls");
        }
        record.end();
        self.buf.push_str(&line);
    }

    /// The `Ordering` arguments of an atomic call: variant names for
    /// constants, `"dynamic"` otherwise.
    fn orderings(&self, caller: &Caller<'_, 'tcx>, args: &[Spanned<Operand<'tcx>>]) -> String {
        let tcx = self.tcx;
        let mut names = Vec::new();
        for arg in args {
            let ty = arg.node.ty(caller.body, tcx);
            let ty::Adt(adt, _) = ty.kind() else {
                continue;
            };
            if tcx.item_name(adt.did()).as_str() != "Ordering"
                || tcx.crate_name(adt.did().krate) != sym::core
                || !self.key(adt.did()).starts_with("core::sync::atomic")
            {
                continue;
            }
            let name = match &arg.node {
                Operand::Constant(c) => c
                    .const_
                    .try_eval_scalar_int(tcx, caller.typing_env)
                    .and_then(|s| u32::try_from(s.to_bits_unchecked()).ok())
                    .and_then(|d| {
                        adt.variants()
                            .iter_enumerated()
                            .find(|(i, _)| i.as_u32() == d)
                            .map(|(_, v)| v.name.to_string())
                    })
                    .unwrap_or_else(|| "const".into()),
                Operand::Copy(place) | Operand::Move(place) => place
                    .as_local()
                    .and_then(|local| unit_variant_assigned(caller.body, local, adt.did()))
                    .map(|v| adt.variant(v).name.to_string())
                    .unwrap_or_else(|| "dynamic".into()),
                Operand::RuntimeChecks(_) => "dynamic".into(),
            };
            names.push(name);
        }
        json::str_array(names.iter().map(String::as_str))
    }
}

/// The unit variant of `adt` that `local` is assigned exactly once, as
/// built MIR does for a path such as `Ordering::Acquire` used as an argument.
fn unit_variant_assigned(
    body: &mir::Body<'_>,
    local: mir::Local,
    adt: DefId,
) -> Option<rustc_abi::VariantIdx> {
    let mut found = None;
    for block in body.basic_blocks.iter() {
        for stmt in &block.statements {
            let mir::StatementKind::Assign(assign) = &stmt.kind else {
                continue;
            };
            let (place, rvalue) = &**assign;
            if place.as_local() != Some(local) {
                continue;
            }
            match rvalue {
                mir::Rvalue::Aggregate(kind, ops) if ops.is_empty() => match **kind {
                    mir::AggregateKind::Adt(did, variant, ..) if did == adt && found.is_none() => {
                        found = Some(variant);
                    }
                    _ => return None,
                },
                _ => return None,
            }
        }
    }
    found
}

/// Collects `FnDef` constants outside the callee position of calls, except
/// in bodies the inliner copied in.
struct RefFinder<'b, 'tcx> {
    body: &'b mir::Body<'tcx>,
    refs: Vec<(DefId, Span)>,
}

impl<'tcx> MirVisitor<'tcx> for RefFinder<'_, 'tcx> {
    fn visit_terminator(&mut self, term: &mir::Terminator<'tcx>, location: mir::Location) {
        if let TerminatorKind::Call { args, .. } | TerminatorKind::TailCall { args, .. } =
            &term.kind
        {
            for arg in args {
                self.visit_operand(&arg.node, location);
            }
            return;
        }
        self.super_terminator(term, location);
    }

    fn visit_const_operand(&mut self, constant: &mir::ConstOperand<'tcx>, location: mir::Location) {
        if is_inlined(self.body, self.body.source_info(location).scope) {
            return;
        }
        if let ty::FnDef(def_id, _) = constant.const_.ty().kind() {
            self.refs.push((*def_id, constant.span));
        }
    }
}

/// Collects user-written `unsafe` blocks of one body (not of nested
/// closure bodies, which are body owners of their own).
struct UnsafeFinder {
    spans: Vec<Span>,
}

impl<'tcx> Visitor<'tcx> for UnsafeFinder {
    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        if let ExprKind::Block(block, _) = expr.kind
            && block.rules == BlockCheckMode::UnsafeBlock(UnsafeSource::UserProvided)
        {
            self.spans.push(block.span);
        }
        intravisit::walk_expr(self, expr);
    }
}
