// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `use` records: statics, consts, enum variants, fields and struct
//! constructions in a body, from its MIR.

use std::collections::{HashMap, HashSet};

use rustc_abi::FIRST_VARIANT;
use rustc_hir::{def::DefKind, def_id::DefId};
use rustc_middle::{
    mir::{
        self, AggregateKind, Local, Operand, Place, PlaceTy, ProjectionElem, Rvalue,
        TerminatorKind,
        visit::{PlaceContext, Visitor as MirVisitor},
    },
    ty::{self, Ty, TyCtxt},
};
use rustc_span::Span;

use super::{
    Exporter,
    bodies::{Caller, is_inlined},
};

/// Crates whose items are not recorded as uses: every body uses `Option`,
/// `Result` and their internals, which would drown the kernel's own uses.
const STD_CRATES: &[&str] = &["core", "alloc", "std", "compiler_builtins"];

/// One use of a definition.
struct Use {
    /// `static`, `const`, `variant`, `field` or `struct`.
    kind: &'static str,
    target: DefId,
    /// `read`, `mut` (write or `&mut`), `construct`, `match` or `ref`
    /// (address of a static).
    access: &'static str,
    span: Span,
}

struct UseFinder<'b, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'b mir::Body<'tcx>,
    uses: Vec<Use>,
    /// Locals holding the discriminant of an enum place, with its type.
    discriminants: HashMap<Local, Ty<'tcx>>,
}

impl<'tcx> UseFinder<'_, 'tcx> {
    fn push(
        &mut self,
        kind: &'static str,
        target: DefId,
        access: &'static str,
        location: mir::Location,
    ) {
        let span = self.body.source_info(location).span;
        self.uses.push(Use {
            kind,
            target,
            access,
            span,
        });
    }

    fn skipped(&self, location: mir::Location) -> bool {
        is_inlined(self.body, self.body.source_info(location).scope)
    }
}

impl<'tcx> MirVisitor<'tcx> for UseFinder<'_, 'tcx> {
    fn visit_const_operand(&mut self, constant: &mir::ConstOperand<'tcx>, location: mir::Location) {
        if self.skipped(location) {
            return;
        }
        if let Some(def) = constant.check_static_ptr(self.tcx) {
            self.push("static", def, "ref", location);
            return;
        }
        if let mir::Const::Unevaluated(unevaluated, _) = constant.const_
            && unevaluated.promoted.is_none()
            && matches!(
                self.tcx.def_kind(unevaluated.def),
                DefKind::Const { .. } | DefKind::AssocConst { .. }
            )
        {
            self.push("const", unevaluated.def, "read", location);
        }
    }

    fn visit_place(&mut self, place: &Place<'tcx>, context: PlaceContext, location: mir::Location) {
        if matches!(context, PlaceContext::NonUse(_)) || self.skipped(location) {
            return;
        }
        let access = if context.is_mutating_use() {
            "mut"
        } else {
            "read"
        };
        let mut place_ty = PlaceTy::from_ty(self.body.local_decls[place.local].ty);
        for elem in place.projection {
            if let ty::Adt(adt, _) = place_ty.ty.kind() {
                match elem {
                    ProjectionElem::Field(index, _) => {
                        let variant = place_ty.variant_index.unwrap_or(FIRST_VARIANT);
                        let field = adt.variant(variant).fields[index].did;
                        self.push("field", field, access, location);
                    }
                    ProjectionElem::Downcast(_, variant) if adt.is_enum() => {
                        self.push("variant", adt.variant(variant).def_id, "read", location);
                    }
                    _ => {}
                }
            }
            place_ty = place_ty.projection_ty(self.tcx, elem);
        }
    }

    fn visit_assign(
        &mut self,
        place: &Place<'tcx>,
        rvalue: &Rvalue<'tcx>,
        location: mir::Location,
    ) {
        if let (Some(local), Rvalue::Discriminant(enum_place)) = (place.as_local(), rvalue) {
            let ty = enum_place.ty(self.body, self.tcx).ty;
            self.discriminants.insert(local, ty);
        }
        self.super_assign(place, rvalue, location);
    }

    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: mir::Location) {
        if let Rvalue::Aggregate(kind, _) = rvalue
            && let AggregateKind::Adt(did, variant, ..) = **kind
            && !self.skipped(location)
        {
            let adt = self.tcx.adt_def(did);
            if adt.is_enum() {
                self.push(
                    "variant",
                    adt.variant(variant).def_id,
                    "construct",
                    location,
                );
            } else {
                self.push("struct", did, "construct", location);
            }
        }
        self.super_rvalue(rvalue, location);
    }

    fn visit_terminator(&mut self, term: &mir::Terminator<'tcx>, location: mir::Location) {
        if let TerminatorKind::SwitchInt { discr, targets } = &term.kind
            && let Operand::Copy(place) | Operand::Move(place) = discr
            && let Some(local) = place.as_local()
            && let Some(ty) = self.discriminants.get(&local).copied()
            && let ty::Adt(adt, _) = ty.kind()
            && !self.skipped(location)
        {
            for (value, _) in targets.iter() {
                if let Some((variant, _)) = adt
                    .discriminants(self.tcx)
                    .find(|(_, discr)| discr.val == value)
                {
                    self.push("variant", adt.variant(variant).def_id, "match", location);
                }
            }
        }
        self.super_terminator(term, location);
    }
}

impl<'tcx> Exporter<'tcx> {
    /// Writes the `use` records of one body, one per (kind, target, access,
    /// line).
    pub(super) fn uses(&mut self, caller: &Caller<'_, 'tcx>) {
        let tcx = self.tcx;
        let mut finder = UseFinder {
            tcx,
            body: caller.body,
            uses: Vec::new(),
            discriminants: HashMap::new(),
        };
        finder.visit_body(caller.body);
        let mut seen = HashSet::new();
        for found in finder.uses {
            if STD_CRATES.contains(&tcx.crate_name(found.target.krate).as_str()) {
                continue;
            }
            let place = self.place(found.span);
            if !seen.insert((found.kind, found.target, found.access, place.line)) {
                continue;
            }
            self.bump(match found.kind {
                "static" => "use_static",
                "const" => "use_const",
                "variant" => "use_variant",
                "field" => "use_field",
                _ => "use_struct",
            });
            let mut line = String::new();
            let mut record = self.caller_record(&mut line, "use", caller);
            record = Self::put_place(record, &place)
                .str("kind", found.kind)
                .str("target", &self.key(found.target))
                .str("access", found.access);
            record = self.def_place_as(record, found.target, "target_file", "target_line");
            record.end();
            self.buf.push_str(&line);
        }
    }
}
